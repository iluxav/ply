# ply on macOS: native microVMs inside the `ply` binary

**Date:** 2026-09-02 · **Status:** spec for review · **Supersedes the "parked" status of** `docs/ply-vm.md` (its requirements R1–R6, A1–A7, N1–N2, S1–S3 carry over unchanged unless restated here) · **Builds on:** the `plyvm` spike (github.com/iluxav/plyvm: ~850-line HVF VMM, a 350-line static guest init, the `ply/microvm-kernel` keg)

## Goal

A developer on an Apple Silicon Mac installs ply with the same one-liner as on Linux and runs registry kegs and stacks the same way:

```
curl -fsSL https://plybox.sh/install.sh | sh
ply run postgres@17 -e POSTGRES_PASSWORD=dev -p 5432
ply up ./stack.toml
ply ps · ply logs db · ply stop db
```

The app being developed runs natively on the Mac; the services it needs (postgres, redis, the API it talks to) run as ply instances, each a disposable microVM the `ply run` parent owns. This is the local-development use of ply, the way people use `docker run postgres` today. It is not a production or scaling story.

One binary per platform. No `plyvm` product, no resident VM, no daemon, no GUI, no distro image. The VM backend is the `isolation = "vm"` arm of the runtime seam that already exists in the manifest, compiled in on macOS and selected automatically there.

## Non-goals (v1)

- Intel Macs (Hypervisor.framework on x86 is a different device model; not worth it).
- Dev bind mounts (`links`, `ply.dev.toml` live source). The app under development runs on the Mac; services don't need the developer's source tree. Virtio-fs is v2.
- `[resources]` limits (cgroups have no guest equivalent worth building now).
- `ply exec` into a VM instance (v2: a virtio-console shell channel).
- Running ply *inside* the guest, Windows, Linux `--isolation vm` GA (the backend is the same, the KVM port is a later milestone).
- Any change to image format, lockfiles, resolution, params, secrets, or the registry. The exact `.img` files and lockfiles that run under namespaces run under the VM.

## What the user sees

