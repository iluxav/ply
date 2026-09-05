# macOS VM Backend Implementation Plan (milestones 2–5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `ply run postgres@17 -p 5432` and `ply up ./stack.toml` work natively on an Apple Silicon Mac, each instance a microVM the `ply run` parent owns — milestones 2–5 of the macOS spec, with zero behaviour change on Linux.

**Architecture:** A second `Backend` (from plan 1) under `ply-core/src/runtime/vm/`. `VmBackend::launch` boots one microVM per instance on Hypervisor.framework threads inside the `ply run` parent: the lockfile's images become N read-only virtio-blk disks in overlay order, each volume a writable ext4 disk, and one small **spec disk** carries entrypoint/env/hostname/hosts/volumes to a static guest init that assembles the overlay and execs the app. Two virtio-consoles: `hvc0` is the app's stdout (teed to the log ring exactly as the namespace backend's pipe is), `hvc1` is a newline-delimited JSON control channel (`ready`, `exit`, `publish`; `signal`, `params`). Networking is the parent's job, not the VM's: `ply up` owns one in-process L2 switch per stack (smoltcp, ARP + static IPs + `<name>.ply` DNS + NAT egress), members join it over a unix socket with `--vswitch`, and published host ports are forwarded through it.

**Tech Stack:** Rust 2021 workspace; `applevisor` 1 (`macos-15-0`) + `vm-fdt` 0.3 for HVF and the device tree, both macOS-only; `smoltcp` for the switch's TCP/IP (portable, so its tests run in Linux CI); a new no-dependency-but-serde workspace crate `ply-vm-proto` shared by host and guest; a new standalone `ply-guest-init` built for `aarch64-unknown-linux-musl`; Linux 6.12 tinyconfig + squashfs/overlay/ext4/virtio for the `ply/microvm-kernel` keg.

**Spec:** `docs/superpowers/specs/2026-09-02-macos-native-vm-design.md` — milestones 2–5, plus its Appendix ("what plan 1 left for the VM backend").
**Milestone 0 result:** `docs/superpowers/specs/2026-09-03-libkrun-spike-result.md` — proceed with plyvm's VMM; three kernel/initramfs consequences carried in below.
**Plan 1:** `docs/superpowers/plans/2026-09-02-runtime-backend-seam.md` — the seam this builds on.
**Source to port:** the plyvm spike, cloned at `/Users/iluxav/Documents/plyvm` (github.com/iluxav/plyvm @ `9f41d84`).

---

## Global Constraints

- **Zero behaviour change on Linux.** `make check` (fmt, clippy `-D warnings`, `cargo test --workspace`) stays green after every task, and CI runs it. No message `ply run` prints on Linux changes text.
- **`make check` does not run on a Mac** — `cargo test --workspace` fails to compile `ply-core`'s test code, which references `crate::runtime::ns` (Linux-only). That is pre-existing on this branch, not something a task here caused; Task 1b narrows it to the two tests that genuinely need Linux. Until then, and for every task after, run the Linux gate in the `default` Lima VM, which has the repo mounted at the same path:

  ```sh
  limactl start default
  lima bash -lc 'export PATH=$HOME/.cargo/bin:$PATH; export CARGO_TARGET_DIR=$HOME/ply-target; cd /Users/iluxav/Documents/ply && make check'
  ```

  The VM-local `CARGO_TARGET_DIR` is not optional: without it the Linux and macOS builds fight over `target/debug` and each one forces a full rebuild of the other.
- **The darwin gate.** `cargo check --target aarch64-apple-darwin -p ply-cli` plus `cargo clippy --target aarch64-apple-darwin -p ply-cli -- -D warnings` must be clean after every task. On the Mac that is `cargo check -p ply-cli` and `cargo clippy -p ply-cli -- -D warnings`. CI's `darwin` job (`.github/workflows/ci.yml`) already runs it.
- **The owner commits.** Implementers never run `git commit`, `git push`, or `git tag`. Where a step says "commit", read: stop and report with the verification output.
- **Lockfile invariance.** Nothing in this plan may change a byte of `ply.lock`. The kernel pin is a constant in the binary, never a lockfile entry. Task 6 has the test that proves it.
- **Portable logic is tested on Linux.** Everything that is not literally touching Hypervisor.framework — the spec-disk format, the control protocol, the switch's ARP/DNS/NAT/port-forward logic, the guest init's pure functions — compiles and is unit-tested on every platform, so Linux CI catches regressions in it. Only the device models and the vCPU loop are `cfg(target_os = "macos")`.
- **No new runtime behaviour on Linux.** `isolation = "vm"` on Linux stays "not available" — the KVM port is v2. `default_backend()` picks `VmBackend` on macOS only.
- **Secrets never reach argv or the kernel cmdline.** Composed env goes in the spec disk, which is created `0600` under the instance directory and removed when the instance drops.

## Rulings carried in from milestone 0

Recorded in `docs/superpowers/specs/2026-09-03-libkrun-spike-result.md` and binding on this plan:

- **R0-1** — plyvm's VMM, not libkrun.
- **R0-2** — the `--vswitch` wire format is a **4-byte big-endian length prefix followed by the raw Ethernet frame** (passt/gvproxy's protocol), superseding the spec's "u16 length prefix". Existing tools can then read the wire.
- **R0-4** (new, from the spike's findings) — the kernel config gains `CONFIG_EXT4_FS` (volumes) and `CONFIG_VIRTIO_CONSOLE` (the control channel), and the initramfs must contain **`/dev/null`** as well as `/dev/console`, or a Rust `std` guest init dies before `main` whenever the console is missing or late. plyvm's `mkinitramfs.py` creates only `/dev/console`; that bug cost an hour in the spike.
- **R0-5** (new) — the spec disk is found by **scanning every disk for the `PLYSPEC1` magic**, not by trusting "the last read-only disk". Same contract, one failure mode fewer, no extra cost.

Recorded during execution:

- **R1-1** — the params tree's wire type is `ply_vm_proto::ParamsTree` (`Vec<(String, Vec<(String, String)>)>`). Spell it with the alias everywhere, including in Tasks 6, 8 and 9: `clippy::type_complexity` rejects the bare tuple nest under `-D warnings` the moment it appears inside an `Option` or a return type. It serializes as nested **arrays**, not an object — `{"params":[["db",[["state","starting"]]]]}` — because it is ordered and deterministic, which an object is not.

## The worst defect this plan contained, and why its test did not catch it

Task 6 found that **this plan's `volumes::needs_format` read the ext4 magic at the wrong offset**. `s_magic` lives at 0x38 *inside* the superblock, which starts at 1024 — absolute offset **1080**, not 1024. The version written in this plan therefore answers "this disk has never been formatted" for **every ext4 volume ever made**, and the guest init reformats it. A user's database is destroyed on the second `ply run`. Task 6's harness printed `formatting /dev/vdc (first use)` over a live filesystem on boot 2, which is how it surfaced.

The plan shipped a unit test for that function. It passed. It passed because **the test wrote the magic wherever the implementation looked** — both halves came from the same wrong belief, so they agreed with each other and with nothing else. Writing the test first does not help when the same author writes both from the same misunderstanding; only the external fact does. The fix carries a regression test that asserts *both* halves: magic at 1080 → formatted, magic at 1024 → **not** formatted.

Two defences were added beyond the fix, and both should stay: `-F` was **dropped** from the `mke2fs` arguments, so mke2fs's own refusal to overwrite a filesystem it can see is a second, independent guard (a genuinely fresh sparse volume carries no signature and is never refused); and a device carrying `hsqs` or `PLYSPEC1` is never treated as a volume.

If you are extending this plan: anywhere it states a byte offset, a magic number, or a wire layout, verify it against the real artifact before trusting the accompanying test.

### A third instance of the same pattern: `ply build` would have shipped an empty keg

This plan put the kernel payload at `/opt/microvm-kernel/`. Two facts about `ply build` make that wrong, and **both fail silently**:

- `ply build` packs a **keg** at `/opt/<name>-<version>`, not `/opt/<name>` — apps get the un-versioned prefix. (An earlier draft of this section claimed `build::tests::package_build_uses_keg_prefix_and_layer_toml` pins that. **It does not** — that test writes `/opt/tool-2.0.0/bin` into its own manifest and asserts the same string comes back out of the embedded `.layer.toml`; it never lists the image, and it passes unchanged if the prefix changes. Citing a guard that does not exist is worse than citing none, because it stops the next reader looking.)
- Its file filter drops **any top-level `*.img`**. A `microvm-kernel.img` staged at the keg root ships as *nothing*: a keg that builds, pushes, resolves and extracts perfectly, with no kernel inside it. The failure would first appear deep in `machine.rs` as a missing file, three tasks later.

The payload therefore lives at `/opt/microvm-kernel-<version>/boot/`, and the layout is written down in exactly two places — `kernel::keg_payload_dir` (with a unit test) and the build script's `keg/boot` — each pointing at the other. The build script now also reads the payload back out of the finished image with `unsquashfs -l` rather than trusting that `ply build` packed what it was given.

Same lesson as the two above: **verify against the artifact, not against the intent.**

## Findings recorded during execution, not fixed here

Two things code review surfaced that are larger than the task that surfaced them. Neither blocks anything; both should be decided deliberately rather than by inertia.

- **`alone_in_its_network` is portable logic that is now tested on one platform.** Task 1b gated its test because the function lives in `runtime::ns`. A reviewer extracted the function verbatim and ran its five assertions natively on macOS — all pass; it is a truth table over `(rootless, network, scale)` and `in_namespace` is short-circuited in four of the five cases. So the gate is about *where the code lives*, not what it computes, which sits awkwardly against this plan's own "portable logic is tested on Linux" constraint. The counter-argument is real: `NetworkFacts.alone` is portable, but `alone_in_its_network` is `NsBackend`'s private *answer* to it, and a VM backend answers differently — hoisting it would split a policy from its only implementor. **Decide when a second backend needs the same rule** (Task 10 is the first place that gets close), not before.

- **`olddefconfig` drops config options silently, and `tinyconfig` makes it likely.** Task 2 hit this for real: five options the fragment asked for were absent from the built kernel — `CONFIG_VIRTIO_MMIO` (gated by `VIRTIO_MENU`), `CONFIG_SQUASHFS`/`_XZ`/`_ZSTD` (gated by `MISC_FILESYSTEMS`), `CONFIG_TMPFS` (`depends on SHMEM`) — because `tinyconfig` turns off the umbrella symbols and `olddefconfig` then discards their children without a word. The first build produced a 5.1 MB kernel with **no virtio transport and no squashfs**, and the trap is that `CONFIG_VIRTIO_BLK` and `CONFIG_VIRTIO_NET` sit *outside* `VIRTIO_MENU`, so the devices looked present while the bus they attach to was gone. Inside a VM this is not an error; it is a guest that boots and finds no disks, with no way to say so.

  Two rules follow, and the build script now enforces both. **Never trust a line in `kernel/microvm.config` — assert it in the built `.config`.** And when adding an option, add its menu umbrella and its `depends on` chain too. This applies to every future kernel bump, not just to Task 2.

- **Disk order is NOT the order the VMM passed them — demonstrated, not theorised.** Task 2's boot smoke test failed on its first run because qemu's `virt` machine hands out virtio-mmio transports in **reverse**: the layer passed first came up as `/dev/vdb`. That turns the theoretical finding below into a live hazard with a reproduction. The smoke init now *probes* (the layer is whichever disk mounts as squashfs) rather than trusting position — the same discipline as R0-5.

  **Task 8 owns this mapping**: ply's own DTB decides the order, so state it explicitly and assert it. **Task 6 must not assume `/dev/vda` is the first disk the VMM attached.** A wrong assumption here produces no error — just an older layer's binary winning.

- **Nothing verifies that `/dev/vdN` actually *is* `images[N]`, and R0-5 already rejected that kind of trust once.** `device_name` is byte-identical to Linux's `sd_format_disk_name` (verified over 200,000 indices) and `lowerdir`'s ordering matches the namespace backend's through all four hops — but both rest on an unverified premise: that the VMM's device-tree order and Linux's virtio-blk probe order agree, so disk *i* carries image *i*. If that is off by one or permuted there is no error, just an older layer's binary winning. R0-5 made exactly this argument for the spec disk and chose a magic scan; the layer disks never inherited it.

  **Task 6 gets the cheap half nearly free:** `find_spec_disk` already reads the head of every device, so asserting that devices `0..layer_count` each carry the squashfs magic `hsqs` costs one comparison per disk and catches a *shifted* mapping — the likely bug, e.g. the spec disk or a volume landing before the layers. It does not catch a permutation. If that is wanted, the honest fix is a per-layer marker or virtio-blk's serial config field (`/sys/block/vdX/serial`), which is a **Task 8 device-model decision** worth making deliberately rather than by omission.

- **`resolv_conf` is a real parity gap with the namespace backend, in two ways.** `ns/container.rs:140-148` never leaves `/etc/resolv.conf` unwritten — both arms write a file — and `resolv_conf_via` preserves the host's `search` and `options` lines. The guest emits `nameserver <ip>\n` and nothing else, so a stack member that inherits a corporate search domain on Linux does not in a microVM. Fixing it properly needs a `dns_search` field on `SpecDisk` (Tasks 1 and 7), which this plan does not do.

  Also fix the *stated reason*: the doc says `None` means "the guest keeps whatever the image shipped", but ply images are per-package squashfs layers and neither `alpine-baselayout` nor Debian `base-files` ships a `/etc/resolv.conf`, so in practice it keeps nothing. The behaviour is fine — once Task 13 lands, `preflight` always supplies a switch resolver and the `None` arm is unreachable. The reason is wrong, and a future reader will act on the reason.

- **`/etc/hosts` goes stale in a VM in a way it does not on Linux.** On the namespace backend `/etc/hosts` is a *bind mount* of `instance_dir/hosts`, rewritten in place whenever a peer restarts on a new address, precisely so a sibling does not keep dialling a dead IP (`runtime/hosts.rs:63-70`). The guest gets a **start-time copy** from the spec disk instead. So inside a stack, a peer that restarts and lands on a different address is stale to every VM that already booted. The control channel is the obvious carrier for a hosts refresh — `HostLine` would need a variant, or `Params` could carry it. **Not in scope for this plan**, whose stacks are small and whose addresses are stable within a run; recorded because it is a behavioural difference from Linux and the failure mode is a connection to nowhere, not an error.

- **`InstanceSpec.local_aliases` has no home in `SpecDisk`** — but the fix is NOT what an earlier draft of this section prescribed, and that prescription was actively dangerous.

  On Linux, `ns/container.rs` appends `127.0.0.1\t<alias>.ply` for siblings sharing a namespace, and loopback is *literally correct* there because they share one network stack. **In a microVM each member is its own machine on the switch.** This plan's own Task 13 says so ("siblings resolving to their switch IPs") and Task 14 has the switch's DNS answering `<name>.ply` from its name→IP table — so the earlier instruction to write `127.0.0.1` contradicted two later tasks.

  It would not have merely failed to help. **`/etc/hosts` is consulted before DNS**, so a `127.0.0.1 db.ply` line shadows the switch's correct answer for the life of the instance and points every cross-member connection back into the caller's own guest. A stack that looks wired up and talks only to itself.

  The correct shape, now implemented: `spec_disk::build` takes `peers: &[(String, Ipv4Addr)]` and resolves each alias against it. **An alias with no known address gets no line at all** — absence leaves the switch's resolver free to answer, and becomes correct by itself once Task 13 supplies the addresses; a placeholder never does. Task 13's only job here is to pass the resolved peers.

- **A pre-existing assertion in that test is vacuous under CI.** `assert!(!alone_in_its_network(true, &opts(2, Some("/proc/1/ns/net"))))` claims to pin "rootless with a namespace still loses it past one instance". Unprivileged, `stat("/proc/1/ns/net")` fails with `EACCES`, so `in_namespace` returns false and the `scale <= 1` guard is never consulted. A reviewer confirmed by mutation that the assertion cannot distinguish the real predicate from one with the scale guard deleted. Pointing it at `/proc/self/ns/net` — which the test process is genuinely in — makes the guard load-bearing. Not fixed in Task 1b, whose instruction was to change no assertions.

---

## File structure after this plan

```
ply-vm-proto/                       NEW workspace member — the host/guest wire contract
  Cargo.toml                        serde + serde_json only
  src/lib.rs                        SpecDisk, VolumeSpec, GuestLine, HostLine, spec-disk codec

ply-guest-init/                     NEW workspace member — PID 1 inside the guest (Linux-only body)
  Cargo.toml                        libc + ply-vm-proto
  src/main.rs                       boot sequence
  src/overlay.rs                    pure: lowerdir string, device ordering
  src/spec.rs                       pure: /etc/hosts + resolv.conf rendering, spec disk read
  src/control.rs                    hvc1 reader/writer
  src/volumes.rs                    mkfs/mount/chown/grow

ply-core/src/runtime/vm/
  mod.rs            NEW  VmBackend + VmInstance (cfg macos); pub portable submodules
  kernel.rs         NEW  MICROVM_KERNEL pin, PLY_MICROVM_KERNEL override, store fetch   (portable)
  spec_disk.rs      NEW  writes the PLYSPEC1 image from an InstanceSpec                 (portable)
  switch/
    mod.rs          NEW  Switch: per-stack L2, ARP, static IPs, DNS, NAT, port forward  (portable)
    frame.rs        NEW  the 4-byte BE length framing over SOCK_STREAM                  (portable)
    dns.rs          NEW  `<name>.ply` answers + upstream forwarding                     (portable)
  machine.rs        NEW  HVF vCPU, memory, DTB synthesis, boot + exit dispatch          (cfg macos)
  blk.rs            NEW  virtio-mmio block, N devices, file-backed, RO/RW               (cfg macos)
  console.rs        NEW  virtio-mmio console, 2 ports (hvc0 app, hvc1 control)          (cfg macos)
  net.rs            NEW  virtio-mmio net, bridged to the switch socket                  (cfg macos)
  pl011.rs          NEW  the early boot UART (kernel log only)                          (cfg macos)

ply-core/src/runtime/
  backend.rs        + Instance::connector(), NetworkFacts::same_network
  run.rs            + the VM stop step in the main loop; same_network from NetworkFacts
  publish.rs        Pool holds a Connector, not a bare SocketAddr
  state.rs          reap_stale stops calling hosts::remove_entry off Linux
  hosts.rs          + a non-Linux no-op arm

ply-core/src/{paths,stats,lifecycle}.rs   /proc readers gated; honest "unknown" off Linux
ply-cli/src/commands/{ps,reconcile}.rs    same
ply-cli/src/commands/up.rs                the switch replaces the stack netns on macOS
ply-cli/src/commands/lifecycle.rs         `ply check` with no image = the host capability report
ply-cli/src/cli.rs                        --vswitch (hidden), CheckArgs.image becomes optional
ply-cli/tests/macos_vm.rs                 NEW  the `make mac-test` integration suite

scripts/build-microvm-kernel.sh           NEW  kernel + initramfs + keg, runs on any Linux (Lima here)
scripts/mkinitramfs.py                    NEW  newc cpio with /dev/console AND /dev/null
kernel/microvm.config                     NEW  the tinyconfig delta
Makefile                                  + mac-test
.github/workflows/release.yml             + the darwin build/sign/notarize job
install.sh                                Darwin arm64 asset; Intel refused; PLY_FROM_SOURCE=1
docs/macos.md                             rewritten: native is the path, Lima the fallback
```

---

## Task 1: `ply-vm-proto` — the host/guest wire contract

The spec disk and the control channel are the only two things the host and the guest must agree on byte-for-byte. They live in one tiny crate so both sides share the definitions and one set of tests covers them, and that crate compiles everywhere so those tests run in Linux CI.

**Files:**
- Create: `ply-vm-proto/Cargo.toml`
- Create: `ply-vm-proto/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`, `[workspace.dependencies]`)

**Interfaces:**
- Consumes: nothing.
- Produces: `SpecDisk`, `VolumeSpec`, `GuestLine`, `HostLine`, `encode_spec_disk`, `decode_spec_disk`, `SPEC_MAGIC`. Used verbatim by Tasks 3, 6, 8 and 9.

- [ ] **Step 1: Create the crate**

`ply-vm-proto/Cargo.toml`:

```toml
[package]
name = "ply-vm-proto"
description = "The wire contract between ply's VM backend and its guest init: the spec disk and the control channel"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

# Deliberately tiny: this crate is linked into the static guest init, which
# lives in an initramfs with a size budget. serde_json is the whole cost.
[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
```

In the root `Cargo.toml`, add `"ply-vm-proto"` to `members` and this line to `[workspace.dependencies]`:

```toml
ply-vm-proto = { path = "ply-vm-proto" }
```

- [ ] **Step 2: Write the failing tests**

`ply-vm-proto/src/lib.rs`, tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SpecDisk {
        SpecDisk {
            entrypoint: vec!["/opt/db/bin/postgres".into(), "-D".into(), "/var/lib/pg".into()],
            workdir: "/opt/db".into(),
            user: Some(UserSpec { name: "postgres".into(), uid: 70, gid: 70 }),
            env: vec![("POSTGRES_PASSWORD".into(), "hunter2".into())],
            hostname: "db".into(),
            hosts: vec![("10.77.0.3".into(), "web.ply".into())],
            dns: Some("10.77.0.1".into()),
            volumes: vec![VolumeSpec { path: "/var/lib/pg".into(), dev: "/dev/vdc".into() }],
            params_seed: vec![("db".into(), vec![("state".into(), "starting".into())])],
            layer_count: 2,
        }
    }

    #[test]
    fn a_spec_disk_round_trips_through_its_own_bytes() {
        let image = encode_spec_disk(&sample()).unwrap();
        assert_eq!(&image[..8], SPEC_MAGIC, "the magic leads, so a scan can find this disk");
        let back = decode_spec_disk(&image).unwrap();
        assert_eq!(back.entrypoint, sample().entrypoint);
        assert_eq!(back.env, sample().env);
        assert_eq!(back.volumes[0].dev, "/dev/vdc");
    }

    #[test]
    fn the_image_is_padded_to_a_whole_number_of_sectors() {
        let image = encode_spec_disk(&sample()).unwrap();
        assert_eq!(image.len() % 512, 0, "virtio-blk hands the guest whole sectors");
    }

    #[test]
    fn a_disk_that_is_not_a_spec_disk_is_refused_not_guessed() {
        assert!(decode_spec_disk(b"hsqs\0\0\0\0nonsense").is_err());
        // Right magic, length longer than the buffer: still a refusal.
        let mut truncated = SPEC_MAGIC.to_vec();
        truncated.extend_from_slice(&9999u32.to_le_bytes());
        truncated.extend_from_slice(b"{}");
        assert!(decode_spec_disk(&truncated).is_err());
    }

    #[test]
    fn guest_lines_are_newline_delimited_json_in_both_directions() {
        let line = serde_json::to_string(&GuestLine::Ready).unwrap();
        assert!(!line.contains('\n'), "a control line must never contain its own delimiter");
        assert!(matches!(
            serde_json::from_str::<GuestLine>(r#"{"exit":3}"#).unwrap(),
            GuestLine::Exit { code: 3 }
        ));
        assert!(matches!(
            serde_json::from_str::<HostLine>(r#"{"signal":"TERM"}"#).unwrap(),
            HostLine::Signal { name } if name == "TERM"
        ));
    }

    #[test]
    fn an_unknown_line_is_ignored_not_fatal() {
        // Either side may be newer than the other; a line it does not know
        // must not end the instance.
        assert!(serde_json::from_str::<HostLine>(r#"{"future":{"x":1}}"#).is_err());
        assert!(parse_host_line(r#"{"future":{"x":1}}"#).is_none());
        assert!(parse_host_line(r#"{"signal":"TERM"}"#).is_some());
    }
}
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test -p ply-vm-proto`
Expected: FAIL — `cannot find type SpecDisk in this scope` and friends.

- [ ] **Step 4: Write the implementation**

Above the tests in `ply-vm-proto/src/lib.rs`:

```rust
//! The wire contract between ply's VM backend (host) and its guest init.
//!
//! Two things cross the machine boundary and nothing else does:
//!
//! 1. The **spec disk** — a raw block image the VMM writes at launch and the
//!    guest reads once: `PLYSPEC1`, a little-endian u32 length, then JSON.
//!    No filesystem, because the guest must be able to read it before it has
//!    mounted anything. Env arrives here and never on the kernel cmdline,
//!    which is world-readable inside the guest and length-limited.
//! 2. The **control channel** — newline-delimited JSON over virtio-console
//!    port 1, guest→host `GuestLine` and host→guest `HostLine`.
//!
//! Both sides link this crate, so neither can drift from the other. It is
//! deliberately dependency-thin: it ends up inside a static musl init in an
//! initramfs with a size budget.

use serde::{Deserialize, Serialize};

/// Leads the spec disk. The guest scans every attached disk for it rather
/// than trusting a device position (ruling R0-5).
pub const SPEC_MAGIC: &[u8; 8] = b"PLYSPEC1";

/// Sector size the padding rounds up to — virtio-blk exposes whole sectors,
/// and a trailing partial sector is simply invisible to the guest.
pub const SECTOR: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSpec {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

/// One volume: where it belongs inside the guest, and which block device
/// carries it. Named explicitly rather than positionally so adding a disk
/// anywhere in the order can never silently remount a volume elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSpec {
    pub path: String,
    pub dev: String,
}

/// Everything the guest needs to become the instance. Built by the VM
/// backend from `runtime::backend::InstanceSpec`; read once by the guest
/// init before it pivots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecDisk {
    pub entrypoint: Vec<String>,
    pub workdir: String,
    #[serde(default)]
    pub user: Option<UserSpec>,
    /// Fully composed on the host: manifest `[env]`, resolved params,
    /// secrets, `-e`, `HOME`, `TERM`, `PORT`. The guest adds nothing.
    pub env: Vec<(String, String)>,
    pub hostname: String,
    /// `(ip, name)` lines for `/etc/hosts` — the stack's `<name>.ply` peers.
    #[serde(default)]
    pub hosts: Vec<(String, String)>,
    /// The switch's resolver, for `/etc/resolv.conf`.
    #[serde(default)]
    pub dns: Option<String>,
    #[serde(default)]
    pub volumes: Vec<VolumeSpec>,
    /// Seed for `/run/ply`: `(app, [(key, value)])`, the facts the parent
    /// already published before launch.
    #[serde(default)]
    pub params_seed: Vec<(String, Vec<(String, String)>)>,
    /// How many of the leading disks are read-only image layers, in overlay
    /// order (top first). Everything after them is a volume or the spec disk.
    pub layer_count: usize,
}

/// Guest → host, one JSON object per line on `hvc1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GuestLine {
    /// `{"ready":true}` — the entrypoint has been exec'd.
    Ready,
    /// `{"exit":N}` — the entrypoint ended.
    Exit { code: i32 },
    /// `{"publish":{"key":"finish_boot","value":"ok"}}` — the app wrote
    /// `/run/ply/self/<key>`; forward it to the host's params tree.
    Publish { publish: Publish },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publish {
    pub key: String,
    pub value: String,
}

/// Host → guest, one JSON object per line on `hvc1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HostLine {
    /// `{"signal":"TERM"}` — forward this signal to the entrypoint.
    Signal { name: String },
    /// `{"params":{"<app>":{"<key>":"<value>"}}}` — a live params update to
    /// apply to the read-only peer nodes under `/run/ply`.
    Params { params: Vec<(String, Vec<(String, String)>)> },
}

/// `Ready` is `{"ready":true}` on the wire; the untagged enum needs the help.
impl Serialize for GuestLine { /* see Step 5 */ }
```

`serde(untagged)` cannot express `{"ready":true}` for a unit variant, so hand-write the two impls instead of fighting the derive. Replace the `#[serde(untagged)]` enums with plain structs and explicit conversions:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct GuestWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    ready: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    publish: Option<Publish>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct HostWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Vec<(String, Vec<(String, String)>)>>,
}

/// Render one guest→host line, newline included.
pub fn guest_line(line: &GuestLine) -> String {
    let wire = match line {
        GuestLine::Ready => GuestWire { ready: Some(true), ..Default::default() },
        GuestLine::Exit { code } => GuestWire { exit: Some(*code), ..Default::default() },
        GuestLine::Publish { publish } => {
            GuestWire { publish: Some(publish.clone()), ..Default::default() }
        }
    };
    let mut s = serde_json::to_string(&wire).expect("wire types serialize");
    s.push('\n');
    s
}

/// Parse one guest→host line. `None` for anything this build does not
/// understand: the two sides are versioned independently (the kernel keg is
/// pinned by the binary, but a user may override it), and an unknown line
/// must never end an instance.
pub fn parse_guest_line(text: &str) -> Option<GuestLine> {
    let wire: GuestWire = serde_json::from_str(text.trim()).ok()?;
    if wire.ready == Some(true) {
        return Some(GuestLine::Ready);
    }
    if let Some(code) = wire.exit {
        return Some(GuestLine::Exit { code });
    }
    wire.publish.map(|publish| GuestLine::Publish { publish })
}

/// Render one host→guest line, newline included.
pub fn host_line(line: &HostLine) -> String { /* mirror of guest_line */ }

/// Parse one host→guest line; `None` for anything unknown.
pub fn parse_host_line(text: &str) -> Option<HostLine> { /* mirror of parse_guest_line */ }
```

And the codec:

```rust
#[derive(Debug)]
pub enum SpecError {
    Magic,
    Truncated,
    Json(serde_json::Error),
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecError::Magic => write!(f, "not a ply spec disk (bad magic)"),
            SpecError::Truncated => write!(f, "spec disk truncated"),
            SpecError::Json(e) => write!(f, "spec disk JSON: {e}"),
        }
    }
}

impl std::error::Error for SpecError {}

/// `PLYSPEC1` + u32 LE length + JSON, zero-padded to a whole sector.
pub fn encode_spec_disk(spec: &SpecDisk) -> Result<Vec<u8>, SpecError> {
    let json = serde_json::to_vec(spec).map_err(SpecError::Json)?;
    let mut out = Vec::with_capacity(SPEC_MAGIC.len() + 4 + json.len() + SECTOR);
    out.extend_from_slice(SPEC_MAGIC);
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    let pad = (SECTOR - out.len() % SECTOR) % SECTOR;
    out.resize(out.len() + pad, 0);
    Ok(out)
}

pub fn decode_spec_disk(bytes: &[u8]) -> Result<SpecDisk, SpecError> {
    if bytes.len() < SPEC_MAGIC.len() + 4 || &bytes[..8] != SPEC_MAGIC {
        return Err(SpecError::Magic);
    }
    let len = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes")) as usize;
    let body = bytes.get(12..12 + len).ok_or(SpecError::Truncated)?;
    serde_json::from_slice(body).map_err(SpecError::Json)
}

/// Does this disk's head carry the spec-disk magic? The guest calls this on
/// every attached device (ruling R0-5).
pub fn is_spec_disk(head: &[u8]) -> bool {
    head.len() >= SPEC_MAGIC.len() && &head[..8] == SPEC_MAGIC
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p ply-vm-proto`
Expected: PASS, 5 tests.

- [ ] **Step 6: Both gates**

Run: `make check`
Expected: green, with 5 more tests than before.
Run: `cargo check -p ply-cli && cargo clippy -p ply-cli -- -D warnings`
Expected: clean.

- [ ] **Step 7: Stop and report.**

---

## Task 1b: make `cargo test -p ply-core` compile on macOS

Plan 1 gated `runtime::ns` itself but not the **test** code that reaches into it, so `cargo test --workspace` cannot build on a Mac at all — four errors, in two `#[cfg(test)]` modules. Tasks 7, 12 and 14 all add portable tests to `ply-core` and all say "run them on the Mac"; none of them can until this is fixed. It is a fifteen-minute job and it unblocks every later task's local verification.

**Files:**
- Modify: `ply-core/src/runtime/publish.rs:391-441` (the test `a_namespace_backend_is_reachable_only_from_inside`)
- Modify: `ply-core/src/runtime/run.rs:1903-1908` (`mod discovery_tests`' module-level import) and its test at ~`run.rs:1961`

**Interfaces:**
- Consumes: nothing.
- Produces: a `ply-core` whose portable tests run on both platforms. Every later task's "run it on the Mac" step depends on it.

- [ ] **Step 1: See the failure**

Run: `cargo test --workspace --no-run 2>&1 | grep -E '^\s+-->' | sort -u`
Expected, exactly these four locations:

```
ply-core/src/runtime/mod.rs:10:9
ply-core/src/runtime/publish.rs:392:29
ply-core/src/runtime/publish.rs:408:29
ply-core/src/runtime/publish.rs:428:29
ply-core/src/runtime/run.rs:1908:25
```

- [ ] **Step 2: Gate the one test that genuinely needs a namespace**

`a_namespace_backend_is_reachable_only_from_inside` creates a `NetNs` and enters it. That is a Linux test of a Linux property; it cannot be made portable and should not be. Put `#[cfg(target_os = "linux")]` on it, directly under its `#[test]`, with a comment saying what it is asserting and why it is platform-bound:

```rust
    #[test]
    // A namespace backend's address means nothing outside its namespace —
    // the property that makes `serve`'s self-connection guard necessary.
    // Linux-only by nature: there is no namespace to enter anywhere else.
    #[cfg(target_os = "linux")]
    fn a_namespace_backend_is_reachable_only_from_inside() {
```

- [ ] **Step 3: Narrow the `run.rs` import to the one test that uses it**

`mod discovery_tests` imports `crate::runtime::ns::alone_in_its_network` at module level, but only one of its tests uses it — the rest parse env files and publish specs and are perfectly portable. Delete the module-level `use` (and the comment above it), and put the import inside the single test that needs it, gated:

```rust
    #[test]
    // `alone_in_its_network` is the namespace backend's own rule about when
    // an instance may keep its declared port. Linux-only, like its subject.
    #[cfg(target_os = "linux")]
    fn alone_in_its_network_matches_when_port_is_injected() {
        use crate::runtime::ns::alone_in_its_network;
        …
    }
```

- [ ] **Step 4: Verify on the Mac**

Run: `cargo test -p ply-core --lib 2>&1 | tail -5`
Expected: it compiles and runs. Record the pass count.

Run: `cargo test -p ply-vm-proto && cargo check -p ply-cli && cargo clippy -p ply-cli -- -D warnings`
Expected: all clean.

- [ ] **Step 5: Verify Linux is untouched**

Run the Lima gate from Global Constraints.
Expected: `make check` green, and the ply-core test count **unchanged** — the two gated tests still run on Linux, which is the whole point of gating rather than deleting them.

- [ ] **Step 6: Stop and report** — both pass counts.

Do **not** expect the raw counts to differ by two. Plan 1 gated the whole `runtime::ns` module, so its ~33 tests already vanish on macOS; the real difference is that plus the two gated here. Prove it by name rather than by arithmetic:

```sh
cargo test -p ply-core --lib -- --list > /tmp/mac.txt
lima bash -lc 'export PATH=$HOME/.cargo/bin:$PATH; export CARGO_TARGET_DIR=$HOME/ply-target; cd /Users/iluxav/Documents/ply && cargo test -p ply-core --lib -- --list' > /tmp/linux.txt
diff /tmp/mac.txt /tmp/linux.txt
```

Every name only on the Linux side must be either `runtime::ns::*` or one of the two gated here, and no name may be only on the macOS side. Then run the same list on Linux against `git show HEAD:` copies of both edited files and confirm it is byte-identical to the list with your edits — that, not a count, is what proves the Linux set did not move.

---

## Task 2: the microVM kernel — config, build script, initramfs packer

The kernel is part of the runtime, like the binary, not part of any app. This task produces the config delta and the script that builds it; the keg it eventually becomes is assembled in Task 5, and pushing that keg to the registry is the owner's.

The spike found two missing options and one missing device node (ruling R0-4). All three are fixed here.

**Files:**
- Create: `kernel/microvm.config`
- Create: `scripts/mkinitramfs.py`
- Create: `scripts/build-microvm-kernel.sh`

**Interfaces:**
- Consumes: nothing in the repo. Reads `/Users/iluxav/Documents/plyvm/kernel/plyvm.config` as the starting point.
- Produces: `microvm-kernel-<kver>-linux-arm64.img` (a raw arm64 `Image`) and `initramfs.cpio`, both consumed by Tasks 4, 5 and 8.

- [ ] **Step 1: Write the config delta**

Copy `/Users/iluxav/Documents/plyvm/kernel/plyvm.config` to `kernel/microvm.config`, then add the block below. Everything else stays as plyvm had it — that config is known to boot on this hardware.

```
# --- ply additions over the plyvm spike config ---------------------------
# Volumes are ext4 disks (spec: "Volumes as disks, not shared directories").
# The spike found this missing: without it `mkfs.ext4` in the guest init
# succeeds and the mount then fails with ENODEV.
CONFIG_EXT4_FS=y
CONFIG_EXT4_USE_FOR_EXT2=y

# The control channel is virtio-console port 1 (`hvc1`); the app's stdout is
# port 0 (`hvc0`). HVF has no vsock device, so a second console is how the
# host and guest talk (spec: "Why a control console instead of virtio-vsock").
CONFIG_VIRTIO_CONSOLE=y
CONFIG_HVC_DRIVER=y

# The overlay is assembled from N read-only squashfs layers; the spike's
# config already has SQUASHFS + OVERLAY_FS. Zstd is what `ply build` writes.
CONFIG_SQUASHFS_ZSTD=y
```

Keep `CONFIG_SERIAL_AMBA_PL011=y` and `CONFIG_SERIAL_AMBA_PL011_CONSOLE=y`: the PL011 stays the kernel's own console (`ttyAMA0`) so a boot failure is visible before any virtio device exists. The app's stdout is `hvc0`, which is a different thing.

- [ ] **Step 2: Write the initramfs packer**

`scripts/mkinitramfs.py` — start from `/Users/iluxav/Documents/plyvm/mkinitramfs.py` (35 lines, newc cpio by hand) and change exactly three things: `/dev/null`, the e2fsprogs binaries, and a `--extra` flag.

```python
#!/usr/bin/env python3
"""Build the microVM initramfs: a newc cpio holding the guest init, the
device nodes it needs, and static e2fsprogs for volume formatting.

Written by hand rather than with cpio(1) because the archive must contain
CHARACTER DEVICE NODES, which no unprivileged tar or cpio can create.

Usage: mkinitramfs.py <init-binary> <out.cpio> [--extra NAME=PATH ...]
"""
import sys, struct, os

def entry(name, mode, filedata=b"", dev=(0, 0), ino=[100]):
    ino[0] += 1
    fields = [ino[0], mode, 0, 0, 1, 0, len(filedata), 0, 0, dev[0], dev[1], len(name) + 1, 0]
    out = b"070701" + b"".join(f"{f:08X}".encode() for f in fields)
    out += name.encode() + b"\0"
    out += b"\0" * ((4 - (len(out) % 4)) % 4)
    out += filedata
    out += b"\0" * ((4 - (len(filedata) % 4)) % 4)
    return out

args = sys.argv[1:]
init_path, out_path = args[0], args[1]
extras = {}
i = 2
while i < len(args):
    if args[i] == "--extra":
        name, path = args[i + 1].split("=", 1)
        extras[name] = path
        i += 2
    else:
        sys.exit(f"unexpected argument {args[i]!r}")

out = b""
out += entry("dev", 0o040755)
out += entry("dev/console", 0o020600, dev=(5, 1))
# /dev/null is NOT optional. The kernel starts init with no open fds when it
# could not open /dev/console, and Rust's std then opens /dev/null for the
# missing standard descriptors and ABORTS if it cannot — the guest dies
# before main() with a bare "Attempted to kill init!". Cost an hour in the
# milestone 0 spike; see the spike result document.
out += entry("dev/null", 0o020666, dev=(1, 3))
out += entry("proc", 0o040755)
out += entry("sys", 0o040755)
out += entry("run", 0o040755)
out += entry("sbin", 0o040755)
for name, path in sorted(extras.items()):
    with open(path, "rb") as f:
        out += entry(f"sbin/{name}", 0o100755, f.read())
with open(init_path, "rb") as f:
    out += entry("init", 0o100755, f.read())
out += entry("TRAILER!!!", 0)

with open(out_path, "wb") as f:
    f.write(out)
print(f"initramfs: {len(out)} bytes -> {out_path}")
```

- [ ] **Step 3: Write the build script**

`scripts/build-microvm-kernel.sh`. It must run on any aarch64 Linux; on this Mac that is Lima (`limactl start default`, then `lima bash scripts/build-microvm-kernel.sh`). The `default` VM already exists and is the one that built the spike's kernel.

```sh
#!/bin/sh
# Build the ply microVM kernel and its initramfs.
#
# Runs on any aarch64 Linux. On a Mac, inside Lima:
#     limactl start default
#     lima bash scripts/build-microvm-kernel.sh
#
# Produces, in out/:
#   microvm-kernel-<kver>-linux-arm64.img   raw arm64 Image, ~4.5 MiB
#   initramfs.cpio                          guest init + e2fsprogs + dev nodes
#
# The kernel is part of the RUNTIME, not of any app: the ply binary pins one
# version in runtime/vm/kernel.rs and no lockfile ever mentions it.
set -eu

KVER="${KVER:-6.12.0}"
OUT="${OUT:-out}"
JOBS="${JOBS:-$(nproc)}"

test "$(uname -s)" = Linux || { echo "error: run this on Linux (on a Mac: lima bash $0)"; exit 1; }
test "$(uname -m)" = aarch64 || { echo "error: arm64 only (the guest is linux-arm64)"; exit 1; }

mkdir -p "$OUT"
here=$(cd "$(dirname "$0")/.." && pwd)

# --- the guest init: one static arm64 binary, no runtime deps -------------
# rustc's own bundled lld links musl targets, so this needs no cross
# toolchain and no Homebrew package. Set the linker by ENV, never in a
# .cargo/config.toml: a config file would apply to the whole workspace and
# would break the release lane's `cross build` for the same target.
rustup target add aarch64-unknown-linux-musl >/dev/null
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release --target aarch64-unknown-linux-musl -p ply-guest-init
init="$here/target/aarch64-unknown-linux-musl/release/ply-guest-init"

# --- static e2fsprogs: the guest formats and grows its own volumes --------
# Alpine's static build is the smallest source of a working mke2fs; ply's own
# registry is built from the same Alpine packages.
e2fs="$OUT/e2fsprogs"
if [ ! -x "$e2fs/sbin/mke2fs" ]; then
    mkdir -p "$e2fs"
    apk_url="https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/aarch64"
    apk=$(curl -fsSL "$apk_url/" | sed -n 's/.*\(e2fsprogs-static-[0-9][^"]*\.apk\).*/\1/p' | head -1)
    test -n "$apk" || { echo "error: no e2fsprogs-static in $apk_url"; exit 1; }
    curl -fsSL "$apk_url/$apk" | tar xz -C "$e2fs"
fi

python3 "$here/scripts/mkinitramfs.py" "$init" "$OUT/initramfs.cpio" \
    --extra "mke2fs=$e2fs/sbin/mke2fs.static" \
    --extra "resize2fs=$e2fs/sbin/resize2fs.static"

# --- the kernel -----------------------------------------------------------
src="$OUT/linux-$KVER"
if [ ! -d "$src" ]; then
    curl -fsSL "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KVER.tar.xz" \
        | tar xJ -C "$OUT"
fi
make -C "$src" O=build ARCH=arm64 tinyconfig
cat "$here/kernel/microvm.config" >> "$src/build/.config"
make -C "$src" O=build ARCH=arm64 olddefconfig
make -C "$src" O=build ARCH=arm64 -j"$JOBS" Image

img="$OUT/microvm-kernel-$KVER-linux-arm64.img"
cp "$src/build/arch/arm64/boot/Image" "$img"
echo "kernel:    $(stat -c%s "$img") bytes -> $img"

# Every option the guest init and the VMM depend on, asserted here rather
# than discovered as a mount failure inside a VM that cannot report it.
for opt in CONFIG_VIRTIO_MMIO CONFIG_VIRTIO_BLK CONFIG_VIRTIO_NET \
           CONFIG_VIRTIO_CONSOLE CONFIG_SQUASHFS CONFIG_SQUASHFS_ZSTD \
           CONFIG_OVERLAY_FS CONFIG_EXT4_FS CONFIG_SERIAL_AMBA_PL011_CONSOLE \
           CONFIG_BLK_DEV_INITRD CONFIG_DEVTMPFS; do
    grep -q "^$opt=y" "$src/build/.config" \
        || { echo "error: $opt is not set in the built kernel"; exit 1; }
done
echo "config:    all required options present"
```

`chmod +x scripts/build-microvm-kernel.sh`.

- [ ] **Step 4: Build it**

The script needs `ply-guest-init` (Task 3). Until that exists, verify only the parts that stand alone:

Run: `python3 scripts/mkinitramfs.py /bin/true /tmp/t.cpio && cpio -itv < /tmp/t.cpio`
Expected: the listing contains `dev/console`, `dev/null`, `init`, and the `dev/*` entries are character devices (`crw-------` and `crw-rw-rw-`).

Run: `sh -n scripts/build-microvm-kernel.sh`
Expected: no output (syntax clean).

- [ ] **Step 5: Both gates, then stop and report.**

Run: `make check` and `cargo check -p ply-cli`
Expected: both unchanged and green — this task adds no Rust.

---

## Task 3: `ply-guest-init` — the pure half

PID 1 inside the guest. This task lands the crate and everything in it that is a pure function, with tests that run in Linux CI. Task 6 adds the syscall half once there is a VMM to boot it.

**Files:**
- Create: `ply-guest-init/Cargo.toml`, `src/main.rs`, `src/overlay.rs`, `src/spec.rs`
- Modify: root `Cargo.toml` (`members`)

**Interfaces:**
- Consumes: `ply-vm-proto` (Task 1).
- Produces: `overlay::lowerdir`, `overlay::device_name`, `spec::hosts_file`, `spec::resolv_conf`, `spec::find_spec_disk` — all used by Task 6's boot sequence.

- [ ] **Step 1: Create the crate**

`ply-guest-init/Cargo.toml`:

```toml
[package]
name = "ply-guest-init"
description = "PID 1 inside a ply microVM: assembles the overlay, reads the spec disk, runs the app"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "ply-guest-init"
path = "src/main.rs"

# libc for the syscalls the kernel expects init to make; ply-vm-proto for the
# spec disk and the control channel. Nothing else: this binary ships inside an
# initramfs the VM loads on every single start.
[dependencies]
libc = "0.2"
ply-vm-proto.workspace = true
```

Add `"ply-guest-init"` to the workspace `members`, and `libc = "0.2"` to `[workspace.dependencies]` if it is not already there (check first: `grep '^libc' Cargo.toml`).

This crate's body is Linux-only. That is fine: `make check` runs on Linux, and the darwin gate is `-p ply-cli`, which never builds it.

- [ ] **Step 2: Write the failing tests**

`ply-guest-init/src/overlay.rs`:

```rust
//! Turning the disks the VMM attached into the root filesystem, expressed
//! as pure functions so Linux CI tests them without a VM.

/// `/dev/vda`, `/dev/vdb`, … `/dev/vdz`, `/dev/vdaa`. Linux names virtio
/// disks in probe order, and the VMM's device tree fixes that order, so
/// index 0 is always the top image layer.
pub fn device_name(index: usize) -> String { unimplemented!() }

/// overlayfs `lowerdir=` for `count` layers mounted under `mounts/<i>`.
///
/// overlayfs takes lowers TOP FIRST, colon-separated — the same order
/// `InstanceSpec.images` uses and the same order `runtime/ns/mount.rs`
/// builds on Linux, so one lockfile means one filesystem on both backends.
pub fn lowerdir(mount_root: &str, count: usize) -> String { unimplemented!() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disks_are_named_the_way_linux_names_them() {
        assert_eq!(device_name(0), "/dev/vda");
        assert_eq!(device_name(1), "/dev/vdb");
        assert_eq!(device_name(25), "/dev/vdz");
        assert_eq!(device_name(26), "/dev/vdaa");
        assert_eq!(device_name(27), "/dev/vdab");
    }

    #[test]
    fn the_overlay_stacks_the_app_image_on_top_of_its_packages() {
        // The same order the Linux backend uses: images[0] is the app.
        assert_eq!(lowerdir("/mnt", 3), "lowerdir=/mnt/0:/mnt/1:/mnt/2");
    }

    #[test]
    fn a_single_layer_still_makes_a_valid_lowerdir() {
        assert_eq!(lowerdir("/mnt", 1), "lowerdir=/mnt/0");
    }
}
```

`ply-guest-init/src/spec.rs`:

```rust
//! Reading the spec disk and rendering the small config files it implies.

use ply_vm_proto::SpecDisk;

/// `/etc/hosts` for this instance: loopback, its own name, then the peers
/// the parent resolved. Identical in content to what `runtime/hosts.rs`
/// writes on the host for a namespace instance.
pub fn hosts_file(spec: &SpecDisk) -> String { unimplemented!() }

/// `/etc/resolv.conf`, or `None` when the run has no resolver (a standalone
/// `ply run` with no switch) — in which case the guest keeps whatever the
/// image shipped.
pub fn resolv_conf(spec: &SpecDisk) -> Option<String> { unimplemented!() }

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SpecDisk {
        SpecDisk {
            entrypoint: vec!["/bin/true".into()],
            workdir: "/".into(),
            user: None,
            env: vec![],
            hostname: "db".into(),
            hosts: vec![("10.77.0.3".into(), "web.ply".into())],
            dns: Some("10.77.0.1".into()),
            volumes: vec![],
            params_seed: vec![],
            layer_count: 1,
        }
    }

    #[test]
    fn hosts_carries_loopback_the_instances_own_name_and_its_peers() {
        let text = hosts_file(&spec());
        assert!(text.contains("127.0.0.1\tlocalhost"));
        assert!(text.contains("127.0.0.1\tdb"), "an app resolving its own hostname must not fail");
        assert!(text.contains("10.77.0.3\tweb.ply"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn no_resolver_means_the_image_keeps_its_own() {
        let mut s = spec();
        s.dns = None;
        assert_eq!(resolv_conf(&s), None);
        assert_eq!(resolv_conf(&spec()).as_deref(), Some("nameserver 10.77.0.1\n"));
    }
}
```

`ply-guest-init/src/main.rs` for now:

```rust
//! PID 1 inside a ply microVM. Task 6 fills in the boot sequence; this file
//! exists now so the crate has a binary target and its pure modules are
//! compiled and tested by Linux CI from the first task that touches them.

mod overlay;
mod spec;

fn main() {
    eprintln!("ply-guest-init: boot sequence lands in Task 6");
    std::process::exit(1);
}
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test -p ply-guest-init`
Expected: FAIL — every test panics at `unimplemented!()`.

- [ ] **Step 4: Implement**

```rust
pub fn device_name(index: usize) -> String {
    // Linux's own scheme (sd_format_disk_name): a..z, then aa..zz, base-26
    // with no zero digit.
    let mut suffix = Vec::new();
    let mut n = index as i64;
    while n >= 0 {
        suffix.push(b'a' + (n % 26) as u8);
        n = n / 26 - 1;
    }
    suffix.reverse();
    format!("/dev/vd{}", String::from_utf8(suffix).expect("ascii"))
}

pub fn lowerdir(mount_root: &str, count: usize) -> String {
    let lowers: Vec<String> = (0..count).map(|i| format!("{mount_root}/{i}")).collect();
    format!("lowerdir={}", lowers.join(":"))
}
```

```rust
pub fn hosts_file(spec: &SpecDisk) -> String {
    let mut out = String::from("127.0.0.1\tlocalhost\n");
    // An app that resolves its own hostname must get an answer; the Linux
    // backend gets this from the host's /etc/hosts bind.
    out.push_str(&format!("127.0.0.1\t{}\n", spec.hostname));
    out.push_str("::1\tlocalhost ip6-localhost ip6-loopback\n");
    for (ip, name) in &spec.hosts {
        out.push_str(&format!("{ip}\t{name}\n"));
    }
    out
}

pub fn resolv_conf(spec: &SpecDisk) -> Option<String> {
    spec.dns.as_ref().map(|ip| format!("nameserver {ip}\n"))
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p ply-guest-init`
Expected: PASS, 5 tests.

- [ ] **Step 6: Both gates**

Run: `make check`
Expected: green. `cargo clippy --workspace` now lints `ply-guest-init` too.
Run: `cargo check -p ply-cli`
Expected: clean — the darwin gate never builds this crate.

- [ ] **Step 7: Stop and report.**

---

## Task 4: the VM backend skeleton and the kernel pin

Wire a `VmBackend` into `default_backend()` on macOS that refuses honestly and reports its capability. Nothing boots yet; the point is that from here on the Mac has a real backend to grow, and `ply run` on a Mac stops saying "not available on this platform".

**Files:**
- Create: `ply-core/src/runtime/vm/mod.rs`, `ply-core/src/runtime/vm/kernel.rs`
- Modify: `ply-core/src/runtime/mod.rs`, `ply-core/src/runtime/backend.rs:160-173`, `ply-core/Cargo.toml`

**Interfaces:**
- Consumes: `Backend`, `Facts`, `NetworkFacts` (plan 1's `backend.rs`).
- Produces: `VmBackend::new()`, `vm::kernel::resolve()`, `vm::capability_report()`. Tasks 8, 15 build on them.

- [ ] **Step 1: Add the macOS dependencies**

In `Cargo.toml`'s `[workspace.dependencies]`:

```toml
# The macOS VM backend. `applevisor` is a safe wrapper over
# Hypervisor.framework; `vm-fdt` writes the device tree the kernel boots on.
# Both are macOS-only and gated in ply-core's Cargo.toml.
applevisor = { version = "1", features = ["macos-15-0"] }
vm-fdt = "0.3"
```

In `ply-core/Cargo.toml`, after the existing Linux-only block:

```toml
# macOS-only: Hypervisor.framework and the device tree writer. Everything
# that uses them lives under runtime/vm/ behind the same cfg; the portable
# half of that module (the spec disk, the switch, the kernel pin) has no
# macOS dependency and is compiled and tested everywhere.
[target.'cfg(target_os = "macos")'.dependencies]
applevisor.workspace = true
vm-fdt.workspace = true
```

And add `ply-vm-proto.workspace = true` to `[dependencies]`.

- [ ] **Step 2: Write the failing tests**

`ply-core/src/runtime/vm/kernel.rs`:

```rust
//! Which kernel a microVM boots.
//!
//! The kernel is part of the RUNTIME, like the ply binary itself, not part
//! of any app: the version is a constant here, `ply self-update` brings a
//! new pin with a new binary, and no `ply.lock` ever mentions it — a
//! lockfile written on a Mac must stay byte-identical to one written on
//! Linux.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// The keg this build boots. Bump it with the binary, never per app.
pub const MICROVM_KERNEL: &str = "ply/microvm-kernel@6.12.0";

/// Escape hatch for kernel development: a path to a raw arm64 `Image`, or a
/// registry ref to fetch instead of the pin.
pub const KERNEL_OVERRIDE_ENV: &str = "PLY_MICROVM_KERNEL";

/// Where the kernel and its initramfs came from, for `ply check` and errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kernel {
    pub image: PathBuf,
    pub initramfs: PathBuf,
    /// What to tell the user this is: the pin, or the override that beat it.
    pub origin: String,
}

/// Split `PLY_MICROVM_KERNEL` into (kernel image, initramfs) when it names a
/// directory or a bare image, or `None` when it names a registry ref.
pub fn override_paths(value: &str) -> Option<(PathBuf, PathBuf)> { unimplemented!() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pin_is_a_registry_ref_with_an_exact_version() {
        let (name, version) = MICROVM_KERNEL.split_once('@').expect("pin carries a version");
        assert_eq!(name, "ply/microvm-kernel");
        assert!(
            semver::Version::parse(version).is_ok(),
            "the pin must be exact — a range would make two Macs boot two kernels"
        );
    }

    #[test]
    fn a_directory_override_supplies_both_files() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("microvm-kernel.img"), b"x").unwrap();
        std::fs::write(td.path().join("initramfs.cpio"), b"y").unwrap();
        let (img, initrd) = override_paths(td.path().to_str().unwrap()).unwrap();
        assert!(img.ends_with("microvm-kernel.img"));
        assert!(initrd.ends_with("initramfs.cpio"));
    }

    #[test]
    fn an_image_override_takes_its_initramfs_from_the_same_directory() {
        let td = tempfile::tempdir().unwrap();
        let img = td.path().join("custom.img");
        std::fs::write(&img, b"x").unwrap();
        std::fs::write(td.path().join("initramfs.cpio"), b"y").unwrap();
        let (got, initrd) = override_paths(img.to_str().unwrap()).unwrap();
        assert_eq!(got, img);
        assert_eq!(initrd, td.path().join("initramfs.cpio"));
    }

    #[test]
    fn a_registry_ref_is_not_a_path() {
        assert!(override_paths("ply/microvm-kernel@6.12.1").is_none());
    }
}
```

- [ ] **Step 3: Run and watch fail**

Run: `cargo test -p ply-core kernel::tests`
Expected: FAIL — the module does not exist yet.

- [ ] **Step 4: Implement `kernel.rs`**

```rust
pub fn override_paths(value: &str) -> Option<(PathBuf, PathBuf)> {
    let path = PathBuf::from(value);
    if path.is_dir() {
        return Some((path.join("microvm-kernel.img"), path.join("initramfs.cpio")));
    }
    if path.is_file() {
        let dir = path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
        return Some((path, dir.join("initramfs.cpio")));
    }
    None // a registry ref, or a path that is not there — resolve() says which
}

/// The kernel this run boots: the override if there is one, else the pin,
/// fetched into the store on first use like any other keg.
pub fn resolve() -> Result<Kernel> {
    if let Some(value) = std::env::var_os(KERNEL_OVERRIDE_ENV) {
        let value = value.to_string_lossy().into_owned();
        if let Some((image, initramfs)) = override_paths(&value) {
            if !initramfs.exists() {
                return Err(Error::Runtime(format!(
                    "{KERNEL_OVERRIDE_ENV}={value}: no initramfs.cpio beside the kernel image"
                )));
            }
            return Ok(Kernel { image, initramfs, origin: format!("{KERNEL_OVERRIDE_ENV}={value}") });
        }
        return fetch_keg(&value).map(|mut k| {
            k.origin = format!("{KERNEL_OVERRIDE_ENV}={value}");
            k
        });
    }
    fetch_keg(MICROVM_KERNEL)
}

/// Fetch `<namespace>/<name>@<version>` into the store and return the two
/// files inside it. The keg is an ordinary ply image; its payload is the
/// kernel and the initramfs at fixed paths.
fn fetch_keg(reference: &str) -> Result<Kernel> {
    let (name, want) = match reference.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (reference, None),
    };
    let (image, resolved, _digest) =
        crate::catalog::fetch_app_image(name, want, crate::catalog::OFFICIAL_RUN_SOURCE)?;
    // The keg is a squashfs image like any other; the two files live at
    // /opt/microvm-kernel/. Extract once into the store, then reuse.
    let digest = crate::digest::sha256_file(&image)?;
    let root = crate::store::Store::open_default()?.extracted_rootfs(&image, &digest)?;
    let dir = root.join("opt/microvm-kernel");
    let kernel = Kernel {
        image: dir.join("microvm-kernel.img"),
        initramfs: dir.join("initramfs.cpio"),
        origin: resolved.to_string(),
    };
    for path in [&kernel.image, &kernel.initramfs] {
        if !path.exists() {
            return Err(Error::Runtime(format!(
                "{reference}: the kernel keg has no {} — rebuild it with scripts/build-microvm-kernel.sh",
                path.display()
            )));
        }
    }
    Ok(kernel)
}
```

`Store::extracted_rootfs` (`ply-core/src/store.rs:52`) is already portable — it extracts with `backhand` through `image::extract::extract_rootfs` and touches no Linux API, so it needs no change. It does print `ply: extracting <digest> (rootless runs use extracted layers)`, which is Linux vocabulary on a Mac. Reword it to `(extracting layers)` and check no test asserts the old text (`grep -rn "rootless runs use extracted" ply-core ply-cli`).

- [ ] **Step 5: Write the backend skeleton**

`ply-core/src/runtime/vm/mod.rs`:

```rust
//! The macOS backend: one instance = one microVM on Hypervisor.framework,
//! its disks the same `.img` files the namespace backend overlays, its
//! network a userspace switch the `ply run`/`ply up` parent owns.
//!
//! Module layout is deliberate. `kernel`, `spec_disk` and `switch` are
//! PORTABLE — they are pure logic (a version pin, a byte format, a TCP/IP
//! stack) and they compile and run their tests on Linux CI, which is where
//! this project's tests actually run. Only the four device models and the
//! vCPU loop touch Hypervisor.framework, and only those are gated.

pub mod kernel;
pub mod spec_disk;
pub mod switch;

#[cfg(target_os = "macos")]
mod blk;
#[cfg(target_os = "macos")]
mod console;
#[cfg(target_os = "macos")]
mod machine;
#[cfg(target_os = "macos")]
mod net;
#[cfg(target_os = "macos")]
mod pl011;

/// Can this host run microVMs, and if not, exactly why? The message is what
/// `ply check` prints and what `ply run` fails with, so it names the remedy.
#[cfg(target_os = "macos")]
pub fn capability_report() -> std::result::Result<String, String> {
    // Apple Silicon: Hypervisor.framework on Intel is a different device
    // model and is a declared non-goal.
    if std::env::consts::ARCH != "aarch64" {
        return Err("ply's microVM runtime needs an Apple Silicon Mac (M1 or later)".into());
    }
    // The kernel's own answer, rather than a version comparison: this is the
    // bit that says whether the CPU and the OS will actually let us in.
    let supported = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.hv_support"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false);
    if !supported {
        return Err(
            "Hypervisor.framework is unavailable on this machine (kern.hv_support = 0) — \
             virtualization may be disabled, or this Mac is not supported"
                .into(),
        );
    }
    // The binary must carry com.apple.security.hypervisor or hv_vm_create
    // fails at launch with a permission error nobody can read.
    match machine::probe_hypervisor() {
        Ok(()) => Ok("Hypervisor.framework ok".into()),
        Err(e) => Err(format!(
            "Hypervisor.framework refused this process ({e}) — the ply binary must be signed \
             with the com.apple.security.hypervisor entitlement; reinstall with \
             `curl -fsSL https://plybox.sh/install.sh | sh`"
        )),
    }
}

#[cfg(target_os = "macos")]
pub struct VmBackend {
    kernel: kernel::Kernel,
}

#[cfg(target_os = "macos")]
impl VmBackend {
    pub fn new() -> crate::Result<VmBackend> {
        Ok(VmBackend { kernel: kernel::resolve()? })
    }
}
```

Implement `Backend for VmBackend` with the trait's seven methods. In this task, five of them are final and two are stubs:

```rust
impl crate::runtime::backend::Backend for VmBackend {
    fn capability(&self) -> std::result::Result<(), String> {
        capability_report().map(|_| ())
    }

    fn preflight(&self, opts: RunOptions) -> Result<RunOptions> {
        eprintln!("ply: microVM runtime ({}) — {} MiB per instance", self.kernel.origin, MEMORY_MIB);
        Ok(opts)
    }

    fn facts(&self) -> Facts {
        // Every instance is its own machine with its own address on the
        // switch, so `{host}`/`{addr}`/`{base_url}` resolve — like a rootful
        // namespace run, not a rootless one. Published listeners bind
        // loopback: on a Mac there is no bridge gateway to bind instead.
        Facts { loopback: true, own_addresses: true }
    }

    fn admit(&self, manifest: &Manifest, _opts: &RunOptions) -> Result<()> {
        // Two features the VM backend does not have. Refusing beats silently
        // running something different from what the manifest asked for.
        if manifest.resources.is_some() {
            eprintln!(
                "ply: warning: [resources] limits are ignored by the microVM runtime \
                 (each instance gets a fixed {MEMORY_MIB} MiB)"
            );
        }
        Ok(())
    }

    fn attach(&self, _opts: &RunOptions) -> Result<()> { Ok(()) }  // Task 13

    fn network(&self, _opts: &RunOptions) -> NetworkFacts {
        NetworkFacts { facts: self.facts(), in_stack_network: false, alone: true, same_network: false }
    }

    fn launch(&self, _spec: &InstanceSpec, _record: Record<'_>) -> Result<Launched> {
        Err(Error::Runtime(
            "the microVM runtime cannot launch instances yet — this build is mid-plan".into(),
        ))
    }

    fn terminal(&self, _app: &str, _slot: u32, _nonce: &str) -> Result<()> {
        Err(Error::Runtime(
            "`ply exec` into a microVM is not available yet (a virtio-console shell channel is v2)"
                .into(),
        ))
    }
}
```

`MEMORY_MIB` is `const MEMORY_MIB: u64 = 512;` per the spec.

Then in `ply-core/src/runtime/mod.rs`, add:

```rust
/// The macOS backend and everything only it may use: Hypervisor.framework,
/// the device models, the guest contract, the userspace switch. Its portable
/// submodules compile everywhere so Linux CI tests them.
pub mod vm;
```

and in `backend.rs`, replace the `#[cfg(not(target_os = "linux"))] default_backend`:

```rust
#[cfg(target_os = "macos")]
pub fn default_backend() -> Result<Box<dyn Backend>> {
    Ok(Box::new(crate::runtime::vm::VmBackend::new()?))
}

/// Neither Linux namespaces nor macOS microVMs: nothing to run instances with.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn default_backend() -> Result<Box<dyn Backend>> {
    Err(crate::Error::Runtime(
        "ply run has no runtime on this platform — Linux (namespaces) and macOS on Apple Silicon (microVMs) are supported".into(),
    ))
}
```

`machine::probe_hypervisor()` for now is the minimum that answers the question:

```rust
/// Create and immediately destroy a VM: the only honest test of whether this
/// process may use Hypervisor.framework (the entitlement is checked at
/// `hv_vm_create`, not at load).
pub fn probe_hypervisor() -> std::result::Result<(), String> {
    use applevisor::prelude::*;
    VirtualMachine::new(VirtualMachineConfig::new())
        .map(|_| ())
        .map_err(|e| e.to_string())
}
```

- [ ] **Step 6: Run the tests and both gates**

Run: `cargo test -p ply-core kernel::`
Expected: PASS, 4 tests.
Run: `make check`
Expected: green — Linux compiles `vm::{kernel, spec_disk, switch}` and skips the gated modules. (`spec_disk` and `switch` are empty `mod.rs`-only stubs at this point; create them with a doc comment and nothing else.)
Run: `cargo check -p ply-cli && cargo clippy -p ply-cli -- -D warnings`
Expected: clean.

- [ ] **Step 7: See it on the Mac**

Run: `cargo run -p ply-cli -- check` → this still wants an image (Task 15 changes that). Instead:
Run: `cargo run -p ply-cli -- run /nonexistent.img 2>&1 | head -3`
Expected: the failure is about the missing image or the missing kernel keg — **not** "ply run is not available on this platform yet". That string must be gone from a macOS build.

Run: `cargo build -p ply-cli && codesign --entitlements /Users/iluxav/Documents/plyvm/hv.entitlements --force -s - target/debug/ply && ./target/debug/ply run /nonexistent.img`
Expected: same, and no Hypervisor entitlement complaint.

- [ ] **Step 8: Stop and report.**

---

## Task 5: assemble the kernel keg

Turn Task 2's two files into an actual `ply/microvm-kernel` image the store can hold, and teach the build script to produce it. The registry **push** is the owner's, not this plan's.

**Files:**
- Modify: `scripts/build-microvm-kernel.sh` (append the keg step)
- Create: `kernel/microvm-kernel.toml`

**Interfaces:**
- Consumes: Task 2's script, Task 3's `ply-guest-init`.
- Produces: `out/microvm-kernel-<kver>-linux-arm64.img`, a ply image whose payload is `/opt/microvm-kernel/{microvm-kernel.img,initramfs.cpio}` — exactly what `kernel::fetch_keg` (Task 4) expects.

- [ ] **Step 0: Fix the fetch path — the kernel is a keg, and `fetch_app_image` refuses kegs**

Task 4 wrote `kernel::fetch_keg` against `catalog::fetch_app_image`, which after downloading checks `manifest.is_app()` (`catalog.rs:550`) and bails with *"is a library package (keg), not a runnable app — add it to a ply.toml [dependencies] instead"*. `is_app()` is just `package.entrypoint.is_some()` (`manifest.rs:637`). So as written, `resolve()` fails on the last step for every kernel keg.

Two ways out, and the wrong one is tempting because it costs nothing: **do not** give the kernel manifest a fake `entrypoint`. That would make `ply run ply/microvm-kernel` a thing that appears to work, and would launder a real guard rather than answer it.

Instead, extract the guard. `fetch_app_image_unless` (`catalog.rs:500-556`) is exactly the right resolution logic — namespaced ref → source → `list_versions` → newest match → `ImageName::new` → `source.fetch` — with an app check bolted on the end. Split it:

```rust
/// Fetch a published image by ref, without requiring it to be runnable.
///
/// For RUNTIME artifacts that are neither an app nor any app's dependency:
/// today that is `ply/microvm-kernel`, which the binary pins and fetches for
/// itself and which no `ply.lock` ever mentions. Everything else should go
/// through `fetch_app_image`, whose `is_app` guard is what stops a keg being
/// `ply run`.
pub fn fetch_keg_image(name: &str, want: Option<&str>) -> Result<(PathBuf, ImageName, String)>
```

with the shared body factored out and `fetch_app_image_unless` keeping the guard. Then point `kernel::fetch_keg` at it.

Two things to carry across while you are there, both from Task 4's report:

- `fetch_app_image` already returns the digest as its third element, in the `sha256:<hex>` form `extracted_rootfs` wants. Task 4 stopped re-hashing the image; keep it that way — the plan's original code re-read and re-hashed a multi-megabyte file on every `ply run`.
- `kernel::resolve`'s override path checks only that `initramfs.cpio` exists, never the kernel image. A `PLY_MICROVM_KERNEL=<dir>` holding an initramfs and no `microvm-kernel.img` resolves fine and fails much later, inside `machine.rs`. Check both.

- [ ] **Step 1: Write the keg manifest**

`kernel/microvm-kernel.toml` — model it on an existing package manifest in this repo (`ls registry/` and read one, e.g. `registry/*/ply.toml`, for the exact key names this version of `ply build` accepts):

```toml
[package]
name = "microvm-kernel"
version = "6.12.0"
description = "The Linux kernel and guest init ply boots inside a macOS microVM"
```

- [ ] **Step 2: Append the keg build to the script**

```sh
# --- the keg -------------------------------------------------------------
# An ordinary ply image whose payload is the two files the VM backend needs.
# `runtime/vm/kernel.rs` extracts it into the store and reads them from
# /opt/microvm-kernel/ — keep these paths and that constant in step.
keg="$OUT/keg"
rm -rf "$keg"
mkdir -p "$keg/opt/microvm-kernel"
cp "$img" "$keg/opt/microvm-kernel/microvm-kernel.img"
cp "$OUT/initramfs.cpio" "$keg/opt/microvm-kernel/initramfs.cpio"
cp "$here/kernel/microvm-kernel.toml" "$keg/ply.toml"
cargo run --release -p ply-cli -- build "$keg" --arch arm64
echo
echo "keg built. Publishing it is the owner's step:"
echo "    ply push $OUT/keg/microvm-kernel-$KVER-linux-arm64.img"
echo "Until it is published, point ply at the local files:"
echo "    export PLY_MICROVM_KERNEL=$here/$keg/opt/microvm-kernel"
```

Check `ply build --help` for the real flag spelling of `--arch` before writing this (`cargo run -p ply-cli -- build --help`).

- [ ] **Step 3: Build it for real**

Run, on the Mac:

```sh
limactl start default
lima bash -c 'cd ~/ply-src 2>/dev/null || git -C ~ clone /Users/iluxav/Documents/ply ply-src; cd ~/ply-src && git pull && bash scripts/build-microvm-kernel.sh'
```

If the Lima VM cannot see the repo, mount it instead: Lima shares `$HOME` by default, so `lima bash -c 'cd /Users/iluxav/Documents/ply && bash scripts/build-microvm-kernel.sh'` may just work — try that first.

Expected, at the end: `config: all required options present`, and — measured, not estimated, on 2026-09-03 — a **5,263,368-byte** kernel and a **3,109,812-byte** initramfs, of which 2.4 MB is stripped static e2fsprogs.

That makes the keg roughly **8.4 MB**, against the spec's "≈7 MiB" estimate. The difference is ext4, virtio-console and squashfs-zstd — options the spec's estimate predates. Not a regression; do not go hunting for one.

Two things Task 2 learned that this step depends on: build with `OUT` on VM-local disk (`OUT=$HOME/microvm-build`), not the virtiofs repo mount — much faster, and it keeps `out/` out of the repo. And `SKIP_GUEST_INIT=1` builds the kernel half alone, which is how to bisect a kernel problem without waiting on the guest init.

- [ ] **Step 4: Verify the two consequences milestone 0 found**

Run: `lima bash -c 'cd <repo> && grep -E "^CONFIG_(EXT4_FS|VIRTIO_CONSOLE)=y" out/linux-6.12.0/build/.config'`
Expected: both lines present.

Run: `cpio -itv < out/initramfs.cpio | grep -E "dev/(null|console)"`
Expected: both nodes, both character devices.

- [ ] **Step 4b: Shrink the guest init by 4% for one environment variable**

Add `CARGO_PROFILE_RELEASE_OPT_LEVEL=z` to the env vars `scripts/build-microvm-kernel.sh` already sets beside `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER`. Measured in Task 3: 349,720 → 334,704 bytes, and every byte is unpacked into guest RAM on every single VM start.

Set it by **environment, never by a profile section**. Task 3 measured the alternatives:

| release profile | musl binary |
|---|---|
| workspace as-is (`opt-level` 3, inherited `lto`/`strip`/`cgu=1`/`panic=abort`) | 349,720 |
| `CARGO_PROFILE_RELEASE_OPT_LEVEL=z` | **334,704** |
| `[profile.release.package.ply-guest-init] opt-level = "z"` | **464,136** ← 33% *worse* |

The per-package override is legal, warning-free, precisely scoped — and a third larger. **The measurement reproduces from clean; the mechanism was never established.** (An early guess that the override drops the crate from the fat-LTO graph is wrong: `lto` is a graph-wide property cargo refuses in a package override for that very reason. A likelier story is that `opt-level = "z"` on the LTO consumer collapses the inline threshold, so serde monomorphizations a uniform `opt-level = 3` would inline and then discard survive as separate functions — which would also explain why `z` *everywhere* beats both. Unconfirmed.) Do not generalise a model from this; do take the ruling, which is the same "set it by env, never in a config file" reasoning the script already applies to the linker.

- [ ] **Step 5: Point this Mac at the local kernel**

```sh
export PLY_MICROVM_KERNEL=<repo>/out/keg/opt/microvm-kernel
```

Record that line in the task report — every later task's manual verification needs it until the keg is published.

- [ ] **Step 6: Both gates, then stop and report.**

---

## Task 6: `ply-guest-init` — the boot sequence

Now the guest becomes real: mount the layers in device order, read the spec disk, prepare volumes, write the config files, seed `/run/ply`, exec the app, forward TERM, report the exit.

**Files:**
- Modify: `ply-guest-init/src/main.rs`
- Create: `ply-guest-init/src/control.rs`, `ply-guest-init/src/volumes.rs`
- Modify: `ply-guest-init/src/spec.rs` (add `find_spec_disk`)

**Interfaces:**
- Consumes: `ply-vm-proto` (Task 1), `overlay`/`spec` (Task 3).
- Produces: the `/init` the initramfs carries. Verified end-to-end by Task 8.

- [ ] **Step 1: Write the failing test for spec-disk discovery**

In `ply-guest-init/src/spec.rs`:

```rust
/// Which attached device is the spec disk. Every disk is scanned for the
/// `PLYSPEC1` magic rather than a position being trusted (ruling R0-5):
/// same contract, one failure mode fewer.
pub fn find_spec_disk<F>(read_head: F, max_devices: usize) -> Option<(usize, Vec<u8>)>
where
    F: Fn(&str) -> Option<Vec<u8>>,
{ unimplemented!() }

#[cfg(test)]
mod disk_tests {
    use super::*;
    use ply_vm_proto::{encode_spec_disk, SPEC_MAGIC};

    #[test]
    fn the_spec_disk_is_found_by_its_magic_wherever_it_sits() {
        let spec_bytes = encode_spec_disk(&super::tests::spec()).unwrap();
        let heads = move |dev: &str| match dev {
            "/dev/vda" => Some(b"hsqs....squashfs".to_vec()),
            "/dev/vdb" => Some(b"hsqs....squashfs".to_vec()),
            "/dev/vdc" => Some(spec_bytes.clone()),
            _ => None,
        };
        let (index, bytes) = find_spec_disk(heads, 8).expect("found");
        assert_eq!(index, 2);
        assert_eq!(&bytes[..8], SPEC_MAGIC);
    }

    #[test]
    fn no_spec_disk_at_all_is_a_none_not_a_panic() {
        assert!(find_spec_disk(|_| Some(b"hsqs".to_vec()), 4).is_none());
    }

    #[test]
    fn scanning_stops_at_the_first_absent_device() {
        let mut probed = std::cell::RefCell::new(Vec::new());
        let seen = &mut probed;
        let result = find_spec_disk(
            |dev| { seen.borrow_mut().push(dev.to_string()); None },
            8,
        );
        assert!(result.is_none());
        assert_eq!(seen.borrow().len(), 1, "a gap in the device list ends the scan");
    }
}
```

Make `tests::spec()` `pub(super)` so `disk_tests` can reuse it.

- [ ] **Step 2: Run, fail, implement**

```rust
pub fn find_spec_disk<F>(read_head: F, max_devices: usize) -> Option<(usize, Vec<u8>)>
where
    F: Fn(&str) -> Option<Vec<u8>>,
{
    for index in 0..max_devices {
        let dev = crate::overlay::device_name(index);
        // Devices are contiguous from vda; the first absent one is the end.
        let head = read_head(&dev)?;
        if ply_vm_proto::is_spec_disk(&head) {
            return Some((index, head));
        }
    }
    None
}
```

Run: `cargo test -p ply-guest-init`
Expected: PASS, 8 tests.

- [ ] **Step 3: The control channel**

`ply-guest-init/src/control.rs`:

```rust
//! The control channel: `/dev/hvc1`, newline-delimited JSON both ways.
//!
//! HVF has no vsock device, so a second virtio-console is how the host and
//! the guest talk. `hvc0` carries the app's own stdout and stderr and
//! nothing else, so nothing ply says can ever be mistaken for app output.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

use ply_vm_proto::{guest_line, parse_host_line, GuestLine, HostLine};

pub const CONTROL_DEV: &str = "/dev/hvc1";

#[derive(Clone)]
pub struct Control {
    out: Arc<Mutex<File>>,
}

impl Control {
    pub fn open() -> std::io::Result<(Control, BufReader<File>)> {
        let out = std::fs::OpenOptions::new().write(true).open(CONTROL_DEV)?;
        let inbound = BufReader::new(File::open(CONTROL_DEV)?);
        Ok((Control { out: Arc::new(Mutex::new(out)) }, inbound))
    }

    /// Best-effort: a control channel that has gone away must never be the
    /// reason an app stops running.
    pub fn send(&self, line: &GuestLine) {
        if let Ok(mut out) = self.out.lock() {
            let _ = out.write_all(guest_line(line).as_bytes());
            let _ = out.flush();
        }
    }
}

/// Read host→guest lines forever, handing each to `on_line`. Unknown lines
/// are skipped: the two sides version independently.
pub fn pump(mut inbound: BufReader<File>, mut on_line: impl FnMut(HostLine)) {
    let mut line = String::new();
    loop {
        line.clear();
        match inbound.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if let Some(parsed) = parse_host_line(&line) {
                    on_line(parsed);
                }
            }
        }
    }
}
```

- [ ] **Step 4: Volumes**

`ply-guest-init/src/volumes.rs`:

```rust
//! Volume disks: format on first use, mount, hand to the app's user.
//!
//! The host creates each volume as a sparse file and hands it over raw. A
//! disk whose first 4 KiB are zero has never been formatted, so this is the
//! guest's first boot on it — `mke2fs` runs exactly once per volume, ever.

/// Has this disk been formatted? The ext4 superblock lives at offset 1024
/// and its magic is 0xEF53; an all-zero head means a fresh disk.
pub fn needs_format(head_4k: &[u8]) -> bool {
    if head_4k.len() < 1026 {
        return true;
    }
    let magic = u16::from_le_bytes([head_4k[1024], head_4k[1025]]);
    magic != 0xEF53
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zeroed_disk_is_formatted_once() {
        assert!(needs_format(&vec![0u8; 4096]));
    }

    #[test]
    fn a_disk_that_already_holds_ext4_is_left_alone() {
        let mut head = vec![0u8; 4096];
        head[1024] = 0x53;
        head[1025] = 0xEF;
        assert!(!needs_format(&head));
    }

    #[test]
    fn a_short_read_is_treated_as_unformatted_never_mounted_blind() {
        assert!(needs_format(&[0u8; 16]));
    }
}
```

Plus the syscall half: `format(dev)` runs `/sbin/mke2fs -q -t ext4 <dev>`, `mount_at(dev, path)` calls `libc::mount`, `grow(dev)` runs `/sbin/resize2fs <dev>` (a manifest may raise a volume's size between runs), `chown_tree(path, uid, gid)` walks and chowns.

- [ ] **Step 5: The boot sequence**

`ply-guest-init/src/main.rs`. Follow the spec's six numbered steps exactly, in order. Port the syscall helpers (`mount`, `run`, `poweroff`) verbatim from `/Users/iluxav/Documents/plyvm/guest-init/src/main.rs:11-62` — including its comment about raw `fork`+`execve` beating `std::process::Command` after `switch_root`, which is hard-won and still true.

```rust
//! PID 1 inside a ply microVM.
//!
//! The contract with the VMM, in order:
//!   vda..vd<layer_count-1>  read-only squashfs image layers, overlay order,
//!                           top (the app) first — the same order
//!                           `runtime/ns/mount.rs` uses on Linux, from the
//!                           same lockfile.
//!   then                    one writable disk per volume, and the spec disk,
//!                           both found by content, not by position.
//!
//! Everything else — env, entrypoint, user, hostname, peers, the params
//! seed — arrives in the spec disk. Nothing arrives on the kernel cmdline:
//! it is world-readable inside the guest and length-limited, and some of
//! this is secret.

mod control;
mod overlay;
mod spec;
mod volumes;

fn main() {
    // 0. The bare minimum to see anything at all.
    mount("proc", "/proc", "proc", 0, "");
    mount("sysfs", "/sys", "sysfs", 0, "");
    mount("devtmpfs", "/dev", "devtmpfs", 0, "");

    let Some((spec_index, head)) = spec::find_spec_disk(read_head, MAX_DEVICES) else {
        fail("no spec disk among the attached devices — the VMM did not write one");
    };
    let spec = match ply_vm_proto::decode_spec_disk(&head) {
        Ok(spec) => spec,
        Err(e) => fail(&format!("spec disk: {e}")),
    };

    // 1. The overlay: N read-only lowers, a tmpfs upper, pivot into it.
    // ... mount each layer at /mnt/<i>, then:
    //     mount("overlay", "/newroot", "overlay", 0,
    //           &format!("{},upperdir=/rw/upper,workdir=/rw/work", overlay::lowerdir("/mnt", spec.layer_count)))
    // then the switch_root dance from plyvm's init.

    // 3. Volumes, by name from the spec disk — never by position.
    // 4. /etc/hosts, hostname, /etc/resolv.conf.
    // 5. /run/ply seeded from params_seed; /run/ply/self a writable tmpfs,
    //    watched, its writes forwarded over control.
    // 6. Ready, exec, forward TERM, report the exit, power off.
}
```

**Before writing a line of this: `panic = "abort"` is inherited from the workspace release profile.** A panic in PID 1 does not unwind — it aborts, the kernel sees init die, and the guest ends in `Attempted to kill init!` with no diagnosis whatsoever, from inside a VM. **Every `.expect()` here is a machine that dies silently.** Report the error on the PL011 (which is the kernel's console and therefore visible in `ply run`'s stderr) or on `hvc1`, then exit deliberately. Reserve `expect` for things that cannot fail on data — `device_name`'s `from_utf8` is fine, because its bytes are constructed from `b'a' + n % 26`.

Also: `ply-vm-proto` carries `#![forbid(unsafe_code)]`. **Do not copy that attribute here** — this crate is raw syscalls by nature.

Two subtleties in the spec-disk contract that the modules from Task 3 do not make obvious:

- `is_spec_disk` reads only 8 bytes, and `ply-vm-proto` guarantees that anything the scan accepts never decodes as `SpecError::Magic` — it decodes, or reports `Truncated`. So in the scan loop, a `Truncated` means **"this *is* the spec disk and it is damaged"**: fail loudly, do not keep scanning. A short or empty read returns `false` and is skipped, which is what makes a not-yet-ready device harmless.
- `decode_spec_disk` never allocates from the length field — it only slices the buffer it is handed. So the caller decides how much to read, and **one sector is not enough** for a spec longer than ~500 bytes. Reading too little yields `Truncated`, not a short read to retry blindly. Read the whole device head, or read the length field first and then the body.

Two details the spec pins and this must honour:

- **The four parent-owned keys are never writable by the app.** On Linux that is a per-file read-only bind (`runtime/ns/container.rs:232`). In the guest, `/run/ply/self` is a tmpfs the app may write; after seeding it, re-mount each of `ply_vm_proto`-shared `PARENT_OWNED` names (`state`, `instances`, `started_at`, `restarts`) read-only over itself with `MS_BIND | MS_REMOUNT | MS_RDONLY`. If any one of them fails, unmount `/run/ply/self` entirely and log it — fail closed, exactly as `container.rs:243-250` does, for exactly the same reason: a writable `state` lets an app forge its own health.
- **The exit code comes back over control, not from a process.** `{"exit":N}` then power off. The host maps a VM that powers off without an `{"exit":…}` to 255.

- [ ] **Step 6: Tests and gates**

Run: `cargo test -p ply-guest-init`
Expected: PASS, 11 tests.
Run: `make check` and `cargo check -p ply-cli`
Expected: both green.

- [ ] **Step 7: Rebuild the keg with the real init**

Run: `lima bash -c 'cd <repo> && bash scripts/build-microvm-kernel.sh'`
Expected: an initramfs noticeably larger than the placeholder's (the real init plus e2fsprogs).

- [ ] **Step 8: Stop and report.**

---

## Task 7: the spec disk writer

The host half of Task 1's format: turn an `InstanceSpec` into the bytes the guest reads.

**Files:**
- Modify: `ply-core/src/runtime/vm/spec_disk.rs`

**Interfaces:**
- Consumes: `ply-vm-proto`, `runtime::backend::InstanceSpec`.
- Produces: `spec_disk::build(spec, layer_count, volumes, dns, hosts) -> SpecDisk` and `spec_disk::write(path, &SpecDisk)`. Used by Task 8.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disk_carries_the_env_the_supervisor_composed_not_the_manifests() {
        let spec = instance_spec_with_env(vec![("POSTGRES_PASSWORD".into(), "s3cret".into())]);
        let disk = build(&spec, &[], None);
        assert_eq!(disk.env, spec.env, "env crosses in the spec disk, verbatim");
    }

    #[test]
    fn volumes_are_named_with_the_device_that_carries_them() {
        let spec = instance_spec_with_volumes(vec!["/var/lib/pg".into(), "/var/log/pg".into()]);
        // Two image layers, so volumes start at vdc.
        let disk = build(&spec, &["/dev/vdc".into(), "/dev/vdd".into()], None);
        assert_eq!(disk.volumes[0].path, "/var/lib/pg");
        assert_eq!(disk.volumes[0].dev, "/dev/vdc");
        assert_eq!(disk.volumes[1].dev, "/dev/vdd");
    }

    #[test]
    fn a_secret_never_reaches_a_file_anyone_else_can_read() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("spec.img");
        write(&path, &build(&instance_spec_with_env(vec![("K".into(), "v".into())]), &[], None))
            .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the spec disk holds composed secrets");
    }

    #[test]
    fn the_written_image_is_what_the_guest_will_decode() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("spec.img");
        let disk = build(&instance_spec_with_env(vec![]), &[], Some("10.77.0.1".into()));
        write(&path, &disk).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(ply_vm_proto::decode_spec_disk(&bytes).unwrap(), disk);
    }
}
```

- [ ] **Step 2: Run, fail, implement**

```rust
//! Writing the spec disk: the one-way channel from the `ply run` parent to
//! the guest init, carrying everything the supervisor already resolved.

use std::path::Path;

use ply_vm_proto::{encode_spec_disk, SpecDisk, UserSpec, VolumeSpec};

use crate::error::{Error, Result};
use crate::runtime::backend::InstanceSpec;

/// Build the guest's view of this instance. `volume_devs` is the device path
/// for each of `spec.binds`, in the same order.
pub fn build(spec: &InstanceSpec, volume_devs: &[String], dns: Option<String>) -> SpecDisk {
    SpecDisk {
        entrypoint: spec.entrypoint.clone(),
        workdir: spec.cwd.to_string_lossy().into_owned(),
        user: spec.run_user.as_ref().map(|u| UserSpec {
            name: u.name.clone(),
            uid: u.uid,
            gid: u.gid,
        }),
        env: spec.env.clone(),
        hostname: spec.hostname.clone(),
        hosts: Vec::new(),      // Task 13 fills this from the switch's peers
        dns,
        volumes: spec
            .binds
            .iter()
            .zip(volume_devs)
            .map(|((_, path), dev)| VolumeSpec { path: path.clone(), dev: dev.clone() })
            .collect(),
        params_seed: Vec::new(), // Task 13
        layer_count: spec.images.len(),
    }
}

/// Write it, readable only by this user: the composed env in here includes
/// every secret the run resolved.
pub fn write(path: &Path, disk: &SpecDisk) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let bytes = encode_spec_disk(disk)
        .map_err(|e| Error::Runtime(format!("building the spec disk: {e}")))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| Error::Io { path: path.into(), source })?;
    file.write_all(&bytes).map_err(|source| Error::Io { path: path.into(), source })
}
```

- [ ] **Step 3: Tests and gates**

Run: `cargo test -p ply-core spec_disk`
Expected: PASS, 4 tests — **on Linux too**, which is the point of keeping this module portable.
Run: `make check` and `cargo check -p ply-cli`

- [ ] **Step 4: Stop and report.**

---

## Task 8: the VMM — boot, N disks, two consoles

The heart of the port. Take plyvm's four device files and its boot loop, and change what the spec requires: N block devices instead of one, file-backed and writable instead of an in-memory `Vec<u8>`, and a virtio-console with two ports.

Read `/Users/iluxav/Documents/plyvm/src/machine.rs` (339 lines), `blk.rs` (166), `pl011.rs` (22) end to end before starting. They are the reference implementation and are known to boot on this exact hardware.

**Files:**
- Create: `ply-core/src/runtime/vm/{machine.rs, blk.rs, console.rs, pl011.rs}`
- Create: `ply-cli/tests/macos_vm.rs`
- Modify: `Makefile` (add `mac-test`)

**Interfaces:**
- Consumes: `vm::kernel` (Task 4), `vm::spec_disk` (Task 7).
- Produces: `machine::Machine::boot(MachineConfig) -> Running`, with `Running::{console_reader, control, signal, wait, shutdown}`. Task 9 turns it into an `Instance`.

- [ ] **Step 1: Port `pl011.rs` unchanged**

Copy `/Users/iluxav/Documents/plyvm/src/pl011.rs` verbatim into `ply-core/src/runtime/vm/pl011.rs`. It is 22 lines and correct: a lookup table answering what a real PL011 answers, with writes printed. Change only the write sink — the kernel log goes to the parent's **stderr**, never stdout, so it can never be confused with the app's output on `hvc0`.

- [ ] **Step 2: Rewrite `blk.rs` for N file-backed disks**

Port plyvm's virtqueue walk (`blk.rs:105-166`) unchanged — it is a correct modern virtio-mmio block device — and change four things:

```rust
//! virtio-mmio block devices. One per image layer (read-only), one per
//! volume (writable), one for the spec disk (read-only).
//!
//! Differences from the plyvm spike this is ported from:
//!  1. N devices, each at its own MMIO window and its own SPI.
//!  2. File-backed, not an in-memory `Vec<u8>`: a volume must survive the
//!     instance, and a 27 MiB image should not be copied into RAM per boot.
//!  3. Writes reach the file (volumes) or are refused (layers, spec disk).
//!  4. VIRTIO_BLK_T_FLUSH is honoured, because a database that fsyncs and
//!     is then told "done" without the bytes landing is a corruption bug.

