#!/bin/sh
# build-arm64.sh — mint + push the arm64 twin of every official package.
#
#   scripts/build-arm64.sh            # build into ./out, push after each phase
#   PUSH=0 scripts/build-arm64.sh     # build only (prints what it would push)
#
# Cross-builds from any host: kegs are inert files (deb2pkg unpacks debs,
# node ships prebuilt tarballs, the app wrappers are scripts) — only notify
# is a real cross-compile, which Go does natively. Phase order is load-
# bearing: the arm64 BASE must be live in the registry before anything
# depending on it builds, and the redis KEG must be pushed before the redis
# app wrapper build reuses the same out/ filename.
set -eu
cd "$(dirname "$0")/.."
OUT=${OUT:-out}
PLY=${PLY:-$PWD/target/release/ply}
PUSH=${PUSH:-1}
NODE_VERSIONS="22.23.2 24.18.1"

[ -x "$PLY" ] || { echo "error: $PLY not found — cargo build --release -p ply-cli" >&2; exit 1; }
mkdir -p "$OUT"

push() {
  ns=$1
  shift
  if [ "$PUSH" != 1 ]; then
    echo "skip push ($ns): $*"
    return 0
  fi
  set -- $(for f in "$@"; do printf -- "--file %s " "$f"; done)
  node scripts/registry-push.mjs "$@" --namespace "$ns" \
    --bucket ply-registry-deb --state scripts/registry-deb-state.json
}

echo "==> phase 1: debian base (arm64)"
PLY="$PLY" scripts/build-base-debian.sh arm64 "$OUT"
push ply "$OUT"/debian-*-linux-arm64.img

echo "==> phase 2: deb2pkg kegs (arm64)"
cargo run --release -p deb2pkg -- git --arch arm64 --outdir "$OUT"
cargo run --release -p deb2pkg -- caddy --arch arm64 --outdir "$OUT"
cargo run --release -p deb2pkg -- redis-server --name redis --arch arm64 --outdir "$OUT"
cargo run --release -p deb2pkg -- postgresql-17 --name postgresql17 --skip-so llvmjit.so --arch arm64 --outdir "$OUT"
cargo run --release -p deb2pkg -- postgresql-client-17 --name postgresql-client17 --arch arm64 --outdir "$OUT"
cargo run --release -p deb2pkg -- rclone --arch arm64 --outdir "$OUT"

echo "==> phase 3: node kegs (arm64, nodejs.org tarballs)"
for V in $NODE_VERSIONS; do
  stage=$(mktemp -d)
  trap 'rm -rf "$stage"' EXIT INT TERM
  curl -fsSL "https://nodejs.org/dist/v$V/node-v$V-linux-arm64.tar.xz" |
    tar xJ --strip-components=1 -C "$stage"
  cat > "$stage/ply.toml" <<EOF
[package]
name = "node"
version = "$V"
provides_abi = "linux-arm64-gnu"

[layer]
path = ["/opt/node-$V/bin"]

[dependencies]
debian = "13"
EOF
  "$PLY" build "$stage" -o "$OUT/node-$V-linux-arm64.img" --arch arm64
  rm -rf "$stage"
done

# kegs go live before the wrappers below resolve against them
push ply "$OUT"/git-*-linux-arm64.img "$OUT"/caddy-*-linux-arm64.img \
  "$OUT"/redis-*-linux-arm64.img "$OUT"/postgresql17-*-linux-arm64.img \
  "$OUT"/postgresql-client17-*-linux-arm64.img "$OUT"/rclone-*-linux-arm64.img \
  "$OUT"/node-*-linux-arm64.img

echo "==> phase 4: app wrappers (arm64)"
# NOTE: the redis wrapper reuses out/redis-<v>-linux-arm64.img — the keg of
# the same name was already pushed above, so the overwrite is safe.
for d in postgres redis pg-backup; do
  v=$(sed -n 's/^version = "\(.*\)"/\1/p' "services/$d/ply.toml" | head -1)
  "$PLY" build "services/$d" -o "$OUT/$d-$v-linux-arm64.img" --arch arm64
done

nv=$(sed -n 's/^version = "\(.*\)"/\1/p' services/notify/ply.toml | head -1)
(cd services/notify && CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -o notify .)
"$PLY" build services/notify -o "$OUT/notify-$nv-linux-arm64.img" --arch arm64
(cd services/notify && CGO_ENABLED=0 go build -o notify .) # restore host-arch binary

pgv=$(sed -n 's/^version = "\(.*\)"/\1/p' services/postgres/ply.toml | head -1)
pbv=$(sed -n 's/^version = "\(.*\)"/\1/p' services/pg-backup/ply.toml | head -1)
rv=$(sed -n 's/^version = "\(.*\)"/\1/p' services/redis/ply.toml | head -1)
push apps "$OUT/postgres-$pgv-linux-arm64.img" "$OUT/redis-$rv-linux-arm64.img" \
  "$OUT/pg-backup-$pbv-linux-arm64.img" "$OUT/notify-$nv-linux-arm64.img"

echo "==> done. gate:"
echo "    curl -s https://registry.plybox.sh/ply/state.json | grep -c arm64"
echo "    (dashboard arm64 comes from its own CI — rerun the ply-dashboard release)"
