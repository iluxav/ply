---
title: ply on macOS
description: Native ply run on Apple Silicon — one microVM per instance on Hypervisor.framework, built into the binary, installed by the same curl line as on Linux — and Lima as the alternative.
section: Guides
order: 18
---

# ply on macOS

On an Apple Silicon Mac, the installer installs ply:

```sh
curl -fsSL https://plybox.sh/install.sh | sh
```

ply's runtime is built on Linux kernel primitives, so on a Mac each
instance needs a Linux kernel around it. ply brings its own: the binary
contains a **microVM backend** — one small VM per instance on Apple's
Hypervisor.framework, the same `.img` files, the same commands. No
Docker-Desktop-style VM product to install or babysit; nothing resident
between runs. The backend is experimental: what it does not do yet is
[listed below](#what-is-not-there-yet).

Two ways to run it:

| | native microVM | Lima |
|---|---|---|
| what runs | `ply` on the Mac, one microVM per instance | `ply` inside a Linux VM |
| install | the installer, Apple Silicon | `brew install lima`, then the installer inside |
| ports, names, internet | published on the Mac; `<name>.ply` and outbound via ply's own switch | forwarded by Lima |
| `ply up` stacks | yes | yes |
| `--scale`, `ply deploy` | yes | yes |
| `ply.dev.toml` links (live source) | yes, over virtio-9p | yes |
| `ply exec` | yes (no interactive shell) | yes |
| egress contract | not enforced yet | audit and enforce, as on Linux |
| Intel Mac | no | yes |

## Native: the built-in microVM

**Requirements.** Apple Silicon (M1 or later) and Hypervisor.framework
(`sysctl kern.hv_support` prints `1`; it does on every Mac that is not
itself a VM). The installer puts the release binary in `/usr/local/bin`
(or `~/.local/bin` without sudo) and checks that it carries the
`com.apple.security.hypervisor` entitlement — the release is signed with
it, because `hv_vm_create` refuses a binary without it, at the call, with
an error that names nothing. There is no `ply setup` on a Mac: nothing on
the host needs preparing.

**The kernel.** Each microVM boots ply's own arm64 kernel and initramfs,
pinned per binary (`ply/microvm-kernel@1.0.0`, whose description names
the Linux version it carries), fetched from the
registry the first time a microVM boots and kept in the store like any
package. `ply self-update` brings a new pin with a new binary; no
`ply.lock` ever mentions it, so a lockfile written on a Mac is
byte-identical to one written on Linux.

**Daily use** is the Linux experience, on the Mac:

```sh
ply build .                             # → myapp-0.1.0-linux-arm64.img
ply run myapp-0.1.0-linux-arm64.img --publish 3000
curl localhost:3000
ply up                                  # a stack: one microVM per member, wired over the switch
```

Each instance gets its own microVM, in a worker process of its own, with
the image's layers attached as block devices; the `ply run` parent owns a
userspace switch that gives members their `<name>.ply` addresses, answers
DNS, and NATs to the internet. Ports publish on the Mac through the
parent, exit codes and signals cross the boundary, `--scale` boots one
machine per instance, `ply deploy` rolls them, and `ply ps` shows each
worker's pid. The guest's clock is set from the Mac's at boot, so TLS and
timestamps are right.

**Live source.** A [`ply.dev.toml`](/docs/stacks/#plydevtoml-the-dev-overlay)
link is shared into the microVM over virtio-9p, uncached: an edit on the
Mac is what the next read inside the guest sees, so `tsx watch`,
`nodemon` and friends work as they do on Linux. The shared tree appears
owned by the app's user.

**Running a command inside an instance.** `ply exec <app> <cmd>` works the
way it does on Linux — the command runs inside the instance, as the app's
user, with the app's environment and workdir, and its output and exit code
come back:

```sh
ply exec myapp ls /opt
echo '{"a":1}' | ply exec myapp jq .a
ply exec myapp sh -c 'exit 3'; echo $?     # 3
```

There is no namespace to enter, so it is not `setns` as on Linux: the
request crosses the control channel the machine already has, and the
guest's init runs the command beside the app. Output streams back as it
is produced, stdout and stderr kept apart, byte for byte. What is missing
is an interactive shell — that needs a pseudo-terminal, which this guest
kernel is built without — so `ply exec app sh` gives you a shell with no
prompt and no line editing. Use `sh -c '…'`.

### What is not there yet

Said plainly:

- An interactive terminal (`ply exec app sh` as a prompt, and the
  dashboard's web terminal): the guest kernel has no pseudo-terminal
  support. Commands run with pipes, which is what a script or an agent
  wants.
- The [egress contract](/docs/security/#egress-the-contract): the microVM
  backend reports itself as not enforcing, and runs the app unobserved.
- `[resources]` limits are ignored: a microVM gets a fixed RAM size.
- [Autoscaling](/docs/autoscale/) policies evaluate but have no samples to
  act on (no cgroups, no veth on the Mac side).
- Published ports are relayed by the parent, as on rootless Linux; the
  kernel DNAT path is Linux rootful only.
- Images are `linux-arm64`; build for your servers' architecture in CI
  (the [GitHub Action](/docs/registries/) does x64 for free).

### Building it yourself

For work on the backend: `make install-mac` builds the release binary,
signs it with the entitlement and installs it (`MAC_PREFIX=~/.local/bin`
to change where). Signing happens on the installed copy, never on
`target/release/ply`: cargo re-uplifts that path on its next run and
silently strips the signature. `make mac-test` boots the integration
suite. A kernel other than the pinned keg — a local build from
`scripts/build-microvm-kernel.sh`, run in Lima — is
`PLY_MICROVM_KERNEL=<dir with microvm-kernel.img + initramfs.cpio>`.

## Lima: the other path

[Lima](https://lima-vm.io) runs a Linux VM with your home directory
shared and guest ports forwarded, so ply inside it feels close to native
— and it is the path on an Intel Mac, and the one with `ply exec` and
egress enforcement today:

```sh
brew install lima
limactl start                                  # a default Ubuntu VM
lima bash -c 'curl -fsSL https://plybox.sh/install.sh | sh'
cd ~/code/myapp && lima ply build . && lima ply run myapp-0.1.0-linux-arm64.img
```

Everything on the Linux pages applies inside it, including `ply exec`,
egress enforcement (rootful) and autoscaling. `limactl stop default`
frees the VM's memory. The two paths are not either/or: both use the same
images and the same registry.

## Windows

WSL2 is a real Linux kernel: ply runs in it directly, no extra tooling.

## Status

The native backend is complete for running and wiring apps and is covered
by an integration suite on Apple Silicon (`make mac-test`): disks, exit
codes, stdout, published ports, `.ply` names, outbound through the switch,
signals, stacks, the clock, `--scale`, links, `ply deploy`, `ply exec`. It
ships as a signed release binary, installed by the installer, with its
kernel in the registry. Next are an interactive terminal and egress
enforcement in the VM. The design that got here is `docs/ply-vm.md`
in the repo.

**Signing, for the curious.** The release is signed ad-hoc with the
hypervisor entitlement, which is what Hypervisor.framework checks. It is
not notarized: a binary fetched by `curl` carries no quarantine flag, so
Gatekeeper never asks. A binary downloaded through a browser from the
GitHub release page does get the flag and macOS refuses to run it until
the flag is cleared (`xattr -d com.apple.quarantine ply`); use the
installer.