/// Base of the virtio-mmio window. Each device gets its own page so the
/// device tree can describe it independently.
pub const VIRTIO_BASE: u64 = 0x0B00_0000;
pub const VIRTIO_STRIDE: u64 = 0x1000;
/// First SPI. GICv3 reports 988 SPIs, so the ceiling below is ours, not the
/// hardware's.
pub const VIRTIO_INTID_BASE: u32 = 48;
/// Disks + net + console. A lockfile with more than 29 layers is not a thing
/// this runtime needs to support today, and a hard ceiling beats a silent
/// device-tree overflow.
pub const MAX_DEVICES: usize = 32;

pub fn device_gpa(index: usize) -> u64 { VIRTIO_BASE + (index as u64) * VIRTIO_STRIDE }
pub fn device_intid(index: usize) -> u32 { VIRTIO_INTID_BASE + index as u32 }

pub struct VirtioBlk {
    file: std::fs::File,
    len: u64,
    read_only: bool,
    // … the register state from plyvm's VirtioBlk, unchanged …
}
```

Register handling changes from plyvm:

- `0x010` features: keep `VIRTIO_F_VERSION_1` (bit 32) and add `VIRTIO_BLK_F_RO` (bit 5) when `read_only`, so the guest itself refuses to write a layer.
- `0x100`/`0x104` capacity: from `len / 512`, computed from the file's length at open.
- Request type `1` (write) on a read-only device returns `VIRTIO_BLK_S_IOERR` (1) instead of writing.
- Request type `4` (`VIRTIO_BLK_T_FLUSH`) calls `file.sync_data()` and returns OK. plyvm has no flush handling at all; a volume needs it.
- Reads and writes use `read_exact_at`/`write_all_at` (`std::os::unix::fs::FileExt`) at `sector * 512`, never a `Vec` copy of the whole disk.

- [ ] **Step 3: Write `console.rs`**

A virtio-mmio console (device id 3) with two ports. `VIRTIO_CONSOLE_F_MULTIPORT` (bit 1) plus the control queues is the full protocol; implement it, because port 1 is how the host stops an instance and learns its exit code.

Queue layout for a 2-port multiport console: queue 0 = port 0 rx, queue 1 = port 0 tx, queue 2 = control rx, queue 3 = control tx, then queue 4/5 = port 1 rx/tx. Config space carries `cols`, `rows`, `max_nr_ports` (2), `emerg_wr`.

The host side exposes:

```rust
/// Port 0: the app's stdout and stderr, byte for byte. The supervisor tees
/// this to its own stdout and to the log ring, exactly as it tees the
/// namespace backend's pipe.
pub fn stdout_reader(&self) -> impl std::io::Read + Send;
/// Port 1: newline-delimited JSON, `ply_vm_proto::{GuestLine, HostLine}`.
pub fn control_lines(&self) -> std::sync::mpsc::Receiver<GuestLine>;
pub fn send_control(&self, line: &HostLine);
```

**Four things Task 2's smoke test established that this task must honour:**

1. **The device-tree order IS the guest's disk order, and nothing else enforces it.** qemu's `virt` machine hands out virtio-mmio transports in reverse, so this is not hypothetical. Emit the `virtio_mmio@<gpa>` nodes in attach order, assert in `make mac-test` that the guest sees them that way (the probe-init technique from the milestone 0 spike reads the magic off each disk), and say in a comment that Task 6's guest depends on it.
2. **Handle the PSCI `SYSTEM_OFF` HVC or a guest can never shut itself down.** `CONFIG_ARM_PSCI_FW=y` is in the kernel and the guest init exits via `reboot(RB_POWER_OFF)`. plyvm's `machine.rs` already does this (`0x16` → `0x8400_0008`/`0x8400_0009`); keep it, and keep `reboot=t` on the cmdline.
3. **Attach a virtio-rng device.** The kernel now carries `HW_RANDOM` + `HW_RANDOM_VIRTIO` specifically so this needs no kernel rebuild. Apple Silicon does not advertise `FEAT_RNG`, so the arm64 arch-random path gives the guest nothing, and a microVM has almost no interrupt jitter — without an rng device, expect **multi-second stalls** in anything calling `getrandom()` early: openssl, node, the JVM. This is a startup-latency bug that looks like a hung VM.
4. **`hvc1` is this task's job, not the kernel's.** A multiport virtio-console port becomes `/dev/hvcN` only if the device sends `VIRTIO_CONSOLE_CONSOLE_PORT` for it; otherwise it is `/dev/vport0p1`. The smoke test reports both and gates on neither, because the config cannot decide it. If `hvc1` does not appear, the bug is in `console.rs`.

- [ ] **Step 4: Port `machine.rs`**

Port plyvm's `machine.rs` with these changes:

- `build_dtb` takes a device list rather than two hardcoded nodes. Emit one `virtio_mmio@<gpa>` node per device, **in attach order**, because that order is the guest's `vda`/`vdb`/… order and it is the contract with the guest init. Keep plyvm's GIC, PSCI, timer, clock and PL011 nodes verbatim — including `compatible = ["arm,pl011", "arm,primecell"]`, which milestone 0 found libkrun getting wrong and which is why plyvm's console works and libkrun's does not.
- The MMIO dispatch in the exit loop becomes a range lookup over the device list instead of three `contains` checks.
- The boot loop gains a shutdown path: an `AtomicBool` the host sets to tear the VM down (`Instance::signal(SIGKILL)`), checked on the same 5 ms waker plyvm already runs.
- `Config` becomes `MachineConfig { kernel: PathBuf, initramfs: PathBuf, disks: Vec<DiskSpec>, mem_bytes, cmdline, switch: Option<UnixStream> }`. The kernel and initramfs are read from paths (they are in the store), not handed in as `Vec<u8>`.
- Everything runs on threads inside the caller: `Machine::boot` spawns the vCPU thread and returns a `Running` handle. It must **not** block, and it must **never** call `exit()` — that was the disqualifying property of libkrun (see the milestone 0 result).

The kernel cmdline is fixed and short, because everything else travels in the spec disk:

```rust
const CMDLINE: &str = "earlycon=pl011,mmio,0x09000000 console=ttyAMA0 panic=-1 reboot=t";
```

- [ ] **Step 5: Write the integration test**

`ply-cli/tests/macos_vm.rs`:

```rust
//! The macOS microVM suite. Run with `make mac-test` on an Apple Silicon Mac
//! with `PLY_MICROVM_KERNEL` pointing at a built kernel; skipped everywhere
//! else, because it boots real VMs.
#![cfg(target_os = "macos")]

