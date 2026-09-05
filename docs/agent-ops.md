---
title: Agent-native ops
description: The filesystem is the API — which means every AI agent already has the SDK. How to operate a ply host with an agent, and why there is no MCP server to install.
section: Guides
order: 14.8
---

# Agent-native ops

Every infra product is currently bolting on an "AI integration": an MCP
server, an API wrapper, a chatbot in the dashboard — adapters between an
agent and a system built for humans with browsers.

ply doesn't need the adapter. An agent's two most reliable tools are *read
a file* and *write a file* — and that is already ply's entire operational
surface. **The filesystem is the API, so every agent ships with the
client.** An agent with ssh access operates a ply host with the tools it
was born with, and it can discover the system by `ls`, because state is
legible text at stable paths — not rows inside a daemon.

## The whole API, one table

| operation | what the agent does |
|---|---|
| deploy an app | write `/var/lib/ply/deployments/<name>.toml` |
| check how it went | read `deployments/.status/<name>.status` |
| see what happened | read `<apps>/events.log` (JSON lines) |
| read logs — dead builders included | read `<run>/logs/<app>.<n>.log` |
| scale to 3 | write `3` into `<apps>/<app>/control/scale` (pins an autoscaled app) |
| hand scaling back to `[scale]` | write `auto` into the same file |
| see what an app reached outbound | `ply egress <app>`; the log is `<data>/egress/<app>.<n>.log` (JSON lines) |
| trigger an update | `touch` the spec file |
| roll back | pin `version =` / `ref =` in the spec |
| shell into an instance | `ply exec <app> sh` |
| machine-readable everything | `ply ps --json`; statuses and events are JSON already |

No token to mint, no SDK version to match, no schema to paste into
context. Compare what an agent needs elsewhere: an API token and a REST
client for Coolify; kubectl, CRD schemas and RBAC for Kubernetes. For
ply: `cat` and `tee`.

## Why the fit is structural, not cosmetic

**Declarative beats imperative — especially for agents.** Agents fumble
long imperative sequences and excel at stating desired outcomes. ply's
model is exactly that: the agent writes what should be true, `ply
reconcile` makes it true — health gates, rolls, retries and failure
backoff absorbed by the machinery — and the status file reports back in
plain text the agent can read and react to.

**Permissions are the ACL — for agents too.** No bot tokens, no roles: an
agent's blast radius is unix permissions on directories, the identical
mechanism that scopes a human or the dashboard. Read-only mounts make an
observing agent. And every action lands in the events journal like
anyone else's — the audit trail exists before the auditor asks.

**Fleet turns agent access into pull requests.** On a
[GitOps fleet](/docs/deployments/), the agent's fleet-wide write is a PR
against the infra repo. It diagnoses at 3am from status + events + the
builder's log ring, opens a PR pinning the last good version, and a human
merges. The approval boundary is git review — mature, understood,
tooled — not a bespoke "agent permission system" nobody trusts yet.

## Recipes (prompt-sized)

Deploy:

```sh
cat > /var/lib/ply/deployments/api.toml <<'EOF'
repo = "https://github.com/org/api"
build = "npm ci && npm run build"
runtime = "node@24"
entrypoint = ["node", "dist/index.js"]
port = 3000
publish = ["internal:3000"]
domain = ["api.example.com"]
EOF
# then watch: cat /var/lib/ply/deployments/.status/api.status
```

Diagnose a failed deploy:

```sh
cat /var/lib/ply/deployments/.status/api.status   # the verdict
tail -50 /var/lib/ply/apps/events.log             # what led up to it
cat /run/ply/logs/api-builder.1.log               # the build's own output —
                                                  # the ring outlives the builder
```

Roll back: edit the spec, pin `version = "1.4.2"` (or `ref = "<commit>"`
for repo builds) — the edit is the trigger. Scale:
`echo 3 > /var/lib/ply/apps/api/control/scale`, answer arrives in
`control/last-result` within ~2s.

## Give your agent the skill

The [ply skill](/docs/agents/) covers packaging *and* operating — the
file map above, the diagnose loop, and the cautions (atomic writes,
secrets stay in root-owned files, one deployment per app name). One URL:

```
https://plybox.sh/ply-skill.md
```

## Where's your MCP server?

You don't need one. ssh is the protocol; the filesystem is the schema.
An MCP layer here would wrap `cat` in JSON-RPC and call it progress. If
your setup insists on one, a thin shim is trivial to write over these
same files — but it will always be optional, because the real interface
is load-bearing and public.

One receipt, offered plainly: this platform — the deploy wizard, the web
terminal, the timers, its own production migration — was built and
operated over two days by an AI agent working through exactly these
interfaces. The product is its own demo.
