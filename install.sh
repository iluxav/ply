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
# prompt-free: no /dev/tty skips everything, and env vars answer the
# questions non-interactively — set them on the SH side of the pipe:
#   curl … | PLY_WIZARD=no sh                  (skip the wizard entirely)
#   curl … | PLY_EDGE=yes PLY_DASHBOARD=yes PLY_DOMAIN=dash.example.com sh
#   curl … | PLY_FLEET=git@github.com:you/infra.git PLY_FLEET_HOST=web-1 sh
PLY_BIN=/usr/local/bin/ply
[ -x "$PLY_BIN" ] || PLY_BIN="$HOME/.local/bin/ply"

is_root_like() { [ "$(id -u)" = "0" ] || { command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; }; }
as_root() { if [ "$(id -u)" = "0" ]; then "$@"; else sudo "$@"; fi; }

# A tty that EXISTS is not a tty that WORKS: CI runners ship a /dev/tty
# node that fails on open. Probe by opening, never by stat.
has_tty() { (exec < /dev/tty) 2>/dev/null; }

ask() { # ask "question" default -> echoes yes|no
    q=$1; default=$2
    if [ -n "${3:-}" ]; then echo "$3"; return; fi        # env override
    if ! has_tty || [ "${PLY_WIZARD:-yes}" = "no" ]; then echo no; return; fi
    if [ "$default" = "yes" ]; then hint="[Y/n]"; else hint="[y/N]"; fi
    printf "%s %s " "$q" "$hint" > /dev/tty
    IFS= read -r answer < /dev/tty || answer=""
    case "$answer" in
        [Yy]*) echo yes ;;
        [Nn]*) echo no ;;
        *) echo "$default" ;;
    esac
}

ask_text() { # ask_text "question" -> echoes answer, whitespace-trimmed
    if [ -n "${2:-}" ]; then echo "$2"; return; fi
    if ! has_tty || [ "${PLY_WIZARD:-yes}" = "no" ]; then echo ""; return; fi
    printf "%s " "$1" > /dev/tty
    IFS= read -r answer < /dev/tty || answer=""
    echo "$answer" | tr -d '[:space:]'
}

# The wizard needs root (systemd units, Caddy, ports 80/443) — skip quietly
# for user-mode installs and CI.
if is_root_like && { has_tty || [ -n "${PLY_EDGE:-}${PLY_DASHBOARD:-}${PLY_FLEET:-}" ]; } && [ "${PLY_WIZARD:-yes}" != "no" ]; then
    echo ""
    # Fleet enrollment is NOT an install question — it happens in the
    # dashboard (paste the infra repo URL) or via env for automation:
    #   curl … | PLY_FLEET=git@github.com:you/infra.git sh
    fleet="${PLY_FLEET:-}"
    if [ -n "$fleet" ]; then
        fleet_host=$(ask_text "fleet host name (hosts/<name>/ in the repo; blank = this hostname):" "${PLY_FLEET_HOST:-}")
        fleet_key=$(ask_text "deploy key path (private repo; blank = public):" "${PLY_FLEET_KEY:-}")
        set -- --edge --fleet "$fleet"
        [ -n "$fleet_host" ] && set -- "$@" --fleet-host "$fleet_host"
        [ -n "$fleet_key" ] && set -- "$@" --fleet-key "$fleet_key"
        mem_kb=$(awk '/MemTotal/{print $2}' /proc/meminfo 2>/dev/null || echo 0)
        if [ "$mem_kb" -lt 2000000 ] && [ "$(ask "create a 2G swapfile? (small hosts need it for builds and memory spikes)" yes "${PLY_SWAP:-}")" = "yes" ]; then
            set -- "$@" --swap 2G
        fi
        as_root "$PLY_BIN" setup "$@"
        echo ""
        echo "fleet host ready — it converges to the repo every minute."
        echo "point your domains' DNS here; certificates issue on their own."
        exit 0
    fi

    if [ "$(ask "enable the HTTPS edge? (Caddy + auto-TLS: apps get domains via --domain)" yes "${PLY_EDGE:-}")" = "yes" ]; then
        as_root "$PLY_BIN" setup --edge
        mem_kb=$(awk '/MemTotal/{print $2}' /proc/meminfo 2>/dev/null || echo 0)
        if [ "$mem_kb" -lt 2000000 ] && [ "$(ask "create a 2G swapfile? (small hosts need it for builds and memory spikes)" yes "${PLY_SWAP:-}")" = "yes" ]; then
            as_root "$PLY_BIN" setup --swap 2G
        fi
        domain=$(ask_text "domain for the dashboard, pointed at this host (blank = skip):" "${PLY_DOMAIN:-}")
    else
        domain=""
    fi

    if [ "$(ask "install the ply dashboard? (web UI, runs as a ply app)" yes "${PLY_DASHBOARD:-}")" = "yes" ]; then
        # A deployment FILE, not a hand unit: the dashboard then shows up on
        # its own /deploy page, follows the registry (newest apps/dashboard
        # wins on every reconcile beat — updates ship themselves), and is
        # retired by deleting the file, like every other app on the host.
        dash_spec=/var/lib/ply/deployments/dashboard.toml
        as_root mkdir -p /var/lib/ply/deployments
        {
            echo 'app = "dashboard"'
            echo 'grant_links = true'
            echo 'publish = ["internal:7070"]'
            if [ -n "$domain" ]; then echo "domain = [\"$domain\"]"; fi
        } | as_root sh -c "cat > '$dash_spec'"
        echo "deploying the dashboard from the registry (deployments/dashboard.toml)"
        as_root "$PLY_BIN" reconcile || true

        # Wait for it, then hand over the one secret that matters.
        echo "waiting for the dashboard to come up …"
        # probe locally: the domain may not even resolve yet, and the
        # dashboard's readiness has nothing to do with DNS
        tries=0
        until curl -fs -m 3 "http://10.77.0.1:7070/healthz" >/dev/null 2>&1 || [ $tries -ge 30 ]; do
            tries=$((tries + 1)); sleep 2
        done
        if ! curl -fs -m 3 "http://10.77.0.1:7070/healthz" >/dev/null 2>&1; then
            echo "! dashboard not up yet — the reconcile verdict is in:"
            echo "  cat /var/lib/ply/deployments/.status/dashboard.status"
        fi
        # the token is printed on the app's first boot; give journald a
        # moment to flush before declaring it missing
        token=""
        tries=0
        while [ -z "$token" ] && [ $tries -lt 10 ]; do
            token=$(as_root journalctl -u ply-dashboard --no-pager 2>/dev/null \
                | sed -n 's/.*setup token: \([A-Za-z0-9_-]*\).*/\1/p' | tail -1)
            [ -n "$token" ] || { tries=$((tries + 1)); sleep 1; }
        done
        echo ""
        if [ -n "$domain" ]; then
            echo "dashboard: https://$domain"
        else
            echo "dashboard: http://10.77.0.1:7070 (on this host; tunnel in with:"
            echo "           ssh -L 7070:10.77.0.1:7070 root@<this-host>)"
        fi
        if [ -n "$token" ]; then
            echo "setup token: $token   (create your account with it)"
        else
            echo "setup token: run \`ply logs dashboard | grep 'setup token'\` to read it"
        fi
    fi
fi
