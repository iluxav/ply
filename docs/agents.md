---
title: Using ply with AI agents
description: A skill file that teaches Claude Code, Cursor or any coding agent how ply actually works — manifests, publishing, service wiring, TLS and the failure modes.
section: Start
order: 2
---

# Using ply with AI agents

Coding agents default to Docker habits, and most of them produce ply setups
that look plausible and do not work: a Dockerfile nobody asked for, `[ports]`
written as if it were `-p 8080:80`, a compose file translated into nothing.

The ply skill fixes that. It is a plain-text brief — the mental model, the
manifest surface, how services find each other, where TLS lives, and the
errors that actually happen — written for an agent rather than a human.

## Any agent: one file

```
https://plybox.sh/ply-skill.md
```

Paste it into the conversation, or drop it in the project as context. Works
with Cursor, Copilot, ChatGPT, Zed, or anything that accepts a document.

For agents that read a project file automatically, saving it as `AGENTS.md`
or `CLAUDE.md` in the repo root also works.

## Claude Code: an installable skill

```
https://plybox.sh/ply.skill
```

Download and install it, and Claude Code loads it on its own whenever a task
touches ply — `ply.toml`, `ply run`, a `.img`, or an error from any ply
command. Progressive disclosure means the detailed references only load when
they are actually needed.

## What it covers

- **Authoring** `ply.toml` — including checking the catalog before writing a
  version range, and what Minimal Version Selection does to a bare major
- **Running** — `--scale`, and `--publish` as the only thing that claims a
  host port
- **Wiring services** — `--publish internal:` plus `--after`, which injects
  `<APP>_ADDR` / `_HOST` / `_PORT` so apps find each other without DNS or a
  stack file
- **TLS** — Caddy at the edge pointed at the published parent, and the
  certificate-volume trap that silently burns your Let's Encrypt quota
- **Importing** Docker images, and why a native package is usually better
- **Debugging** — what `EINVAL` versus `EPERM` means on a rootless `chown`,
  why `Address in use` usually means a stray parent, and the rest

## Keeping it honest

The skill is generated from `skills/ply/` in the repo on every site build, so
it cannot drift from the source. If something in it is wrong, that is a bug
worth reporting like any other.