/// Every disk this VMM attaches must reach the guest in the order it was
/// attached, or the overlay assembles the wrong filesystem. Milestone 0
/// established this as the single load-bearing property of the disk model.
#[test]
fn disks_reach_the_guest_in_attach_order() { /* … */ }

#[test]
fn a_guest_that_powers_off_returns_its_exit_code_over_the_control_channel() { /* … */ }

#[test]
fn the_apps_stdout_arrives_on_port_zero_and_control_json_never_does() { /* … */ }
```

Add to the `Makefile`:

```make
# The macOS microVM suite: boots real VMs, so it runs only on an Apple
# Silicon Mac and only when asked. Needs a kernel — either the published
# `ply/microvm-kernel` keg or PLY_MICROVM_KERNEL pointing at a local build
# (scripts/build-microvm-kernel.sh).
mac-test:
	cargo test -p ply-cli --test macos_vm -- --test-threads=1 --nocapture
```

- [ ] **Step 6: Boot something**

Run: `make mac-test`
Expected: the disk-order test passes. If it does not, the fastest instrument is the one milestone 0 used: a probe init that writes `/proc/partitions` and every disk's first 16 bytes into a writable disk the host reads back. That technique is described in the spike result document.

- [ ] **Step 7: Both gates**

Run: `make check` (Linux) — must be untouched; none of this task's code compiles there.
Run: `cargo check -p ply-cli && cargo clippy -p ply-cli -- -D warnings`
Expected: clean.

- [ ] **Step 8: Stop and report** — include the `make mac-test` output and the boot time.

---

## Task 9: `VmBackend::launch` and `VmInstance`

Join Tasks 7 and 8 to the seam: an `InstanceSpec` in, a `Launched` out, state files written, `ply ps`/`logs`/`stop`/`restart` working.

**Files:**
- Modify: `ply-core/src/runtime/vm/mod.rs`

**Interfaces:**
- Consumes: everything from Tasks 4, 7, 8.
- Produces: a working `ply run <image>` on macOS for an app with no network and no volumes.

- [ ] **Step 1: Write `launch`**

The sequence, in order, with the reasons the order matters:

1. Resolve the kernel (`kernel::resolve()`, cached on the backend from `new()`).
2. Build the disk list: `spec.images` in order (read-only), then one per `spec.binds` (writable, created sparse under `paths::volumes_dir()` if absent), then the spec disk written into `spec.instance_dir/spec.img`.
3. `spec_disk::build` + `write`, with the volume device names computed from the disk list — the same `device_name` function the guest uses, so the two cannot disagree. Import it rather than reimplementing: move `overlay::device_name` into `ply-vm-proto` in this task and have both sides call it.
4. `Machine::boot` — non-blocking, returns `Running`.
5. Wait for `{"ready":true}` on the control channel, up to a bounded timeout (10 s). A VM that never says ready is a failed launch, not a hung one.
6. `record(std::process::id() as i32, ip)` — the parent's own pid, per the spec, while nothing can race it.
7. Return `Launched { instance, output }` where `output` is the console port-0 reader.

**Filter `PARENT_OWNED` out of inbound `{"publish":…}` on the host side, too.**

The guest already refuses to forward `state`, `instances`, `started_at` and `restarts` from its own `/run/ply/self` watch, and that filter is what stops an app forging its own health — every `--after` dependant downstream believes those four keys. But the guest is not a trustworthy place to enforce it alone: unlike the namespace backend, a VM app can run as uid 0 with full capabilities in the guest's initial user namespace and defeat the in-guest seal, and `PLY_MICROVM_KERNEL` is a documented user override, so the entire guest sits inside a boundary the user can replace.

So when this task applies a `GuestLine::Publish`, drop any key in `PARENT_OWNED` and log it once. The parent owns those four; nothing arriving over a console should be able to set them. Defence in depth belongs on the host side of a replaceable boundary.

- [ ] **Step 2: Write `VmInstance`**

```rust
/// One instance as the macOS backend runs it: a microVM on threads inside
/// this process.
///
/// `pid()` is the PARENT's pid — the spec's choice, and the right one: it is
/// what `ply stop` signals and what `state::reap_stale` tests for liveness,
/// and after a `kill -9` of the parent that test correctly says "dead".
/// `child_pid()` is therefore `None`, which is why the supervisor's main
/// loop (Task 10) must send the stop signal itself: the signal HANDLER
/// cannot, there being no process to `kill`.
pub(crate) struct VmInstance {
    app: String,
    n: u32,
    ip: Ipv4Addr,
    machine: machine::Running,
    /// Sticky, like `NsInstance::ended`: once known, always the same answer.
    ended: Option<i32>,
    _spec_disk: TempPath,
}

