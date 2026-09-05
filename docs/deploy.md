---
title: Deploys, health & restarts
description: Zero-downtime rolling deploys with health gates and crash-loop restart policies — no daemon, no orchestrator.
section: Guides
order: 14
---

# Deploys, health & restarts

Everything on this page works without a daemon or orchestrator: state is
files, coordination is a signal, enforcement is the kernel.

## Health gates

```toml
[health]
port = 5432        # TCP connect check against the instance's IP
grace = "30s"      # budget for cold start (mounts + app init)
```

An instance is *healthy* once its declared port accepts a TCP connection.
`grace` is how long a fresh instance may take before the gate fails —
budget for cold-mount I/O and app initialization, not just process start.

## Rolling deploys

```sh
ply build .                                    # new version → new image
ply deploy myapp-1.3.0-linux-x64.img
```

What happens:

1. ply writes a deploy pointer file and sends the app's run parent `SIGHUP`
2. The parent preps the new version while the old instances keep serving
3. Instances roll **one at a time**: the old instance leaves the published
   pool (no new connection reaches it from this moment, in the kernel's
   DNAT or the relay), gets one second for what is in flight, then its stop
   signal; the new one starts and **joins the pool only once it passes the
   health gate** — before that, nothing is routed to it
4. A failed gate **aborts the roll and reverts that slot** — untouched
   instances never left the old version

Zero downtime with `--scale ≥ 2`, and the failure mode is "some instances
still on the old version," never "everything down." `--timeout <s>` bounds
how long to watch before reporting partial progress.

The same rules carry the [autoscaler](/docs/autoscale/): when a `[scale]`
policy adds or removes instances it uses this launch and stop path.

The same readiness rule holds for every launch, not only rolls: an instance
takes traffic once its first published port accepts a connection, so a
scale-up or a crash-restart never routes a request to a process that is
still starting.

**What the app owes.** ply can stop feeding an instance new connections; it
cannot end the keep-alive connections clients already hold on it. On the
stop signal, a well-behaved server marks every further response
`Connection: close` — the client hangs up after reading it and reconnects,
to the other instances — waits until its open connections have drained,
and only then shuts down. What it must **not** do is close idle connections
itself: under load "idle" is the instant a client's next request is already
on the wire, and that request is lost. Go's `SetKeepAlivesEnabled(false)`
and `Shutdown` both close idle connections, so call `Shutdown` only after
the drain; `bench/api/main.go` has the pattern. Measured on the bench: a
rolling restart at scale 2 under 655k requests/s, 39 million requests,
zero lost — with the idle-close variant, one request per connection was.

There's no magic: you can watch the pointer file and per-instance state
files change under `/run/ply/` while it happens.

## Rollback

Images are content-addressed files and registries are append-only — the old
version still exists. Rolling back **is** deploying:

```sh
ply deploy myapp-1.2.9-linux-x64.img
```

## Restart policies

```toml
[restart]
policy = "on-failure"     # or "always" / "never" (default)
backoff = "1s"            # first respawn delay, doubles each failure
max_backoff = "60s"
```

The run parent respawns instances it started — same slot, same volume —
with exponential backoff that resets after healthy uptime. `Ctrl-C` and
`ply rm` are shutdown-aware (no respawn on intentional stops). `ply ps`
shows a RESTARTS column.

The parent stays boring by design: no socket, no API. It supervises only
what it forked.

## Supervision across reboots

The restart policy covers crashes; host reboots are systemd's job:

```sh
ply systemd myapp.img --scale 4 --publish 80:3000 \
  | sudo tee /etc/systemd/system/ply-myapp.service
sudo systemctl enable --now ply-myapp
```

## Continuous deployment

The page above is the push side: `ply deploy` rolls whatever you hand it,
so CI can `scp` an image and run one command over SSH. The published
actions (`iluxav/ply@v1` to build, `iluxav/ply/deploy@v1` to ship and
roll) do it in two workflow steps — the complete recipe is
[Running on DigitalOcean](/docs/digitalocean/).

The pull side is usually the better default: declare a deployment file
naming a GitHub repo or release, and the host converges itself — on push,
within a minute, with no server credentials in CI at all. That whole
story, including building straight from a repo on a $4 droplet, is
[Deployments & CD](/docs/deployments/).
