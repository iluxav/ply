#!/bin/sh
# publish-release.sh — pull a GitHub release's ply image(s) and push them to
# the official registry. The recurring move after any app release.
#
#   scripts/publish-release.sh iluxav/ply-dashboard dashboard          # latest
#   scripts/publish-release.sh iluxav/ply-dashboard dashboard 0.1.26  # exact
#   NAMESPACE=ply scripts/publish-release.sh <repo> <app> [ver]       # keg lane
#
# Downloads every arch the release carries (x64 + arm64) and pushes what it
# found. Needs `wrangler login` once, like registry-push.mjs.
set -eu
cd "$(dirname "$0")/.."

[ $# -ge 2 ] || { echo "usage: $0 <org/repo> <app> [version]" >&2; exit 2; }
REPO=$1
APP=$2
VER=${3:-}

if [ -z "$VER" ]; then
    VER=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
        sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p')
    [ -n "$VER" ] || { echo "error: cannot resolve the latest release of $REPO" >&2; exit 1; }
    echo "latest release: v$VER"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

set --
for arch in x64 arm64; do
    img="$APP-$VER-linux-$arch.img"
    if curl -fsSL -o "$tmp/$img" "https://github.com/$REPO/releases/download/v$VER/$img"; then
        echo "downloaded $img"
        set -- "$@" --file "$tmp/$img"
    else
        echo "skip $arch — release has no $img"
    fi
done
[ $# -gt 0 ] || { echo "error: release v$VER carries no ply images for $APP" >&2; exit 1; }

node scripts/registry-push.mjs "$@" --namespace "${NAMESPACE:-apps}" \
    --bucket ply-registry-deb --state scripts/registry-deb-state.json