impl Instance for VmInstance {
    fn pid(&self) -> i32 { std::process::id() as i32 }
    fn child_pid(&self) -> Option<i32> { None }
    fn ip(&self) -> Ipv4Addr { self.ip }
    fn alive(&self) -> bool { self.ended.is_none() && self.machine.running() }

    fn signal(&self, sig: Signal) -> Result<()> {
        match sig {
            // The polite request: the guest init forwards it to the app,
            // which gets the same signal it would get under namespaces.
            Signal::SIGKILL => self.machine.shutdown(),
            other => {
                self.machine.send_control(&HostLine { name: signal_name(other) });
                Ok(())
            }
        }
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        if let Some(code) = self.ended { return Ok(Some(code)); }
        // `{"exit":N}` is the app's own status. A machine that stopped
        // without one died some other way (a panic, a kill, a guest that
        // powered off on its own); 255 is what a supervisor calls that.
        match self.machine.poll_exit() {
            Some(code) => { self.ended = Some(code); Ok(self.ended) }
            None if !self.machine.running() => { self.ended = Some(255); Ok(self.ended) }
            None => Ok(None),
        }
    }

    fn tcp_open(&self, port: u16, timeout: Duration) -> std::io::Result<()> { /* Task 14 */ }
}
```

`signal_name` maps `Signal::SIGTERM` → `"TERM"` etc. — the guest init turns the name back into a number, so nothing depends on the numbers matching across the boundary (they do on arm64 Linux vs macOS for the common ones, but relying on that would be a trap).

- [ ] **Step 3: Verify on the Mac**

Run:
```sh
export PLY_MICROVM_KERNEL=<repo>/out/keg/opt/microvm-kernel
cargo build -p ply-cli
codesign --entitlements /Users/iluxav/Documents/plyvm/hv.entitlements --force -s - target/debug/ply
./target/debug/ply run <a debian .img built by ply build>
```
Expected: the image's entrypoint runs, its stdout appears, and `ply run` exits with the app's code.

Run: `./target/debug/ply ps` from a second terminal while it runs.
Expected: one row, the app's name, the parent's pid, its IP.

Run: `./target/debug/ply logs <app>`
Expected: the same output the terminal showed — the log ring is fed by the console tee.

- [ ] **Step 4: Both gates, then stop and report.**

---

## Task 10: the supervisor holes plan 1 left

Three items from the spec's Appendix. All portable, all testable on Linux with `supervise::FakeInstance`, and all needed before a VM instance can be stopped correctly.

**Files:**
- Modify: `ply-core/src/runtime/run.rs` (the shutdown branch ~line 348; `same_network` ~line 138)
- Modify: `ply-core/src/runtime/backend.rs` (`NetworkFacts`)
- Modify: `ply-core/src/runtime/ns/mod.rs` (`network()` returns the new field)

- [ ] **Step 1: Write the failing test**

In `ply-core/src/runtime/supervise.rs`'s test module, add a test for the new helper this task extracts:

```rust
#[test]
fn a_shutdown_reaches_an_instance_that_is_not_a_child_process() {
    // A VM instance has no pid to kill: `child_pid()` is None, so the
    // signal HANDLER cannot reach it and the main loop must. Before this,
    // such an instance got nothing for ten seconds and then SIGKILL.
    let fake = FakeInstance { obeys: Some(Signal::SIGTERM), ..Default::default() };
    let sent = request_stop(&[&fake as &dyn Instance], Signal::SIGTERM);
    assert_eq!(sent, 1);
    assert_eq!(*fake.signals.borrow(), vec![Signal::SIGTERM]);
}

