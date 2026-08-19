#!/bin/sh
# Build real packages (alpine base + node runtime) into ./registry/,
# simulating the origin that images are installed from.
set -eu

cd "$(dirname "$0")"
PLY="${PLY:-ply}"
command -v "$PLY" >/dev/null || { echo "ply not found — run 'make install' first (or PLY=/path/to/ply)"; exit 1; }

ALPINE_VERSION=3.20.7
ALPINE_MINOR=v3.20
NODE_VERSION=24.6.0

WORK=.work
CACHE=$WORK/cache
REGISTRY=registry
mkdir -p "$CACHE" "$REGISTRY"

fetch() { # url dest
    [ -f "$2" ] || { echo "fetching $1"; curl -fL --progress-bar -o "$2.part" "$1"; mv "$2.part" "$2"; }
}

# ---- alpine (base package: owns /, FHS, /bin/sh, musl libc) ----
ALPINE_TAR=$CACHE/alpine-minirootfs-$ALPINE_VERSION-x86_64.tar.gz
fetch "https://dl-cdn.alpinelinux.org/alpine/$ALPINE_MINOR/releases/x86_64/alpine-minirootfs-$ALPINE_VERSION-x86_64.tar.gz" "$ALPINE_TAR"

rm -rf "$WORK/alpine"; mkdir -p "$WORK/alpine"
tar -xzf "$ALPINE_TAR" -C "$WORK/alpine"
cat > "$WORK/alpine/ply.toml" <<EOF
[package]
name = "alpine"
version = "$ALPINE_VERSION"
base = true
EOF
"$PLY" build "$WORK/alpine" -o "$REGISTRY/alpine-$ALPINE_VERSION-linux-x64.img"

# ---- node (runtime package: keg at /opt/node-<version>, musl build) ----
NODE_TAR=$CACHE/node-v$NODE_VERSION-linux-x64-musl.tar.xz
fetch "https://unofficial-builds.nodejs.org/download/release/v$NODE_VERSION/node-v$NODE_VERSION-linux-x64-musl.tar.xz" "$NODE_TAR"

rm -rf "$WORK/node"; mkdir -p "$WORK/node"
tar -xJf "$NODE_TAR" -C "$WORK/node" --strip-components=1

# node's musl build links libstdc++/libgcc, which the minirootfs base lacks —
# vendor them into the keg from alpine's own apks (an apk is just a tar.gz).
APK_REPO="https://dl-cdn.alpinelinux.org/alpine/$ALPINE_MINOR/main/x86_64"
fetch "$APK_REPO/APKINDEX.tar.gz" "$CACHE/APKINDEX.tar.gz"
apk_version() { # package-name → version from APKINDEX
    tar -xzOf "$CACHE/APKINDEX.tar.gz" APKINDEX 2>/dev/null | \
        awk -v pkg="$1" '$0=="P:"pkg {found=1} found && /^V:/ {print substr($0,3); exit}'
}
for lib in libstdc++ libgcc; do
    ver=$(apk_version "$lib")
    [ -n "$ver" ] || { echo "cannot find $lib in APKINDEX"; exit 1; }
    fetch "$APK_REPO/$lib-$ver.apk" "$CACHE/$lib-$ver.apk"
    tar -xzf "$CACHE/$lib-$ver.apk" -C "$WORK/node" --warning=no-unknown-keyword usr/lib 2>/dev/null || true
done
mkdir -p "$WORK/node/lib"
mv "$WORK/node/usr/lib/"*.so* "$WORK/node/lib/"
rm -rf "$WORK/node/usr"

cat > "$WORK/node/ply.toml" <<EOF
[package]
name = "node"
version = "$NODE_VERSION"
provides_abi = "linux-x64-musl"

[layer]
path = ["/opt/node-$NODE_VERSION/bin"]
ld_library_path = ["/opt/node-$NODE_VERSION/lib"]

[dependencies]
alpine = "3.20"
EOF
"$PLY" build "$WORK/node" -o "$REGISTRY/node-$NODE_VERSION-linux-x64.img"

# ---- index.json (version listing for dumb http hosts) ----
(cd "$REGISTRY" && ls *.img | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin]))') > "$REGISTRY/index.json"

echo
echo "registry ready:"
ls -lh "$REGISTRY"
echo
echo "serve it with: ./serve.sh"
