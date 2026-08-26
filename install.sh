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

# --- optional guided setup (edge + dashboard) --------------------------------
# Interactive only when a human can answer: prompts read /dev/tty, never
# stdin (stdin is this script when piped through sh). CI and automation stay
# prompt-free: no /dev/tty, or PLY_WIZARD=no, skips everything; env vars
# answer the questions non-interactively:
#   PLY_EDGE=yes|no   PLY_DASHBOARD=yes|no   PLY_DOMAIN=dash.example.com
PLY_BIN=/usr/local/bin/ply
[ -x "$PLY_BIN" ] || PLY_BIN="$HOME/.local/bin/ply"

is_root_like() { [ "$(id -u)" = "0" ] || { command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; }; }
as_root() { if [ "$(id -u)" = "0" ]; then "$@"; else sudo "$@"; fi; }

ask() { # ask "question" default -> echoes yes|no
    q=$1; default=$2
    if [ -n "${3:-}" ]; then echo "$3"; return; fi        # env override
    if [ ! -e /dev/tty ] || [ "${PLY_WIZARD:-yes}" = "no" ]; then echo no; return; fi
    if [ "$default" = "yes" ]; then hint="[Y/n]"; else hint="[y/N]"; fi
    printf "%s %s " "$q" "$hint" > /dev/tty
    IFS= read -r answer < /dev/tty || answer=""
    case "$answer" in
        [Yy]*) echo yes ;;
        [Nn]*) echo no ;;
        *) echo "$default" ;;
    esac
}

ask_text() { # ask_text "question" -> echoes answer (may be empty)
    if [ -n "${2:-}" ]; then echo "$2"; return; fi
    if [ ! -e /dev/tty ] || [ "${PLY_WIZARD:-yes}" = "no" ]; then echo ""; return; fi
    printf "%s " "$1" > /dev/tty
    IFS= read -r answer < /dev/tty || answer=""
    echo "$answer"
}

# The wizard needs root (systemd units, Caddy, ports 80/443) — skip quietly
# for user-mode installs and CI.
if is_root_like && { [ -e /dev/tty ] || [ -n "${PLY_EDGE:-}${PLY_DASHBOARD:-}" ]; } && [ "${PLY_WIZARD:-yes}" != "no" ]; then
    echo ""
    if [ "$(ask "enable the HTTPS edge? (Caddy + auto-TLS: apps get domains via --domain)" yes "${PLY_EDGE:-}")" = "yes" ]; then
        as_root "$PLY_BIN" setup --edge
        domain=$(ask_text "domain for the dashboard, pointed at this host (blank = skip):" "${PLY_DOMAIN:-}")
    else
        domain=""
    fi

    if [ "$(ask "install the ply dashboard? (web UI, runs as a ply app)" yes "${PLY_DASHBOARD:-}")" = "yes" ]; then
        dash_arch=$target
        dash_tag=$(curl -fsSL "https://api.github.com/repos/iluxav/ply-dashboard/releases/latest" \
            | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')
        if [ -z "$dash_tag" ]; then
            echo "! cannot reach the dashboard release — install later: https://github.com/iluxav/ply-dashboard"
        else
            dash_ver=${dash_tag#v}
            dash_img="dashboard-$dash_ver-linux-$dash_arch.img"
            as_root mkdir -p /srv/ply-dashboard
            echo "downloading $dash_img ($dash_tag)"
            curl -fsSL -o "$tmp/$dash_img" \
                "https://github.com/iluxav/ply-dashboard/releases/download/$dash_tag/$dash_img"
            as_root install -m 644 "$tmp/$dash_img" "/srv/ply-dashboard/$dash_img"
            # stable symlink: an update is download + re-link + restart
            as_root ln -sfn "/srv/ply-dashboard/$dash_img" /srv/ply-dashboard/current.img

            set -- --grant-links --publish internal:7070
            [ -n "$domain" ] && set -- "$@" --domain "$domain"
            as_root sh -c "'$PLY_BIN' systemd /srv/ply-dashboard/current.img $* \
                > /etc/systemd/system/ply-dashboard.service"
            as_root systemctl daemon-reload
            as_root systemctl enable --now ply-dashboard

            # Wait for it, then hand over the one secret that matters.
            echo "waiting for the dashboard to come up …"
            probe="http://10.77.0.1:7070/healthz"
            [ -n "$domain" ] && probe="https://$domain/healthz"
            tries=0
            until curl -fsk -m 3 "$probe" >/dev/null 2>&1 || [ $tries -ge 30 ]; do
                tries=$((tries + 1)); sleep 3
            done
            token=$(as_root journalctl -u ply-dashboard --no-pager 2>/dev/null \
                | sed -n 's/.*setup token: \([A-Za-z0-9_-]*\).*/\1/p' | tail -1)
            echo ""
            if [ -n "$domain" ]; then
                echo "dashboard: https://$domain"
            else
                echo "dashboard: http://10.77.0.1:7070 (on this host; tunnel in with:"
                echo "           ssh -L 7070:10.77.0.1:7070 root@<this-host>)"
            fi
            [ -n "$token" ] && echo "setup token: $token   (create your account with it)"
        fi
    fi
fi