#[test]
fn an_instance_the_handler_already_signalled_is_not_signalled_twice() {
    // Namespace instances are reached by the handler (it can kill a pid
    // even while the loop is blocked in the `--after` wait); signalling
    // them again here would send SIGTERM twice to an app that asked for
    // SIGQUIT-then-nothing.
    struct Child(FakeInstance);
    // child_pid() = Some → skipped.
    let fake = FakeInstance::default();
    assert_eq!(request_stop_for(&fake, Some(42), Signal::SIGTERM), 0);
}
```

- [ ] **Step 2: Implement**

In `supervise.rs`:

```rust
/// Ask every instance the signal handler cannot reach to stop.
///
/// The handler `kill`s `child_pid()` directly, which is the only thing that
/// works while the main loop is blocked in the `--after` wait — so namespace
/// instances are already covered. A VM instance has no pid: its `child_pid()`
/// is `None` and the only way in is `Instance::signal`, from here. Without
/// this, a VM instance received nothing until `SHUTDOWN_GRACE` expired and
/// then got SIGKILL — a `systemctl stop` that means "kill -9 in ten
/// seconds" to half the runtime.
///
/// Returns how many were signalled.
pub fn request_stop(instances: &[&dyn Instance], stop: Signal) -> usize {
    instances
        .iter()
        .filter(|i| i.child_pid().is_none())
        .map(|i| { let _ = i.signal(stop); })
        .count()
}
```

In `run.rs`'s main loop, in the `if shutting_down` branch, **before** the existing escalation check:

```rust
// First observation of a shutdown: reach the instances the handler could
// not (see supervise::request_stop). Once — a second SIGTERM to an app
// that asked for one is not what `ply stop` promises.
if shutting_down && shutdown_began.is_none() && !instances.is_empty() {
    let refs: Vec<&dyn crate::runtime::backend::Instance> =
        instances.iter().map(|r| r.inner.as_ref()).collect();
    crate::runtime::supervise::request_stop(&refs, stop_signal);
}
```

`shutdown_began` is set by the existing `get_or_insert_with` just below, so `is_none()` is true exactly once. Verify that by reading the branch as it stands before editing.

- [ ] **Step 3: Move `same_network` into `NetworkFacts`**

In `backend.rs`, add to `NetworkFacts`:

```rust
/// Do the published listener and the instances live in ONE network? On the
/// host's, yes — and then a backend equal to the listener's own address is
/// a loop (`publish::serve` refuses it). Namespaces answer "no" once the
/// run has joined one; a VM always answers "no", its instances being on
/// the switch. Was `opts.netns.is_none()` inline in `run.rs`, which is
/// namespace vocabulary in platform-neutral code.
pub same_network: bool,
```

`NsBackend::network` returns `same_network: opts.netns.is_none()`; `VmBackend::network` returns `false`. In `run.rs`, replace `let same_network = opts.netns.is_none();` with `let same_network = net.same_network;`.

- [ ] **Step 4: Verify**

Run: `cargo test -p ply-core supervise`
Expected: PASS, 9 tests.
Run: `make check`
Expected: green, all existing tests unchanged.
Run: `cargo check -p ply-cli && cargo clippy -p ply-cli -- -D warnings`

- [ ] **Step 5: Confirm on the Mac**

With an instance running from Task 9, from a second terminal: `./target/debug/ply stop <app>`
Expected: the app receives its stop signal and exits within the patience window — **not** ten seconds later by SIGKILL. Time it.

- [ ] **Step 6: Stop and report.**

---

## Task 11: the Linux-only paths that degrade off Linux

Six places named in the spec's Appendix read `/proc` or `/etc/hosts` from portable code. On a Mac they silently do the wrong thing. Fix each so it is honest.

**Files:**
- Modify: `ply-core/src/paths.rs:31-38`, `ply-core/src/runtime/state.rs:reap_stale`, `ply-core/src/runtime/hosts.rs`, `ply-core/src/stats.rs`, `ply-core/src/lifecycle.rs`, `ply-cli/src/commands/ps.rs`, `ply-cli/src/commands/reconcile.rs`

- [ ] **Step 1: Write the failing tests**

```rust
// ply-core/src/paths.rs
#[test]
#[cfg(not(target_os = "linux"))]
fn off_linux_root_is_just_the_uid() {
    // `in_initial_user_ns` reads /proc/self/uid_map, which does not exist on
    // a Mac — and its `Err(_) => true` fallback then makes `sudo ply` pick
    // Linux root's paths (/var/lib/ply) on a machine that has no such
    // convention. There are no user namespaces here; the uid is the answer.
    assert_eq!(is_root(), nix::unistd::geteuid().is_root());
}

