#!/bin/sh
# build-base.sh — mint the alpine base package for one architecture.
#
#   usage: scripts/build-base.sh <alpine-version> <arch> [outdir]
#   e.g.:  scripts/build-base.sh 3.20.7 arm64
#          scripts/build-base.sh 3.20.7 x64 ./out
#
# Downloads Alpine's minirootfs tarball (sha512-verified against the
# mirror's published checksum), writes the base ply.toml, and runs
# `ply build` — emitting alpine-<version>-linux-<arch>.img into outdir.
# Run it once per Alpine release per arch; push the .img with the normal
# registry flow. Runs on any Linux host for any target arch: a base is
# inert files (no resolution, no execution), so cross-building is just
# downloading the other tarball. `ply build` names images by host arch —
# the one thing it can't know — so when host != target the script renames
# the output; the filename's arch claim is made true by the rootfs inside.
set -eu

usage() { echo "usage: $0 <alpine-version> <arch: x64|arm64> [outdir]" >&2; exit 2; }
[ $# -ge 2 ] || usage

VERSION=$1
ARCH=$2
OUTDIR=${3:-.}
PLY=${PLY:-ply}

case "$ARCH" in
  x64)   ALPINE_ARCH=x86_64 ;;
  arm64) ALPINE_ARCH=aarch64 ;;
  *) usage ;;
esac
BRANCH=v$(echo "$VERSION" | cut -d. -f1-2)
TARBALL=alpine-minirootfs-${VERSION}-${ALPINE_ARCH}.tar.gz
URL=https://dl-cdn.alpinelinux.org/alpine/${BRANCH}/releases/${ALPINE_ARCH}/${TARBALL}

command -v "$PLY" >/dev/null || { echo "error: ply not on PATH (set PLY=/path/to/ply)" >&2; exit 1; }
mkdir -p "$OUTDIR"
OUTDIR=$(cd "$OUTDIR" && pwd)

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT INT TERM

echo "==> downloading $URL"
curl -fsSL -o "$WORK/$TARBALL" "$URL"
curl -fsSL -o "$WORK/$TARBALL.sha512" "$URL.sha512"
(cd "$WORK" && sha512sum -c "$TARBALL.sha512" >/dev/null) \
  || { echo "error: sha512 mismatch for $TARBALL" >&2; exit 1; }

# Device nodes are excluded: the image writer refuses them, and ply mounts
# its own minimal /dev at run time anyway. Everything else packs as-is;
# `ply build` normalizes ownership and timestamps (deterministic squashfs).
ROOTFS=$WORK/rootfs
mkdir "$ROOTFS"
tar -xzf "$WORK/$TARBALL" -C "$ROOTFS" --exclude='dev/*'

cat > "$ROOTFS/ply.toml" <<EOF
[package]
name = "alpine"
version = "$VERSION"
base = true
provides_abi = "linux-${ARCH}-musl"
EOF

case "$(uname -m)" in
  x86_64) HOST_ARCH=x64 ;;
  aarch64) HOST_ARCH=arm64 ;;
  *) echo "error: unsupported host arch $(uname -m)" >&2; exit 1 ;;
esac

echo "==> ply build ($ARCH)"
(cd "$ROOTFS" && "$PLY" build . )
IMG=alpine-${VERSION}-linux-${ARCH}.img
mv "$ROOTFS/alpine-${VERSION}-linux-${HOST_ARCH}.img" "$OUTDIR/$IMG"

echo "==> $OUTDIR/$IMG"
echo "    push it with the registry flow, e.g.: node scripts/registry-push.mjs $OUTDIR/$IMG"
