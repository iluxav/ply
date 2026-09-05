#!/bin/sh
# Build everything the bench needs: the API binary, the two ply images, the
# Docker image. No root needed. Re-run after changing bench/api.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
PLY=${PLY:-$HERE/../target/release/ply}
[ -x "$PLY" ] || { echo "error: $PLY not found — cargo build --release -p ply-cli" >&2; exit 1; }

echo "==> api binary"
(cd "$HERE/api" && CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' -o server .)
cp "$HERE/api/server" "$HERE/ply/api/server"
cp "$HERE/api/server" "$HERE/docker/server"

echo "==> ply images"
(cd "$HERE/ply/api" && "$PLY" build .)
(cd "$HERE/ply/pgdb" && "$PLY" build .)

echo "==> docker image"
docker build -q -t benchapi:local "$HERE/docker"
docker pull -q postgres:17

echo "==> done"
ls -la "$HERE"/ply/api/*.img "$HERE"/ply/pgdb/*.img
docker images --format '{{.Repository}}:{{.Tag}}  {{.Size}}' | grep -E '^(benchapi:local|postgres:17) '
