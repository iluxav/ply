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
3. Instances roll **one at a time**: stop old → start new → wait for the
   health gate
4. A failed gate **aborts the roll and reverts that slot** — untouched
   instances never left the old version

Zero downtime with `--scale ≥ 2`, and the failure mode is "some instances
still on the old version," never "everything down." `--timeout <s>` bounds
how long to watch before reporting partial progress.

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

## CI-driven deployment

Because a deploy is `scp` + one command over SSH, GitHub Actions can drive
the whole thing — build on release, push the image to your host, roll
with health gates, verify with `ply ps --json`. Dedicated `ply push` /
`ply status` / `ply rollback` verbs for exactly this workflow are on the
roadmap.