// ply-core/src/runtime/hosts.rs
#[test]
#[cfg(not(target_os = "linux"))]
fn managing_etc_hosts_is_a_linux_only_affair() {
    // A microVM gets its peers from the spec disk, inside its own /etc/hosts.
    // Rewriting the Mac's /etc/hosts would need sudo and would be wrong.
    assert!(remove_entry("nothing", 0).is_ok());
    assert!(add_entry("nothing", 0, std::net::Ipv4Addr::LOCALHOST).is_ok());
}
```

- [ ] **Step 2: Implement each**

1. **`paths::is_root`** — gate `in_initial_user_ns`:

```rust
/// User namespaces are a Linux concept; elsewhere the uid is the whole
/// answer. Reading a missing /proc/self/uid_map and falling back to "trust
/// the uid" happened to give the same result, but it did so by accident and
/// only for the euid==0 case.
#[cfg(not(target_os = "linux"))]
fn in_initial_user_ns() -> bool { true }
```

2. **`hosts.rs`** — wrap the module body in `#[cfg(target_os = "linux")]` and add non-Linux no-op `add_entry`/`remove_entry`/`refresh_instances` that return `Ok(())`. Document why: a VM instance's `/etc/hosts` is written inside the guest from the spec disk, so there is nothing on the host to manage.

