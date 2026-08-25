---
title: Quickstart
description: Install ply, write a manifest, build a deterministic image, and run it — in five minutes.
section: Start
order: 1
---

# Quickstart

## Install

```sh
curl -fsSL https://plybox.sh/install.sh | sh
```

As root it installs `/usr/local/bin/ply` and prepares the host. As a regular
user it installs to `~/.local/bin/ply` and tells you if a one-time
`sudo ply setup` is needed (it usually is, for networking and the store).

## Write a manifest

A ply app is a directory with a `ply.toml`. `ply init` writes it for you
(it detects Python/Node projects and asks a few questions); `ply add python3`
adds a dependency at its latest version. By hand, it is:

```toml
[package]
name = "hello"
version = "0.1.0"
entrypoint = ["python3", "-c", "print('hello from ply')"]
base = "debian@13"

[dependencies]
python3 = "3.13"

[sources]
default = "https://registry.plybox.sh/ply/{package}"
```

`base` and `python3` come from the [official registry](https://registry.plybox.sh)
— prebuilt, content-addressed packages served from a CDN.

## Build

```sh
ply build .
```

This resolves the version ranges (writing `ply.lock`), fetches the
dependencies by hash, and produces a deterministic image:

```
locked debian 13.6.0, python3 3.13.5
built hello-0.1.0-linux-x64.img (4.0 KiB)
```

The image is tiny because dependencies are *references*, not copies — ten
Python apps on one host share one python3 in the store. Rebuilding the same
directory produces a byte-identical file, always.

## Run

```sh
ply run hello-0.1.0-linux-x64.img
ply run .                          # same thing: build if changed, then run
```

The app runs in the foreground as a normal process: stdout is your terminal,
`Ctrl-C` stops it, the exit code propagates. Under the hood it got a mount /
PID / network / user namespace, all capabilities dropped, seccomp, and a
read-only rootfs — secure by default, no flags needed.

Useful variations:

```sh
ply run --scale 3 app.img          # three identical instances, each with its own IP
ply run -e KEY=value app.img       # environment overrides
ply run --env-file .env app.img    # secrets stay out of the image
ply run --link ./src:/opt/app app.img   # dev mode: bind-mount live code
```

Need a database next to it? Prebuilt services run by name — no manifest,
no build ([Databases & services](/docs/services/)):

```sh
ply run postgres@17 -e POSTGRES_PASSWORD=dev -e POSTGRES_DB=todos
```

And when the project is db + server + web, one `[stack]` file starts them
all in order: [`ply up`](/docs/stacks/).

## Inspect

```sh
ply ps            # instances, IPs, health, restarts
ply stats         # live CPU / memory / net per instance, no agent
ply exec hello sh # shell into a running instance
```

## Deploy to a host

An image is a file. That's the whole story:

```sh
scp hello-0.1.0-linux-x64.img server:
ssh server ply run hello-0.1.0-linux-x64.img
```

Dependencies fetch by hash from your sources on first run. For zero-downtime
upgrades of a running app, see [Deploys & health gates](/docs/deploy/).

Working with a coding agent? Give it
[the ply skill](/docs/agents/) first — agents reach for Docker habits by
default, and the skill is what stops them writing a Dockerfile you never
asked for.
