#!/bin/sh
# build-base-debian.sh — mint the debian (glibc) base package for one arch.
#
#   usage: scripts/build-base-debian.sh <arch: x64|arm64> [outdir]
#   e.g.:  scripts/build-base-debian.sh x64 ./out
#
# Downloads the debuerreotype rootfs for debian <suite> slim — the exact
# reproducible rootfs Docker's debian:<suite>-slim is built from — verifies
# its sha256 against the layer digest in the repo's own OCI image manifest,
# writes the base ply.toml (glibc ABI), and runs `ply build`. Emits
# debian-<version>-linux-<arch>.img into outdir. Cross-building works the
# same as the alpine script: a base is inert files, so any host builds any
# arch; the output is renamed when host != target.
set -eu

usage() { echo "usage: $0 <arch: x64|arm64> [outdir]" >&2; exit 2; }
[ $# -ge 1 ] || usage

ARCH=$1
OUTDIR=${2:-.}
SUITE=${SUITE:-trixie}
PLY=${PLY:-ply}

case "$ARCH" in
  x64)   GIT_ARCH=amd64 ;;
  arm64) GIT_ARCH=arm64v8 ;;
  *) usage ;;
esac
RAW=https://raw.githubusercontent.com/debuerreotype/docker-debian-artifacts/dist-${GIT_ARCH}/${SUITE}/slim

command -v "$PLY" >/dev/null || { echo "error: ply not on PATH (set PLY=/path/to/ply)" >&2; exit 1; }
mkdir -p "$OUTDIR"
OUTDIR=$(cd "$OUTDIR" && pwd)

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT INT TERM

echo "==> downloading $RAW/oci/blobs/rootfs.tar.gz"
curl -fsSL -o "$WORK/rootfs.tar.gz" "$RAW/oci/blobs/rootfs.tar.gz"
curl -fsSL -o "$WORK/image-manifest.json" "$RAW/oci/blobs/image-manifest.json"
VERSION=$(curl -fsSL "$RAW/rootfs.debian_version")

# Integrity: the tarball's sha256 must equal the layer digest the artifacts
# repo publishes in its own OCI manifest (the "layers" entry — the config
# digest comes first, the layer digest last).
WANT=$(grep -o 'sha256:[0-9a-f]\{64\}' "$WORK/image-manifest.json" | tail -1 | cut -d: -f2)
GOT=$(sha256sum "$WORK/rootfs.tar.gz" | cut -d' ' -f1)
[ -n "$WANT" ] && [ "$WANT" = "$GOT" ] \
  || { echo "error: sha256 mismatch for rootfs.tar.gz (want $WANT, got $GOT)" >&2; exit 1; }

# "13.6" → "13.6.0" (ply versions are semver)
case "$VERSION" in
  *.*.*) SEMVER=$VERSION ;;
  *.*)   SEMVER=$VERSION.0 ;;
  *)     SEMVER=$VERSION.0.0 ;;
esac

# Device nodes are excluded: the image writer refuses them, and ply mounts
# its own minimal /dev at run time anyway.
ROOTFS=$WORK/rootfs
mkdir "$ROOTFS"
tar -xzf "$WORK/rootfs.tar.gz" -C "$ROOTFS" --exclude='dev/*' --exclude='./dev/*'

cat > "$ROOTFS/ply.toml" <<EOF
[package]
name = "debian"
version = "$SEMVER"
base = true
provides_abi = "linux-${ARCH}-gnu"
EOF

case "$(uname -m)" in
  x86_64) HOST_ARCH=x64 ;;
  aarch64) HOST_ARCH=arm64 ;;
  *) echo "error: unsupported host arch $(uname -m)" >&2; exit 1 ;;
esac

echo "==> ply build ($ARCH, debian $VERSION)"
(cd "$ROOTFS" && "$PLY" build . )
IMG=debian-${SEMVER}-linux-${ARCH}.img
mv "$ROOTFS/debian-${SEMVER}-linux-${HOST_ARCH}.img" "$OUTDIR/$IMG"

echo "==> $OUTDIR/$IMG"
echo "    push it with the registry flow, e.g.: node scripts/registry-push.mjs $OUTDIR/$IMG"