3. **`state::reap_stale`** — it currently calls `hosts::remove_entry(...)?` and propagates. With the no-op above it is already correct off Linux; leave the call, and add a test that a reap with no `/etc/hosts` access still reaps.

4. **`stats.rs`** — it already falls back to "unknown" for every field. Add a one-line module doc saying so and that `ply stats` on macOS reports memory and CPU as unknown until a later release, so nobody reads the zeros as real.

5. **`lifecycle.rs`** (`/proc/<pid>/stat` for the parent pid) and **`ps.rs`** (`/proc/<ppid>/exe`, `/proc/<pid>/stat`) — extract each `/proc` read into a small function with a `#[cfg(not(target_os = "linux"))]` arm returning `None`, and make the caller's fallback path the honest one.

6. **`reconcile.rs`** (`/proc/meminfo`) — same shape; on macOS use `sysctl -n hw.memsize`, which is one command and gives the number reconcile actually wants.

- [ ] **Step 3: Verify**

Run: `make check`
Expected: green — no Linux behaviour changed.
Run: `cargo test -p ply-core` **on the Mac**: `cargo test -p ply-core --lib`
Expected: the portable tests pass, including the two new ones. (Some ply-core tests are Linux-only and will not compile on macOS; if `cargo test -p ply-core` does not build on the Mac at all, note exactly which modules block it in the report — that is a finding for a later task, not something to fix here.)

- [ ] **Step 4: Stop and report.**

---

## Task 12: the switch — L2, ARP, addressing

The per-stack virtual network, in the parent process. Portable and unit-tested on synthetic frames, because that is the only way this gets tested at all.

**Files:**
- Create: `ply-core/src/runtime/vm/switch/{mod.rs, frame.rs}`
- Modify: `Cargo.toml` (`smoltcp`)

**Interfaces:**
- Consumes: nothing.
- Produces: `Switch::bind(path)`, `Switch::allocate(name) -> Ipv4Addr`, `Switch::connect(guest_ip, port) -> TcpStream`. Tasks 13, 14 use them.

- [ ] **Step 1: Add smoltcp**

Run: `cargo add --dry-run smoltcp` and read the version it reports; pin that major.minor in `[workspace.dependencies]`:

```toml
# The switch's TCP/IP: pure Rust, no_std-capable, and it has a loopback
# device, which is what lets the switch's logic be tested on synthetic
# frames in Linux CI instead of only inside a booted VM.
smoltcp = { version = "<what cargo add reported>", default-features = false, features = [
    "std", "medium-ethernet", "proto-ipv4", "socket-tcp", "socket-udp", "socket-dns",
] }
```

Add `smoltcp.workspace = true` to `ply-core`'s plain `[dependencies]` — not the macOS block. The switch is portable and Linux CI must compile and test it.

- [ ] **Step 2: Write the framing, test first**

`switch/frame.rs`:

```rust
//! The `--vswitch` wire: a 4-byte big-endian length followed by one raw
//! Ethernet frame, over a unix SOCK_STREAM.
//!
//! Big-endian u32, not the u16 the design sketched, because this is exactly
//! what passt and gvproxy speak (ruling R0-2 from the milestone 0 spike):
//! the same tooling that debugs a podman machine's network debugs this one.
//! macOS has no SOCK_SEQPACKET for unix sockets, which is why the length
//! prefix exists at all.

pub const MAX_FRAME: usize = 65_535;

pub fn write_frame(out: &mut impl std::io::Write, frame: &[u8]) -> std::io::Result<()>;
pub fn read_frame(input: &mut impl std::io::Read, buf: &mut Vec<u8>) -> std::io::Result<usize>;

#[cfg(test)]
mod tests {
    #[test]
    fn a_frame_round_trips_through_the_length_prefix() { /* … */ }

    #[test]
    fn two_frames_in_one_read_are_both_delivered() {
        // SOCK_STREAM has no message boundaries: the reader must never
        // assume one read is one frame.
    }

    #[test]
    fn a_frame_split_across_reads_is_reassembled() { /* … */ }

    #[test]
    fn an_oversized_length_is_refused_before_it_allocates() {
        // A malicious or broken peer must not be able to make the parent
        // allocate 4 GiB by sending four bytes.
    }
}
```

- [ ] **Step 3: Write the switch, test first**

```rust
//! One virtual L2 network per stack, in the `ply up` (or standalone
//! `ply run`) parent. No daemon, no entitlement, no tap device: macOS gives
//! us no tap without `com.apple.vm.networking`, which is restricted, so the
//! network lives in userspace and dies with the parent — exactly as the
//! stack's netns does on Linux.
//!
//! Members connect a unix socket and speak Ethernet. The switch answers ARP,
//! hands out fixed addresses from the same 10.77.0.0/16 range the Linux
//! bridge uses, answers `<name>.ply`, and terminates guest TCP to anywhere
//! (smoltcp in `any_ip` mode), bridging to real host sockets for egress.
```

Tests, all on synthetic frames with no VM in sight:

```rust
#[test]
fn a_member_gets_an_address_in_the_same_range_the_linux_bridge_uses() {
    let sw = Switch::in_memory();
    let ip = sw.allocate("db");
    assert_eq!(ip.octets()[0..2], [10, 77]);
    assert_ne!(ip, publish::GATEWAY, "the gateway is the switch itself");
}

#[test]
fn the_same_name_always_gets_the_same_address_within_one_stack() {
    let sw = Switch::in_memory();
    assert_eq!(sw.allocate("db"), sw.allocate("db"));
    assert_ne!(sw.allocate("db"), sw.allocate("web"));
}

#[test]
fn an_arp_request_for_the_gateway_is_answered_with_the_switchs_mac() {
    let sw = Switch::in_memory();
    let reply = sw.handle_frame(&arp_request(GUEST_MAC, guest_ip, publish::GATEWAY)).expect("reply");
    assert_eq!(ethertype(&reply), 0x0806);
    assert_eq!(arp_opcode(&reply), 2, "an ARP reply, not a second request");
}

#[test]
fn a_frame_for_a_peer_is_delivered_to_that_peer_and_to_nobody_else() {
    // The L2 half: two members, one frame, one recipient.
}

#[test]
fn a_broadcast_reaches_every_member_except_the_sender() { /* … */ }
```

