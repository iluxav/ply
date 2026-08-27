---
title: Dashboard
description: The optional web UI — apps, logs, deploys, update checks, and a real terminal into any container. Itself just a ply app; every button is a file the operator granted.
section: Guides
order: 14.7
---

# Dashboard

ply works entirely from the terminal. The dashboard is the optional web
face on top: apps with live CPU/mem sparklines, log tailing, an events
journal, a deploy wizard for GitHub repos, update indicators with a
one-click deploy, and a real shell into any container — served by a single
static Go binary that is **itself just a ply app**.

The honest premise: they sell a dashboard that needs a server; ply is a
server that happens to have a dashboard.

## Install

On a host prepared with `sudo ply setup --edge`, the dashboard is one
deployment file — it installs, updates, and supervises itself through the
same machinery it displays:

```toml
# /var/lib/ply/deployments/dashboard.toml
github = "iluxav/ply-dashboard"
publish = ["internal:7070"]
domain = ["dash.example.com"]      # point DNS here; the edge does HTTPS
grant_links = true
```

First boot prints a **setup token** to the app's log
(`ply logs dashboard`); the create-account page requires it, which closes
the first-visitor-owns-the-box race. Credentials live in one `auth.json`
in the app's volume — deleting that file is the documented password reset.
The filesystem is the admin API.

## Permissions ARE the ACL

The dashboard reads ply's state through explicitly granted bind mounts
(the `[requests]` links in its manifest, mounted because the operator said
`grant_links = true`). What it may *do* follows from what was granted:

- state, logs, cgroups mounted read-only → observe-only dashboard
- apps dir mounted read-write → **scale, restart, and terminal** work,
  because commands are files in `<apps>/<app>/control/` and the app's own
  run parent consumes them
- deployments dir read-write → the deploy pages work

No roles, no tokens, no API surface. Granting the apps dir read-write
means granting a shell — that is stated here plainly, not hidden.

## Deploying from the dashboard

Paste a GitHub URL. Public repos are inspected: a release carrying a ply
image recommends the *pull CI image* lane; a `package.json` with `next`
prefills the known-good Next.js build; `ply.toml` in the repo means the
repo rules. Private repos work identically once you paste a fine-grained
token (Contents: read, that one repo) — one credential for cloning,
images, and update checks. The wizard previews the exact TOML it will
write, because the file is the truth — and every deployment's spec is
editable in place afterward.

Each GitHub-backed deployment shows freshness — *update available:
`<version or commit>`* — with a **deploy now** button. The button just
touches the spec file; inotify and `ply reconcile` do the rest. With the
timer installed, updates flow with no clicks at all (see
[Deployments & CD](/docs/deployments/)); `auto = false` keeps an app
manual, and the button still works, because a touch is explicit intent.

## Shared env files

The deploy page manages `deployments/.env/*.env` — the secrets your
specs reference (`env_file = ".env/site.env"`) without containing.
The editor lives in the right drawer, values blurred until you ask;
**save** writes the file (0600), **save & apply** also touches every
spec that references it, so reconcile restarts those apps onto the new
values. Specs pointing at absolute paths (`/root/foo.env`) are listed
too, read-only, with a nudge to move them under `.env/` where the
dashboard can reach them. Deleting a file that something still
references is refused.

## Logs, events, post-mortems

Every instance writes a rotating log ring; the dashboard tails it live.
The events journal records deploys, scales, restarts, crash respawns, and
terminal opens — each row links to the logs that explain it, in a
resizable side pane. Build failures link to the **builder's** ring, which
outlives the builder: the post-mortem for a dead build is one click.

## The terminal

Every live instance gets a terminal button. How it works is the whole
security story: the dashboard holds no privileges — it writes an `exec`
command file, and the app's own run parent (already root *for its own
app*) answers by serving a PTY on a unix socket inside the control dir.
The socket crosses the bind mount; the browser speaks to it through a
WebSocket bridge; every open lands in the events journal. Everything is a
file, including the shell.

## Designing without a server

`MOCK=true ply-dashboard` runs the entire UI against a fabricated state
tree — fake apps, wandering sparklines, a crash-looping worker, every
deployment state at once — for UI work with no ply install at all
(login `mock` / `mockmock`).