| | Linux today | macOS (this spec) |
|---|---|---|
| install | `install.sh` downloads `ply-linux-<arch>` | same line; downloads `ply-darwin-arm64`, notarized; Intel gets "Apple Silicon only" |
| `ply run img` | namespaces | one microVM; same foreground process, SIGTERM, exit code |
| images | `linux-x64`/`linux-arm64` by host | `linux-arm64` (the registry's arm64 twins) |
| `ply up` | one netns per stack, `<name>.ply` | one virtual switch per stack owned by the `ply up` parent, `<name>.ply` served by it |
| `--publish` | proxy on the host | host listener forwarded into the guest; same `BindScope` rules |
| volumes | host directories | one ext4 disk per volume under the data dir; persists across runs |
| `ply ps/logs/stats/stop/restart` | state files under `run_dir()` | identical files; the VMM writes them |
| params, secrets, `/run/ply`, `after` conditions | resolved by the parent | identical; the parent resolves, the guest receives |
| first run | | fetches `ply/microvm-kernel` (≈7 MiB) into the store; the version is pinned by the binary, not by the app's lockfile |
| `ply check` | | reports `virtualization: Hypervisor.framework ok` or the reason it is not |

Differences a user can notice: no `links`, no `[resources]`, no `ply exec`, and a ~200 ms slower start. Everything else is byte-for-byte the same commands and files.

## Architecture

### The seam: `runtime::Backend`

`ply-core/src/runtime/run.rs::run()` today does two jobs: the platform-neutral half (load the manifest, resolve layers from the lockfile and the store, compose env, resolve params and self-config, parse `--publish`, allocate the instance IP, write `InstanceState` and the params tree, run the health gate and the `after` gate, supervise restarts, tee logs) and the Linux half (clone into namespaces, mount the overlay, pivot, drop rights, tap/veth into the bridge). This spec splits them at one trait:

```rust
pub trait Backend {
    /// Launch one instance. Returns when the instance is running (or failed to start).
    fn launch(&self, spec: &InstanceSpec) -> Result<Box<dyn Instance>>;
    /// Host-side capability report for `ply check`.
    fn capability(&self) -> Capability;
}
pub trait Instance {
    fn pid(&self) -> i32;                          // the container's pid for ns; the parent's own pid for vm (see below)
    fn ip(&self) -> Ipv4Addr;
    fn signal(&self, sig: Signal) -> Result<()>;   // SIGTERM → guest init forwards to the entrypoint
    fn wait(&self) -> Result<ExitStatus>;
    fn stdout(&self) -> Box<dyn Read + Send>;       // console stream, teed into the log ring as today
    fn tcp_open(&self, port: u16) -> bool;         // the health gate and `after` port probes go through this: a plain connect for ns, a connect through the switch for vm
}
pub struct InstanceSpec {
    pub app: String, pub n: u32,
    pub layers: Vec<PathBuf>,        // store paths in lockfile (overlay) order, base last
    pub entrypoint: Vec<String>, pub workdir: Option<String>, pub user: Option<String>,
    pub env: Vec<(String, String)>,  // fully composed, params resolved, secrets included
    pub volumes: Vec<(String, PathBuf)>,   // container path → host-side backing (dir on Linux, disk on macOS)
    pub publish: Vec<Publish>, pub ip: Ipv4Addr, pub hostname: String,
    pub hosts: Vec<(Ipv4Addr, String)>,    // peers' `<name>.ply` lines
    pub params_tree: PathBuf,        // run_dir()/params (host view)
    pub network: NetworkAttachment,  // Ns { netns, peers, dns } | Switch { socket }
}
```

- `ply-core/src/runtime/ns/` (Linux; today's `container.rs`, `mount.rs`, `netns.rs`, `security.rs`, `exec.rs`, `loopdev.rs`) implements `Backend` behind `#[cfg(target_os = "linux")]`.
- `ply-core/src/runtime/vm/` (macOS; the plyvm code, restructured) implements it behind `#[cfg(target_os = "macos")]`.
- `run.rs` keeps everything above the seam and becomes platform-neutral; `craft.rs` and `manifest.rs`'s Linux-only imports move behind the same gate or into `ns/`.
- The default isolation is the platform's only backend; `isolation = "vm"` in a manifest on Linux remains "not available" until the KVM port.

**Gate:** CI runs `cargo check --target aarch64-apple-darwin -p ply-cli` on every push from the first task on, so macOS never silently breaks again.

### The VMM (`runtime/vm/`)

From plyvm: `machine.rs` (HVF vCPU, memory, DTB synthesis, direct arm64 boot), `blk.rs` (virtio-mmio block), `net.rs` (virtio-mmio net + the userspace stack), `pl011.rs`. Changes for this spec:

- **N disks, not one.** One read-only virtio-blk per layer in `InstanceSpec.layers`, in order, plus one writable disk per volume, plus one small read-only **spec disk**. Device order is the contract with the guest init (below).
- **Two consoles.** virtio-console 0 = the app's stdout/stderr (teed to the log ring exactly as the ns backend's pipe is). virtio-console 1 = the **control channel**: newline-delimited JSON both ways. Guest→host: `{"ready":true}`, `{"exit":N}`, `{"publish":{"key":"finish_boot","value":"ok"}}` (the guest's `/run/ply/self` writes, forwarded). Host→guest: `{"signal":"TERM"}`, `{"params":{...}}` (live-tree updates: `state`, `instances`, `restarts`, a peer's published key).
- **Memory.** Default 512 MiB per instance, `--memory` and a `[resources] memory` override later; the guest sees a `mem=` cmdline.
- **Process model.** The VMM runs on threads inside the `ply run` parent (vCPU thread, device threads, switch thread). `InstanceState.pid` is the parent's pid; `ply stop` signals the parent as today, which sends `{"signal":"TERM"}` over control, waits the existing 10 s patience, then tears the VM down. `ply ps` reads the same state files; `stats` reports VMM RSS and vCPU time from the host thread (good enough for dev).
- **Networking is the parent's job, not the VM's.** See below.

### The guest: kernel keg + init contract

`ply/microvm-kernel` (registry keg, arm64) = the 4.5 MiB kernel + an initramfs holding the static guest init and `mkfs.ext4`/`resize2fs` (static e2fsprogs, ≈1.5 MiB). The kernel is part of the runtime, like the binary itself, not part of the app: the `ply` binary pins one version in a constant (`MICROVM_KERNEL: &str = "ply/microvm-kernel@6.12.<n>"`), fetches it into the store on first use, and `ply self-update` brings the new pin with the new binary. App lockfiles never change (`ply.lock` is `deny_unknown_fields`; a Mac-written lockfile must stay byte-identical to a Linux-written one). `PLY_MICROVM_KERNEL=<path|ref>` overrides the pin for kernel development. The kernel config adds squashfs, overlayfs, ext4, virtio-mmio/blk/net/console to tinyconfig.

Guest init contract (replaces plyvm's "one image" init):

1. Mount `/dev/vda` … `/dev/vdN` (read-only squashfs) as the overlay lowers in device order, tmpfs upper, and pivot into the result. This reproduces exactly what `runtime/mount.rs` builds on Linux from the same lockfile order.
2. Read the **spec disk** (last read-only disk, a raw block image the VMM writes at launch: 8-byte magic `PLYSPEC1`, u32 length, then JSON; no filesystem): `{entrypoint, workdir, user, env, hostname, hosts[], volumes: [{path, dev}], params_seed: {...}}`. Env arrives here, never on the kernel cmdline (cmdline length and visibility).
3. For each volume disk: if the first 4 KiB are zero, `mkfs.ext4 -q`; mount at `path`, chown to `user` (the Linux backend does the same chown for `[package] user`).
4. `/etc/hosts` from `hosts[]`, hostname, `/etc/resolv.conf` → the switch's DNS (10.77.x.1).
5. Seed `/run/ply/<peer>/…` from `params_seed` (facts + `state=starting`), mount `/run/ply/self` as a tmpfs the app may write; watch it (inotify) and forward writes to the host over control; apply host→guest `{"params":…}` updates to the read-only peer nodes. The four parent-owned keys are never writable by the app (same rule as the Linux mount layout).
6. Send `{"ready":true}`, exec the entrypoint as `user` with `workdir`, forward TERM from control, send `{"exit":N}`, power off.

The init is Rust, static, ~600 lines, its own crate in the ply repo (`ply-guest-init/`, built for `aarch64-unknown-linux-musl` by the kernel keg's build script). Task 2's build produces the keg; the registry push of the keg is the owner's.

### Networking

Design principle (from N2 in `docs/ply-vm.md`): macOS gives us no tap device without entitlements we don't want, so the network lives in userspace, in the parent process.

- **A switch per stack, owned by `ply up`.** The `ply up` parent opens one `switch` (an in-process virtual L2 with ARP, DHCP-less static IPs from the same `10.77.0.0/16` allocator, a DNS responder for `<name>.ply`, and NAT egress to the host's sockets). Each member's `ply run` child receives `--vswitch <unix socket path>` (the macOS counterpart of today's `--netns`/`--netns-peer`); its VMM's virtio-net backend is a client of that socket, one Ethernet frame per message with a u16 length prefix over `SOCK_STREAM` (macOS has no `SOCK_SEQPACKET` for unix sockets). Standalone `ply run` starts a private switch in-process. No daemon: the switch dies with the stack's parent, exactly like the netns does today.
- **Published ports.** For each `Publish`, the parent binds the host listener per `BindScope` (loopback for internal, `0.0.0.0` for public; same rules and errors as Linux) and forwards accepted connections to `guest_ip:instance_port` through the switch's TCP stack. The `InstanceState.published_addr` semantics are unchanged, so `discovery_env`, `after` port probes, and the health gate keep working (they connect to the published host address or, inside a stack, to `guest_ip:instance_port` via the switch — the same two branches `discovery_env` already has).
- **The TCP stack.** plyvm's hand-written TCP was "correct because the wire is lossless"; for NAT egress to the real internet that is not enough (retransmits, windows, MSS). Use `smoltcp` (pure Rust, no_std, widely used) as the guest-facing stack in `any_ip` mode (it terminates guest connections to any destination, slirp-style), bridged to host sockets for egress and to accepted host connections for inbound. This is the one new dependency the spec adds.
- **DNS.** The switch answers `<name>.ply` for stack members and forwards everything else to the host's resolver.

### Params, secrets, live tree, `after`

Nothing in the resolution path changes: `ply up` and `ply run` resolve params, mint secrets, compose env, and compute waits on the host exactly as today (params.rs, secrets.rs, stack.rs are platform-neutral). The differences are delivery: env goes in the spec disk; the params tree's host view stays at `run_dir()/params/<app>/` and is the source of truth for `after` conditions (evaluated by the parent, as on Linux); the guest gets a seeded copy plus live updates and forwards its self-published keys back. Secret-tainted env never touches argv or the cmdline (the spec disk is a file readable only by the parent's user, deleted at exit).

### Platform, signing, install, release

- **Capability:** `Backend::capability()` on macOS checks Apple Silicon + macOS ≥ 15 + `hv_vm_create` succeeding; `ply check` prints it; `ply run` fails loudly with the remedy otherwise.
- **Entitlement:** `com.apple.security.hypervisor` (from plyvm's `hv.entitlements`). A downloaded binary needs Developer ID signing + notarization to run without Gatekeeper friction: the release lane gets a `macos-latest` job that builds `aarch64-apple-darwin`, codesigns with the entitlement, notarizes (`notarytool`), and attaches `ply-darwin-arm64` to the release. Requires an Apple Developer ID certificate + notarization credentials as CI secrets (owner action; until then `install.sh` offers `PLY_FROM_SOURCE=1` which compiles locally and ad-hoc signs, as plyvm's installer does today).
- **`install.sh`:** `Darwin` + `arm64` → `ply-darwin-arm64`; `Darwin` + `x86_64` → "Apple Silicon only"; `ply self-update` learns the darwin asset name.
- **Kernel keg:** built by `scripts/build-microvm-kernel.sh` (Linux CI, cross toolchain or the plyvm recipe), published as `ply/microvm-kernel@<kver>.<n>` for arm64.

### Testing

- **Unit (any host):** the split itself (`run.rs` platform-neutral; every existing test still passes on Linux); `InstanceSpec` construction from a manifest + lockfile (golden); spec-disk JSON round-trip; the switch's ARP/DNS/NAT table logic and the port-forward state machine on synthetic frames (smoltcp has a loopback device for this); guest-init logic factored as pure functions (device ordering, spec parsing, hosts rendering) tested on Linux.
- **Integration (macOS only, `make mac-test`, run by the Mac session and by a `macos-latest` CI job if its runner allows HVF):** boots `debian` with `entrypoint = ["/bin/true"]` → exit 0 in < 2 s; boots postgres arm64 with `-p 5432` and a volume, waits for the health gate, connects over TCP, stops, restarts, and finds the data persisted; a two-member stack (`postgres` + a tiny dependent) resolves `db.ply` and `{db.url}`; `ply stop` from another terminal ends the instance within the patience window; `ply check` reports the capability.
- **Lockfile invariance:** a lockfile written by `ply build` on the Mac is byte-identical to one written on Linux for the same manifest (the kernel pin never leaks into it).

### Milestones (with a Claude Code session on the Mac driving the loop)

| # | Deliverable | Verifiable | Estimate |
|---|---|---|---|
| 0 | **libkrun spike** (throwaway): can it boot our kernel or does it require libkrunfw; attach N block devices in order; expose port forwarding and a per-stack L2; binary size and C deps; boot time. Decision rule: adopt libkrun only if it does N-disk rootfs + port mapping + our kernel; otherwise proceed with plyvm's VMM | a written result | 1 day |
| 1 | **Backend seam**: trait, `ns/` behind `cfg(linux)`, `run.rs` neutral, darwin `cargo check` gate in CI; `ply build/inspect/push/search/up --plan` run natively on the Mac | Linux tests unchanged; darwin check green; Mac smoke | 1–2 days |
| 2 | **Guest init + kernel keg**: N-disk overlay, spec disk, volumes with `mkfs`, control channel; keg build script | `debian` boots to `/bin/true`, exit code back | 3–4 days |
| 3 | **VMM in `runtime/vm/`**: plyvm code restructured, two consoles, N disks, spec disk, volumes, `Instance` impl, state files, `ply ps/logs/stop/restart` | postgres runs with a volume, persists | 4–5 days |
| 4 | **Networking**: switch (smoltcp), `--vswitch`, published ports, DNS, `<name>.ply`, `discovery_env`/health/`after` over the switch | `-p 5432` reachable; a two-member stack wires via `{db.url}` | 5–7 days |
| 5 | **Install + release**: darwin job, signing/notarization (owner's certificate), `install.sh`, `self-update`, `ply check`, docs (`docs/macos.md` rewritten: Lima becomes the fallback) | a fresh Mac installs with the one-liner and runs postgres | 2–3 days |

About three working weeks on the Mac, plus the owner's Apple Developer setup in parallel. Milestone 1 is done on Linux, in this repo, before the switch; milestone 0 is the first thing done on the Mac. This spec therefore gets **two implementation plans**: plan 1 = milestone 1 (the seam; executable here, verifiable with `make check` and the darwin `cargo check`), plan 2 = milestones 0 and 2–5 (needs the Mac to boot anything).

### Out of scope, deliberately

virtio-fs/`links` (v2), `ply exec` into VMs (v2), Linux `--isolation vm` via KVM (v2; the trait and guest contract are designed so it is a second `Backend` impl, not a redesign), Intel Macs, Windows, GPU, snapshots, a shared long-lived VM.

### Open questions settled here

- **Why not virtio-fs for the rootfs?** Blk-per-image keeps content addressing, store dedup, and the Linux overlay assembly identical; virtio-fs would need a host daemon per instance (there is no virtiofsd on macOS) and a different assembly. Decided: blk.
- **Why an in-process switch instead of vmnet.framework?** vmnet needs the `com.apple.vm.networking` entitlement (restricted) or root; the in-process switch needs nothing and dies with the parent. Decided: userspace.
- **Why a control console instead of virtio-vsock?** HVF has no vsock device; a second virtio-console with JSON lines is 100 lines and enough. Decided: console.
- **Volumes as disks, not shared directories.** Persistence like Docker's named volumes; no host daemon; the existing `ply volume` commands operate on the disk files under `data_dir()/volumes/` (one `<app>/<name>.<instance|shared>/disk.ext4` — the same per-volume directory the Linux backend bind-mounts, so the supervisor's volume bookkeeping and empty-volume warning are shared; sparse, grown by `resize2fs` from the guest when a manifest raises the size). Decided: disks.

## Appendix: what plan 1 left for the VM backend (from its whole-branch review, 2026-09-03)

Plan 1 (`docs/superpowers/plans/2026-09-02-runtime-backend-seam.md`) landed the seam. Three things in the supervisor still assume a child process or namespace vocabulary; plan 2 budgets for them rather than discovering them mid-VMM:

1. **Graceful stop reaches an instance only through `Instance::child_pid()`.** The signal handler is the only sender of the declared stop signal; the main loop's shutdown branch sends SIGKILL after `SHUTDOWN_GRACE` and nothing else. A VM instance (`child_pid() == None`) would get SIGKILL ten seconds after `systemctl stop`. Plan 2 adds one main-loop step: on first observing `SHUTTING_DOWN`, `inner.signal(stop_signal)` for every instance whose `child_pid()` is `None`; the handler path stays for namespaces, where the loop may be blocked in the `--after` wait.
2. **Published-port pools hold raw `SocketAddr`s** and `publish::serve` connects to them directly. `Instance::tcp_open` covers only the health and `--after` probes. The switch either exposes host-reachable addresses for each guest port, or `Pool` gets a connector from the backend.
3. **`same_network = opts.netns.is_none()`** and the `RunOptions` fields `netns`/`netns_dns`/`netns_peers` are namespace vocabulary in a portable struct. `same_network` moves into `NetworkFacts`; the fields are renamed when the `--vswitch` attachment lands.

Linux-only paths that remain on portable code and will silently degrade on macOS until plan 2 routes them through the seam: `paths::is_root` reads `/proc/self/uid_map` (unreadable → trusts the uid, so `sudo ply` on a Mac picks Linux-root paths); `state::reap_stale` calls `hosts::remove_entry` (`/etc/hosts`) unconditionally and propagates its error; `lifecycle.rs` reads `/proc/<pid>/stat` for the parent pid; `stats.rs` falls back to `/proc/<pid>/status|stat` and `/sys/class/net/<veth>/statistics`, so `ply stats` off Linux reports every field as unknown; `ps.rs` reads `/proc/<ppid>/exe` and `/proc/<pid>/stat` to detect the supervisor; `reconcile.rs` reads `/proc/meminfo`.

Two supervisor semantics worth knowing when writing `VmInstance`: `Instance::alive()` for namespaces is `kill(pid, 0)`, which a zombie still answers, so `Health::Died` is only observed once something reaps; and `stop_with_patience` gives up five seconds after SIGKILL and returns `None`, after which the supervisor drops the instance without a final reap (a process stuck in D state; the old code hung forever there).