- [ ] **Step 4: Verify**

Run: `cargo test -p ply-core switch`
Expected: PASS, 9 tests, **on Linux**.
Run: `make check` and `cargo check -p ply-cli`

- [ ] **Step 5: Stop and report.**

---

## Task 13: `--vswitch` — wiring the switch to `ply up` and `ply run`

Replace the namespace vocabulary in `RunOptions` and give the stack a switch on macOS the way it gets a netns on Linux.

**Files:**
- Modify: `ply-core/src/runtime/run.rs` (`RunOptions` fields), `ply-cli/src/cli.rs:285-295`, `ply-cli/src/commands/up.rs:336-352,427-460`, `ply-core/src/runtime/vm/mod.rs`

- [ ] **Step 1: Rename the fields**

The spec's Appendix item 3: `netns`/`netns_dns`/`netns_peers` are namespace words in a portable struct. Rename to the neutral names both backends can mean:

| was | becomes | Linux meaning | macOS meaning |
|---|---|---|---|
| `netns: Option<PathBuf>` | `network: Option<PathBuf>` | a netns path to join | a switch socket to connect |
| `netns_dns: Option<String>` | `network_dns: Option<String>` | the netns router | the switch's resolver |
| `netns_peers: Vec<String>` | `network_peers: Vec<String>` | siblings resolving to loopback | siblings resolving to their switch IPs |

Keep the **CLI flags** as they are (`--netns`, `--netns-peer`, `--netns-dns` are hidden and `ply up` passes them to itself), and add `--vswitch` as a second hidden spelling that sets the same fields. A user never types either.

- [ ] **Step 2: `ply up` starts a switch on macOS**

`stack_network()` in `up.rs` already has a `#[cfg(not(target_os = "linux"))]` arm returning `(None, None)` with a comment naming "plan 2's switch". Replace it:

```rust
/// One network for the stack: on macOS, an in-process L2 switch this
/// process owns. It listens on a unix socket under `run_dir()`, each
/// member's VMM connects to it, and it dies with this process — the same
/// lifetime the netns has on Linux, and for the same reason.
#[cfg(target_os = "macos")]
fn stack_network() -> (Option<StackNet>, Option<String>) { … }
```

Members then get `--vswitch <socket>` plus `--netns-peer` for each sibling, exactly as the Linux path does.

- [ ] **Step 3: A standalone `ply run` gets its own switch**

`VmBackend::preflight` mirrors `NsBackend::preflight`'s "a rootless run with no namespace makes its own": when `opts.network` is `None`, start a private in-process switch and set `opts.network`/`opts.network_dns` to it. One app is then as isolated as a stack member.

- [ ] **Step 4: `attach` and the spec disk's peers**

`VmBackend::attach` connects to the switch socket and allocates this instance's address. `spec_disk::build` gains the `hosts` and `params_seed` it left empty in Task 7: `hosts` from `opts.network_peers` resolved through the switch, `params_seed` from `params_tree` for this app and its peers.

- [ ] **Step 5: Verify**

Run: `make check`
Expected: green — the rename touches Linux code, so every existing test must still pass, and `ply up` on Linux must behave identically. Run a rootless two-member stack by hand and compare its output to `git stash`-ed behaviour.
Run: `cargo check -p ply-cli && cargo clippy -p ply-cli -- -D warnings`
Run, on the Mac: `./target/debug/ply run <image>` and confirm from the guest's own output that it has an `eth0` with a `10.77.x.y` address and can resolve its own hostname.

- [ ] **Step 6: Stop and report.**

---

## Task 14: published ports, DNS, and the health/`after` path

The last networking piece: the parent's host listeners reach guests through the switch, `<name>.ply` resolves, and the three probes (`health_gate`, `--after`, `discovery_env`) work.

**Files:**
- Modify: `ply-core/src/runtime/publish.rs` (`Pool`), `ply-core/src/runtime/backend.rs` (`Instance::connector`), `ply-core/src/runtime/vm/switch/{mod.rs, dns.rs}`, `ply-core/src/runtime/vm/mod.rs`

- [ ] **Step 1: Give `Pool` a connector**

The spec's Appendix item 2: `Pool` holds raw `SocketAddr`s and `publish::serve` connects to them directly, which cannot reach a guest on the switch. Introduce:

```rust
/// How to reach one instance's port. Namespaces hand back an address the
/// host can dial; a VM hands back a connector that goes through the switch.
/// `Instance::tcp_open` covered only the health and `--after` probes — the
/// published pool needs the same indirection or every `--publish` on macOS
/// dials an address that means nothing on the host.
pub trait Connector: Send + Sync {
    fn connect(&self, timeout: Duration) -> std::io::Result<TcpStream>;
    /// For messages and for `serve`'s self-connection guard.
    fn addr(&self) -> SocketAddr;
}
```

`Pool::insert(n, Arc<dyn Connector>)`. A plain `SocketAddr` gets a trivial impl so the namespace path is unchanged in behaviour. Add `Instance::connector(&self, port: u16) -> Arc<dyn Connector>` to the trait, with a default implementation returning the address-based one so `NsInstance` needs no change.

- [ ] **Step 2: Test it**

```rust
#[test]
fn a_pool_backed_by_a_connector_reaches_a_backend_with_no_host_address() {
    // The property that matters: `serve` never assumes it can dial the
    // backend itself. A test connector that hands back a socketpair proves
    // the pool works with a guest that has no host-reachable address.
}

#[test]
fn the_self_connection_guard_still_fires_for_address_backends() {
    // The loop guard in `serve` (a backend equal to the listener's own
    // address) must survive the indirection — it is what stops the proxy
    // spawning a thread per hop until the process dies.
}
```

- [ ] **Step 3: DNS**

`switch/dns.rs`: answer A queries for `<name>.ply` from the switch's own name→IP table, forward everything else to the host's resolver (read `/etc/resolv.conf`; on macOS also accept `scutil --dns` output, or simply fall back to the system resolver by making the query from the host with `std::net`). Tests: a `<name>.ply` question is answered locally with the right address; an unknown `.ply` name gets NXDOMAIN, not a forward; any other name is forwarded.

- [ ] **Step 4: `tcp_open` and `discovery_env`**

`VmInstance::tcp_open` connects through the switch to `self.ip:port`. That single method makes `health_gate` and the `--after` port probes work unchanged — they were written against the trait in plan 1 precisely so this would be true.

`discovery_env` already has the two branches it needs (`in_stack_network` chooses the guest address over the published one); confirm `VmBackend::network` sets `in_stack_network: true` when the run joined a switch, and add a test.

- [ ] **Step 5: Verify**

Run: `make check`
Expected: green, and the existing publish tests unchanged.
Run, on the Mac:
```sh
./target/debug/ply run postgres@17 -e POSTGRES_PASSWORD=dev -p 5432
# second terminal:
psql -h 127.0.0.1 -p 5432 -U postgres -c 'select 1'
```
Expected: a row. This is the spec's headline command.

- [ ] **Step 6: Stop and report.**

---

## Task 15: volumes that persist, and `ply check`

**Files:**
- Modify: `ply-core/src/runtime/vm/mod.rs` (volume disks), `ply-cli/src/cli.rs` (`CheckArgs`), `ply-cli/src/commands/lifecycle.rs` (`check`)

- [ ] **Step 1: Volume disks**

Per the spec's "Volumes as disks": one sparse `disk.ext4` under `data_dir()/volumes/<app>/<name>.<instance|shared>/`, the same per-volume directory the Linux backend bind-mounts, so the supervisor's volume bookkeeping and its empty-volume warning (`run.rs:1519-1543`) are shared. Default size 8 GiB sparse; a manifest that raises it triggers `resize2fs` from the guest.

The empty-volume warning tests `read_dir(host_dir).next().is_none()`. A volume directory holding a `disk.ext4` is never empty, so that warning would never fire on macOS. Fix it: the check becomes "empty, or holds only an unformatted disk image". Add a test.

- [ ] **Step 2: `ply check` reports the host capability**

Make `CheckArgs.image` `Option<PathBuf>`. With no image, print the host report:

```
ply check
  platform:       macOS 26.5.2, Apple Silicon
  virtualization: Hypervisor.framework ok
  kernel:         ply/microvm-kernel@6.12.0 (store)
  store:          /Users/…/.local/share/ply/store
```

or, when it is not ok, the reason and the remedy from `capability_report()`. On Linux the same command reports namespaces, subid ranges and the AppArmor situation — reuse what `ply setup` already knows.

`ply check <image>` keeps doing exactly what it does today; add a test that the image path is unchanged.

- [ ] **Step 3: Verify**

Run: `make check`; `cargo check -p ply-cli`
Run, on the Mac:
```sh
./target/debug/ply check
./target/debug/ply run postgres@17 -e POSTGRES_PASSWORD=dev -p 5432   # ^C after it is up
./target/debug/ply run postgres@17 -e POSTGRES_PASSWORD=dev -p 5432   # again
```
Expected: the second run finds the first one's data — no `initdb`, an existing database.

- [ ] **Step 4: Stop and report.**

---

## Task 16: `install.sh`, `self-update`, and the release lane

**Files:**
- Modify: `install.sh`, `ply-cli/src/commands/self_update.rs`, `.github/workflows/release.yml`
- Create: `macos/ply.entitlements`

- [ ] **Step 1: The entitlement**

`macos/ply.entitlements` — copy `/Users/iluxav/Documents/plyvm/hv.entitlements` verbatim. It is the four lines that let `hv_vm_create` succeed.

- [ ] **Step 2: `install.sh`**

Replace the `[ "$(uname -s)" = "Linux" ] || { echo "error: ply is Linux-only"; exit 1; }` line with a per-OS asset choice:

```sh
os=$(uname -s)
case "$os" in
    Linux)  asset="ply-linux-$target" ;;
    Darwin)
        [ "$arch" = "arm64" ] || { echo "error: ply on macOS needs Apple Silicon (M1 or later) — Intel Macs are not supported"; exit 1; }
        asset="ply-darwin-arm64" ;;
    *) echo "error: unsupported OS: $os (ply supports Linux and macOS on Apple Silicon)"; exit 1 ;;
esac
```

`uname -m` on Apple Silicon reports `arm64`, not `aarch64` — the existing `case "$arch"` must learn that spelling.

The macOS install path differs from Linux's: no `ply setup`, no AppArmor, no sudo — install to `/usr/local/bin` if writable, else `~/.local/bin`, and skip the edge/dashboard wizard entirely (it is a Linux server story). Add `PLY_FROM_SOURCE=1` which clones and builds locally and ad-hoc signs with the entitlement, as plyvm's installer does — the escape hatch until the owner's Developer ID is in CI.

- [ ] **Step 3: `self-update`**

`self_update.rs:56` hardcodes `ply-linux-{arch}`. Make it pick `ply-darwin-arm64` on macOS. Add a unit test for the asset-name function on both platforms.

After replacing the binary on macOS, the new one must be signed with the entitlement or it cannot create a VM. A downloaded, notarized binary carries its signature already; a locally built one does not. Verify with `codesign -d --entitlements - <path>` after the rename and warn loudly if the entitlement is missing.

- [ ] **Step 4: The release job**

Add to `release.yml`'s `build` job a second matrix entry — or, more honestly, a separate `darwin` job, because the steps differ:

```yaml
  darwin:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup target add aarch64-apple-darwin
      - run: cargo build --release --target aarch64-apple-darwin -p ply-cli
      # Ad-hoc signing until the owner's Developer ID is in secrets: the
      # binary then runs for anyone who builds it, and Gatekeeper quarantines
      # a downloaded copy. With the certificate present it is signed and
      # notarized properly and the download just works.
      - name: sign
        env:
          CERT_P12: ${{ secrets.APPLE_DEVELOPER_ID_P12 }}
        run: |
          cp target/aarch64-apple-darwin/release/ply ply-darwin-arm64
          if [ -n "$CERT_P12" ]; then
            # import the certificate, codesign --options runtime with the
            # entitlement, then notarytool submit --wait and staple
            …
          else
            echo "::warning::no Developer ID in secrets — ad-hoc signing only; downloads will be quarantined"
            codesign --entitlements macos/ply.entitlements --force -s - ply-darwin-arm64
          fi
      - uses: actions/upload-artifact@v4
        with: { name: ply-darwin-arm64, path: ply-darwin-arm64, retention-days: 1 }
```

and make `publish` `needs: [build, darwin]` with `ply-darwin-arm64` added to its existence check and its `gh release create` argument list. That check is deliberate — the release refuses to publish a partial set.

- [ ] **Step 4b: The owner's step**

Write, in the task report, exactly what the owner must add: an Apple Developer ID Application certificate exported as a base64 `.p12` in `APPLE_DEVELOPER_ID_P12`, its password, and an App Store Connect API key for `notarytool`. Name the secrets the workflow reads.

- [ ] **Step 5: Verify**

Run: `sh -n install.sh`
Run: `make check`; `cargo check -p ply-cli`
Run: `PLY_BINARY=target/debug/ply sh install.sh` on the Mac
Expected: it installs and reports a version; no Linux-only step runs.

- [ ] **Step 6: Stop and report.**

---

## Task 17: docs

**Files:**
- Rewrite: `docs/macos.md`
- Modify: `docs/ply-vm.md` (status), `docs/index.md` and `README.md` (the install line and the platform list)

- [ ] **Step 1: Rewrite `docs/macos.md`**

The current page says "ply's runtime is built on Linux kernel primitives … so it doesn't run natively on macOS" and sends the reader to Lima. Invert it: native microVMs are the path; Lima becomes the fallback for Intel Macs and for anyone who wants a full Linux userland. Keep the Lima section — it is still the right answer for an Intel Mac — under a heading that says so.

Cover: the one-liner install, what is the same as Linux (every command, every file, the same images and lockfiles), what is different (no `links`, no `[resources]`, no `ply exec`, ~200 ms slower start), the kernel keg and `PLY_MICROVM_KERNEL`, `ply check`, and where volumes live.

- [ ] **Step 2: `docs/ply-vm.md`**

It is marked "parked". Update its status line to point at the two spec documents and this plan, and keep its requirements R1–R6 as the record of why the design is what it is.

- [ ] **Step 3: Verify**

Run: `grep -rn "Linux-only\|doesn't run natively on macOS" docs/ README.md install.sh`
Expected: no stale claim survives.

- [ ] **Step 4: Stop and report.**

---

## Task 18: the full acceptance run

No new code. Walk the spec's own acceptance list on this Mac and write down what happened.

- [ ] **Step 1: The spec's integration list** (from its Testing section)

```sh
export PLY_MICROVM_KERNEL=<repo>/out/keg/opt/microvm-kernel   # until the keg ships

# 1. a trivial boot, under two seconds
time ply run <debian image with entrypoint = ["/bin/true"]>   # → exit 0, < 2 s

# 2. postgres with a volume and a published port
ply run postgres@17 -e POSTGRES_PASSWORD=dev -p 5432 &
psql -h 127.0.0.1 -p 5432 -U postgres -c 'create table t (x int); insert into t values (1)'
ply stop postgres
ply run postgres@17 -e POSTGRES_PASSWORD=dev -p 5432 &
psql -h 127.0.0.1 -p 5432 -U postgres -c 'select * from t'    # → the row persisted

# 3. a two-member stack: db.ply resolves, {db.url} interpolates
ply up ./stack.toml

# 4. a stop from another terminal lands inside the patience window
ply stop db

# 5. the capability report
ply check
```

- [ ] **Step 2: Lockfile invariance**

Build the same app directory on the Mac and on Linux (Lima) and compare:

Run: `ply build <dir> && shasum ply.lock` on both.
Expected: identical. The kernel pin must not have leaked into a lockfile.

- [ ] **Step 3: Linux is untouched**

Run, in Lima or on the droplet: `make check`, then a rootless two-member stack, and compare its output word for word with the same stack on `main`.

- [ ] **Step 4: Both gates one last time**

Run: `make check`
Run: `cargo check --target aarch64-apple-darwin -p ply-cli && cargo clippy --target aarch64-apple-darwin -p ply-cli -- -D warnings`

- [ ] **Step 5: Report** — the acceptance table, the boot time, and anything the spec promised that this plan did not deliver.

---

## What this plan deliberately does not do

- **Intel Macs, `links`/virtio-fs, `[resources]`, `ply exec` into a VM, Linux `--isolation vm`.** All declared non-goals in the spec. `ply exec` and `admit` say so out loud rather than failing obscurely.
- **Publish the kernel keg.** Task 5 builds it and prints the `ply push` line; pushing to the registry is the owner's.
- **Provide the Apple Developer ID.** Task 16 implements both paths and names the secrets; obtaining the certificate is the owner's.
- **`ply stats` on macOS.** Task 11 makes it report "unknown" honestly instead of zeros. Real numbers are a later release.
