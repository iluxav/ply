---
title: Notifications
description: ply tells you when a deploy fails, an app crash-loops, a backup fails or a disk fills — to Telegram, Discord, a webhook or a command. No daemon.
section: Guides
order: 17
---

# Notifications

Everything ply does is already a line in the events journal, and `ply
reconcile` already runs every minute on a host set up with `sudo ply setup
--edge`. Notifications ride that beat: each minute ply reads the events
since last time, keeps the ones you subscribed to, and sends a short line
where you say. No new resident process, no metrics stack.

The premise for a one-to-five-server host: you are not going to run
Prometheus and Alertmanager, but you do want to know when a deploy failed
at 3am.

## Turn it on

One file, `/var/lib/ply/notify.toml`:

```toml
on = ["deploy-failed", "restart-loop", "snapshot-failed", "disk-high"]
to = ["telegram:<bot-token>:<chat-id>"]
```

`on` is the events to notify about; `to` is where. That's it — the
reconcile timer picks it up. Flush immediately by hand with `ply notify`,
and prove a destination works with `ply notify --test`.

## What you can subscribe to

Journal events, by name:

| name | fires when |
|---|---|
| `deploy-failed` | a rolling deploy's health gate failed and reverted |
| `deploy` | a deploy completed (noisy; usually you want only the failures) |
| `instance-restart` | an instance crashed and was respawned (every crash) |
| `restart-loop` | **3 crashes of one app within 5 minutes** — one message, not one per crash |
| `snapshot-failed` | a `ply snapshot` failed |
| `snapshot` / `restore` | a snapshot was taken / a volume restored |
| `egress-blocked` | an app under `--egress enforce` tried a destination it may not reach |
| `disk-high` | **the ply data filesystem passed 90% full** (re-warns every 6h while high) |

`restart-loop` and `disk-high` are computed, not raw journal lines:
`restart-loop` fires once on the crash that crosses the threshold, so a
wedged app sends one alert rather than a stream; `disk-high` is checked
each beat. Subscribe to `instance-restart` instead if you truly want every
crash.

## Where it can send

| destination | form |
|---|---|
| Telegram | `telegram:<bot-token>:<chat-id>` (the token keeps its own colon; the chat id is the last field) |
| Discord | `discord:<webhook-url>` |
| any webhook | `https://…` (or `webhook:https://…`) — receives the message as a JSON string |
| a command | `command:<program> [args]` — the message on its stdin |

The `command:` form is the escape hatch: `command:/usr/bin/mail -s ply
ops@example.com` sends email through a configured `mail`, and anything that
reads stdin works the same way.

## Keep the token out of the repo

A bot token is a secret, and a fleet repo is at its best public. Seal the
whole destination ([Sealed secrets](/docs/secrets/)) under the name
`notify`:

```sh
ply secret seal notify=telegram:123456:ABC-DEF:987654 --for ply-host-…
# notify = "enc:v1:…"
```

Put the `enc:v1:…` value in `to`. ply opens it with the host key when it
sends, and never logs it:

```toml
on = ["deploy-failed", "restart-loop", "disk-high"]
to = ["enc:v1:9hf2…"]
```

## What it is not

No severities, silences, escalation or on-call rotations. That is what
Prometheus and Alertmanager are for, and ply hands off to them rather than
becoming them. This is one honest message per event you asked about, to a
place you already read. Point a `https://` destination at your own bridge
if you want it fanned into something bigger.
