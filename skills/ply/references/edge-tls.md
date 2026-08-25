# The edge: TLS, Caddy, and certificates

ply terminates no TLS, issues no certificates and binds no `:443`. ACME, SNI
routing, h2/h3 and websocket upgrades are a decade of someone else's work.
The edge is Caddy (or nginx); ply's job is to say what the upstreams are.

## Point the proxy at the published address

```caddyfile
app.example.com {
	reverse_proxy 10.77.0.1:3000     # the ply parent, not an instance
}
```

The parent already balances the pool, skips unhealthy backends and drains on
deploy, so this line survives scale, rolls, crashes and restarts. Point Caddy
at instance IPs instead and its config must be regenerated on every one of
those events.

`10.77.0.1` is the bridge gateway — where a **rootful** `--publish internal:`
parent binds, reachable from inside any instance. Rootless it is `127.0.0.1`.

## Caddy as a ply app

Works, and makes the edge one more versioned artifact. Two things it needs:

```toml
[package]
name = "edge"
version = "0.1.0"
entrypoint = ["/bin/sh", "-c", "[ -f /etc/caddy/Caddyfile ] || cp /opt/edge/Caddyfile /etc/caddy/Caddyfile; exec caddy run --config /etc/caddy/Caddyfile --adapter caddyfile --watch"]
base = "alpine@3.20"

[dependencies]
caddy = "2"

[env]
HOME = "/tmp"
XDG_CONFIG_HOME = "/tmp"
XDG_DATA_HOME = "/data"    # NOT /tmp — see below

[ports]
http = 80                  # declaring <1024 is what earns CAP_NET_BIND_SERVICE
https = 443

[volumes]
config = { path = "/etc/caddy" }   # live Caddyfile; --watch hot-reloads it
data   = { path = "/data" }        # issued certificates — MUST persist
```

**The certificate volume is not optional.** Caddy stores certs under
`$XDG_DATA_HOME/caddy`. On the instance's tmpfs every restart loses them and
re-issues, and Let's Encrypt's duplicate-certificate limit (5 per week, no
appeal) then locks the domain out. This failure is invisible in staging and
permanent in production.

Run it holding both web ports so Caddy can do its own HTTP→HTTPS redirect and
ACME has an HTTP-01 fallback:

```sh
ply run edge.img --publish 80:80 --publish 443:443
```

## ACME gotchas

- **The domain must point at this host.** Behind Cloudflare's orange cloud,
  TLS-ALPN-01 cannot work at all and HTTP-01 validates against Cloudflare.
  Grey-cloud the record first, and confirm with `dig +short domain A`.
- **Prove the pipeline on staging.** A global block, which must come *first*
  in the Caddyfile, before any site block:
  ```caddyfile
  {
  	acme_ca https://acme-staging-v02.api.letsencrypt.org/directory
  }
  ```
  Staging certs are untrusted by browsers but unlimited. Remove the block to
  issue for real — once.
- **Only `:443` published** means TLS-ALPN-01 only, with no fallback.
- **Verify a restart did not re-issue**: the expiry timestamp must be
  identical before and after. A changed one means the volume is not
  persisting, and every restart is spending quota.

## Rootless and privileged ports

Rootless shares the host netns, and `CAP_NET_BIND_SERVICE` inside a user
namespace does not authorize binding below 1024 out there. Rootless Docker
and Podman have the same limitation. Either bind above 1024 and let a
system-service edge own `:443`, or lower the floor host-wide:

```sh
sudo ply setup --unprivileged-ports
```

## System service instead

Choose an apt/dnf Caddy when TLS should keep working while ply is stopped
entirely: separate supervision, certs in `/var/lib/caddy`, and nothing to
configure beyond the emitted config.
