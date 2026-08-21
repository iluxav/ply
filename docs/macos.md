---
title: ply on macOS
description: Run ply on a Mac today with Lima — a lightweight Linux VM with automatic file sharing and port forwarding.
section: Guides
order: 18
---

# ply on macOS

ply's runtime is built on Linux kernel primitives (namespaces, overlayfs,
cgroups), so it doesn't run natively on macOS. The practical path today is
[Lima](https://lima-vm.io) — a lightweight Linux VM manager built on
Apple's Virtualization framework, with two touches that make it feel
native: your home directory is shared into the VM automatically, and ports
the guest binds are forwarded to the Mac.

## Setup (once)

```sh
brew install lima
limactl start          # boots a default Ubuntu VM
lima sudo ply setup    # after installing ply inside — see below
```

Install ply inside the VM:

```sh
lima bash -c 'curl -fsSL https://plybox.sh/install.sh | sh'
```

## Daily use

Prefix any ply command with `lima` — it runs inside the VM, in your
current directory (shared automatically):

```sh
cd ~/code/myapp
lima ply build .
lima ply run myapp-0.1.0-linux-x64.img
```

The app's port is forwarded, so `curl localhost:3000` works from the Mac.
Or open a shell and forget the Mac side exists:

```sh
lima          # → a Linux shell, ply installed, your files present
```

## Notes

- The VM is x86_64 or arm64 to match your Mac — on Apple Silicon you'll
  run `linux-arm64` images. Build for your hosts' architecture in CI
  (the [GitHub Action](/docs/registries/) does this for free on x64
  runners).
- `limactl stop default` frees the VM's memory when you're done;
  `limactl start` brings it back.
- Windows: use WSL2 — it's a real Linux kernel, and ply runs in it
  directly with no extra tooling.

## The future

A native `ply run` on macOS — no resident VM, a disposable microVM per
instance — is designed (see `docs/ply-vm.md` in the repo) but not
scheduled. Lima is the recommended path until then.
