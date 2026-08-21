#!/bin/sh
# ply installer:  curl -fsSL https://plybox.sh/install.sh | sh
#
# As root            → /usr/local/bin/ply + automatic host setup.
# As user with sudo  → same, via sudo (one system-wide copy; a stale
#                      ~/.local/bin/ply shadow copy is removed).
# As user, no sudo   → ~/.local/bin/ply, prints the one-time
#                      `sudo ply setup` hint only if this host needs it.
#
# Overrides: PLY_VERSION (default: latest), PLY_REPO, PLY_BINARY (install a
# local file instead of downloading — used by CI and development).
set -eu

PLY_REPO="${PLY_REPO:-iluxav/ply}"
PLY_VERSION="${PLY_VERSION:-latest}"

arch=$(uname -m)
case "$arch" in
    x86_64)  target="x64" ;;
    aarch64) target="arm64" ;;
    *) echo "error: unsupported architecture: $arch (ply supports x86_64 and aarch64)"; exit 1 ;;
esac
[ "$(uname -s)" = "Linux" ] || { echo "error: ply is Linux-only"; exit 1; }

# --- fetch (or take a local binary) -----------------------------------------
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [ -n "${PLY_BINARY:-}" ]; then
    cp "$PLY_BINARY" "$tmp/ply"
else
    if [ "$PLY_VERSION" = "latest" ]; then
        url="https://github.com/$PLY_REPO/releases/latest/download/ply-linux-$target"
    else
        url="https://github.com/$PLY_REPO/releases/download/$PLY_VERSION/ply-linux-$target"
    fi
    echo "downloading $url"
    curl -fsSL -o "$tmp/ply" "$url"
fi
chmod 755 "$tmp/ply"

# --- install ----------------------------------------------------------------
# One copy, system-wide, whenever possible: two ply binaries on one host is
# how an AppArmor profile ends up naming one while PATH runs the other.
# ~/.local/bin is the no-privileges fallback only.
if [ "$(id -u)" = "0" ]; then
    install -m 755 "$tmp/ply" /usr/local/bin/ply
    echo "installed /usr/local/bin/ply ($(/usr/local/bin/ply --version))"
    /usr/local/bin/ply setup
elif command -v sudo >/dev/null 2>&1 && { sudo -n true 2>/dev/null || [ -e /dev/tty ]; }; then
    echo "installing to /usr/local/bin (sudo may prompt for your password)"
    sudo install -m 755 "$tmp/ply" /usr/local/bin/ply
    if [ -f "$HOME/.local/bin/ply" ]; then
        rm -f "$HOME/.local/bin/ply"
        echo "removed $HOME/.local/bin/ply (a shadow copy from an earlier user-mode install)"
    fi
    echo "installed /usr/local/bin/ply ($(/usr/local/bin/ply --version))"
    sudo /usr/local/bin/ply setup
else
    mkdir -p "$HOME/.local/bin"
    install -m 755 "$tmp/ply" "$HOME/.local/bin/ply"
    echo "installed $HOME/.local/bin/ply ($("$HOME/.local/bin/ply" --version))"
    case ":$PATH:" in
        *":$HOME/.local/bin:"*) ;;
        *) echo "note: add ~/.local/bin to your PATH" ;;
    esac
    # rootless containers need one-time host prep only on restricted kernels
    if [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null)" = "1" ] \
        && [ ! -f /etc/apparmor.d/ply ]; then
        echo ""
        echo "! rootless containers need one-time host setup:  sudo ply setup"
    fi
fi
