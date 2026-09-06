---
title: ply on macOS
description: Native ply run on Apple Silicon — one microVM per instance on Hypervisor.framework, built into the binary — and Lima as the zero-build path.
section: Guides
order: 18
---

# ply on macOS

ply's runtime is built on Linux kernel primitives, so on a Mac each
instance needs a Linux kernel around it. ply brings its own: the binary
contains a **microVM backend** — one small VM per instance on Apple's
Hypervisor.framework, the same `.img` files, the same commands. No
Docker-Desktop-style VM product to install or babysit; nothing resident
between runs.

Two ways to run it today:

| | native microVM | Lima |
|---|---|---|
| what runs | `ply` on the Mac, one microVM per instance | `ply` inside a Linux VM |
| install | build from source (`make install-mac`), Apple Silicon | `brew install lima`, prebuilt binaries |
| ports, names, internet | published on the Mac; `<name>.ply` and outbound via ply's own switch | forwarded by Lima |
| `ply up` stacks | yes | yes |
| `ply exec` | not yet | yes |
| egress contract | not enforced yet | audit and enforce, as on Linux |

## Native: the built-in microVM

**Requirements.** Apple Silicon (M1 or later), Hypervisor.framework
available (`sysctl kern.hv_support` prints `1`), a Rust toolchain. The
binary must carry the `com.apple.security.hypervisor` entitlement, which
`make install-mac` signs in; a prebuilt, signed macOS release is not
published yet, so this path is build-from-source for now.

```sh
git clone https://github.com/iluxav/ply && cd ply
make install-mac                        # builds, signs, installs to /usr/local/bin
                                        # (MAC_PREFIX=~/.local/bin to change)
```

**The kernel.** Each microVM boots ply's own arm64 kernel and initramfs,
pinned per binary (`ply/microvm-kernel@6.12.0`). Until that keg is
published to the registry, build it once — on any aarch64 Linux, which is
what a Lima VM is for — and point ply at the output:

```sh
lima bash -lc 'OUT=$HOME/microvm-build sh ~/ply/scripts/build-microvm-kernel.sh'
export PLY_MICROVM_KERNEL=~/microvm-build   # the directory with microvm-kernel.img + initramfs.cpio
```

**Daily use** is then the Linux experience, on the Mac:

```sh
ply build .                             # → myapp-0.1.0-linux-arm64.img
ply run myapp-0.1.0-linux-arm64.img --publish 3000
curl localhost:3000
ply up                                  # a stack: one microVM per member, wired over the switch
```

Each instance gets its own microVM with the image's layers attached as
block devices; the `ply run` parent owns a userspace switch that gives
members their `<name>.ply` addresses, answers DNS, and NATs to the
internet. Ports publish on the Mac through the parent, exit codes and
signals cross the boundary, `ply ps` and `ply stats` read the same state
files.

**What is not there yet**, said plainly:

- `ply exec` into a microVM (a console channel into the guest is v2).
- The [egress contract](/docs/security/#egress-the-contract): the microVM
  backend reports itself as not enforcing, and runs the app unobserved.
- `[resources]` limits are ignored: a microVM gets a fixed RAM size.
- [Autoscaling](/docs/autoscale/) policies evaluate but have no samples to
  act on (no cgroups, no veth on the Mac side).
- Published ports are relayed by the parent, as on rootless Linux; the
  kernel DNAT path is Linux rootful only.
- Images are `linux-arm64`; build for your servers' architecture in CI
  (the [GitHub Action](/docs/registries/) does x64 for free).

## Lima: the zero-build path

[Lima](https://lima-vm.io) runs a Linux VM with your home directory
shared and guest ports forwarded, so ply inside it feels close to native:

```sh
brew install lima
limactl start                                  # a default Ubuntu VM
lima bash -c 'curl -fsSL https://plybox.sh/install.sh | sh'
cd ~/code/myapp && lima ply build . && lima ply run myapp-0.1.0-linux-arm64.img
```

Everything on the Linux pages applies inside it, including `ply exec`,
egress enforcement (rootful) and autoscaling. `limactl stop default`
frees the VM's memory. Lima is also where you build the microVM kernel
above, so the two paths are not either/or.

## Windows

WSL2 is a real Linux kernel: ply runs in it directly, no extra tooling.

## Status

The native backend is complete for running and wiring apps and is covered
by an integration suite on Apple Silicon (`make mac-test`): disks, exit
codes, stdout, published ports, `.ply` names, outbound through the switch,
signals, stacks. What makes it the default rather than the from-source
path is publishing the kernel keg and a signed macOS release binary; then
`ply exec` and egress. The design that got here is `docs/ply-vm.md` in the
repo.
