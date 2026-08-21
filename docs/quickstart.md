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

A ply app is a directory with a `ply.toml`:

```toml
[package]
name = "hello"
version = "0.1.0"
entrypoint = ["python3", "-c", "print('hello from ply')"]
base = "alpine@3.20"

[dependencies]
python3 = "3.12"

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
locked alpine 3.20.7, python3 3.12.13
built hello-0.1.0-linux-x64.img (2.1 MiB)
```

The image is tiny because dependencies are *references*, not copies — ten
Python apps on one host share one python3 in the store. Rebuilding the same
directory produces a byte-identical file, always.

## Run

```sh
ply run hello-0.1.0-linux-x64.img
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
