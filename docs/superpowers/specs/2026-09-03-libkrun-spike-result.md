# Milestone 0: the libkrun spike — result

**Date:** 2026-09-03 · **Status:** decided · **Spec:** `docs/superpowers/specs/2026-09-02-macos-native-vm-design.md` (Milestones, row 0) · **Throwaway:** the spike code lives only in the session scratchpad; nothing here lands in the repo.

**Decision rule (from the spec):** adopt libkrun only if it does N-disk rootfs **+** port mapping **+** our kernel; otherwise proceed with plyvm's VMM.

**Verdict: the rule is not met — 2 of 3. Proceed with plyvm's VMM.** Port mapping and our own kernel are mutually exclusive in libkrun, by construction, on both `main` and the current stable branch. The relaxed question ("even so, is libkrun the better base?") is answered below, and the answer is also no.

## What was measured, and on what

| | |
|---|---|
| host | Apple M3 Max, macOS 26.5.2 (25F84), `kern.hv_support: 1` |
| toolchain | rustc 1.97.1, Apple clang 17.0.0, no Homebrew `lld` installed |
| libkrun | `main` @ `6a11683` (v2.0.0-dev), built `--features blk,net`; `stable-1.19.x` re-checked for every constraint below |
| guest kernel | `ply/microvm-kernel` 6.12.0 arm64 Image, 4,866,056 bytes (the keg plyvm installs) |
| guest init | `probe-init`, a static musl PID 1 written for this spike: mounts proc/sys/devtmpfs, then reports cmdline, `/proc/consoles`, `/proc/partitions`, the first 16 bytes of every `/dev/vd*`, and the interfaces — over the console *and* into a writable "report disk" the host reads back |

Reporting to a disk rather than the console is what made the libkrun side measurable at all; see Q1.

## Q1 — does it boot our kernel, or does it require libkrunfw?

**It boots our kernel. libkrunfw is not required. But our kernel gets no usable console under libkrun.**

- `krun_set_kernel(ctx, path, KRUN_KERNEL_FORMAT_RAW, initramfs, cmdline)` is compiled in on aarch64 for non-`tee` builds, and when an external kernel is set libkrun never dlopens `libkrunfw.5.dylib` — the payload load is guarded by `external_kernel.is_none() && kernel_bundle.is_none() && firmware_config.is_none()`.
- Empirically: our 6.12.0 arm64 Image booted all the way to `Run /init as init process`, unpacked the initramfs, ran the init, wrote the report disk, and powered off cleanly.

The console is the catch:

- libkrun's aarch64 FDT declares the PL011 as `compatible = "arm,pl011"` **without** `"arm,primecell"`, so Linux's AMBA bus never binds it. Inside the guest, `/proc/consoles` shows only `pl11` — the earlycon bootconsole — and no `ttyAMA0`. The kernel prints `Warning: unable to open an initial console.`
- Patching libkrun's FDT to add `"arm,primecell"` makes Linux probe the device, and libkrun then aborts the vCPU thread: `panicked at src/hvf/src/lib.rs:664: unsupported mmio len=2`. Its HVF MMIO path has no 16-bit accessor, which the PL011 probe needs. The serial is earlycon-only **by construction**, not by oversight.
- So a real guest console under libkrun means virtio-console (`hvc0`). `kernel/plyvm.config` has no `CONFIG_VIRTIO_CONSOLE`, so today's kernel cannot use it.

A third finding fell out of this and matters for milestone 2 regardless of the decision: **with no console, a Rust `std` init dies before `main`** — std's fd sanitiser opens `/dev/null` for the missing standard fds and aborts if it cannot. plyvm's `mkinitramfs.py` creates only `/dev/console`. Adding `/dev/null` (char 1:3) fixed it; without it the guest died with `Attempted to kill init! exitcode=0x0000000b`.

## Q2 — N block devices in a fixed order?

**Yes, unambiguously — this is libkrun's strongest answer.**

Four disks, each stamped with a distinct 16-byte magic, attached in order. The guest saw them in exactly the insertion order:

```
PROBE: /dev/vda head="PLYDISK-01......"
PROBE: /dev/vdb head="PLYDISK-02......"
PROBE: /dev/vdc head="PLYDISK-03......"
PROBE: /dev/vdd head="PLYREPORT......."
```

The read-only flag is per disk; the writable disk round-tripped the guest's report back into the host file. `krun_add_disk` accepted 64 disks without refusing (the probe stopped counting there).

## Q3 — port forwarding *and* a per-stack L2 network?

**No. They are mutually exclusive, and port mapping additionally needs libkrun's own patched kernel.**

`krun_set_port_map` fills `tsi_port_map` and refuses outright once any virtio-net device exists — identical on `main` and `stable-1.19.x`:

```rust
fn set_port_map(&mut self, new_port_map: HashMap<u16, u16>) -> Result<(), ()> {
    if self.net_index != 0 { return Err(()); }
    self.tsi_port_map.replace(new_port_map);
    Ok(())
}
```

Measured, by asking the library itself:

| case | configuration | `krun_set_port_map` |
|---|---|---|
| A | no net device, no vsock | `-19` ENODEV |
| B | virtio-net (the per-stack L2) | `-19` ENODEV |
| C | virtio-net + explicit vsock | `-22` EINVAL |
| E | vsock, **no** net device | `0` OK |

Port mapping is a property of TSI (Transparent Socket Impersonation), and libkrun's own README lists TSI's first known limitation as: *"Requires a custom kernel (like the one bundled in libkrunfw)."* TSI is an out-of-tree vsock hijack; our 6.12 tinyconfig kernel has no `CONFIG_VSOCKETS` at all. So **port mapping + our kernel is not reachable**, independently of the net-device conflict.

The per-stack L2 half, on the other hand, works well and is worth taking: `krun_add_net_unixstream(path)` had the VMM connect to a stub switch socket and the guest came up with `eth0`. The wire format is a **4-byte big-endian length prefix followed by the raw Ethernet frame** (passt's protocol).

## Q4 — binary size and C dependencies

| | |
|---|---|
| `libkrun.dylib` (blk+net, release) | 3,938,336 bytes (3.9 MiB) |
| today's `ply` binary | 4,495,088 bytes |
| added, as a **second shipped file** | +88% |
| `otool -L` | Hypervisor.framework, libiconv, libSystem — no third-party C libraries |
| crates in the lockfile | libkrun 208 · ply 151 · plyvm 5 |
| license | Apache-2.0 (ply is MIT) — compatible, not a blocker |

Two frictions worth naming:

- The dylib's install_name is `libkrun.2.dylib` with no `@rpath`, so a consumer must patch it (`-headerpad_max_install_names` + `install_name_tool`) or set `DYLD_LIBRARY_PATH`. That is a direct tax on "one binary per platform, no daemon".
- A full `make` on macOS cross-compiles a Linux-musl init and needs Homebrew `lld`; the plain workspace build failed here with `ld: unknown options: --as-needed -Bstatic -Bdynamic ...`. Building only `-p libkrun` sidesteps it. (For contrast, plyvm's guest init cross-compiles with rustc's own `rust-lld` and needs no Homebrew at all.)

## Q5 — boot time

**A dead heat.** Same kernel, same initramfs, same probe init, seven runs each, whole-process wall clock:

| | runs (ms) | median |
|---|---|---|
| libkrun, 4 disks | 63, 52, 50, 51, 51, 49, 50 | **51** |
| plyvm, 1 disk | 54, 54, 51, 51, 51, 49, 47 | **51** |

Guest-side `/proc/uptime` when init started: 0.01 s under libkrun, 0.02 s under plyvm. libkrun brings no boot-time advantage.

## One finding that was not asked for, and outweighs several that were

**`krun_start_enter` never returns.** The spike's `fprintf` after the call never printed, and the process exited with the guest's status from inside the library. This is documented: *"the VMM assumes it has full control of the process, and will call to exit() with the workload's exit code once the microVM shuts down."*

The spec's process model runs the VMM **on threads inside the `ply run` parent**, which also tees logs into the ring, runs the health gate, evaluates `after` conditions, writes `InstanceState`, and supervises restarts. libkrun's contract is incompatible with that: adopting it means a helper child process per instance and re-exec plumbing that the namespace backend does not need.

Related fragility, seen on this machine: `krun_add_serial_console_default(ctx, 0, 1)` aborted the VMM outright — `panicked at src/utils/src/macos/epoll.rs:226: assertion left == right failed, left: -1` — because it `kevent()`s the input fd and stdin was not kqueue-able. Passing `-1` avoids it.

## Recommendation

**Proceed with plyvm's VMM**, restructured into `ply-core/src/runtime/vm/` as the spec's milestone 3 describes.

The spec's decision rule already settles it (2 of 3). But the rule leans on a criterion ply does not strictly need from libkrun — the spec's own design has the `ply run`/`ply up` parent forwarding published ports through its own switch, not the VMM. So the fairer question is: **relaxing port mapping, is libkrun the better base?** Also no:

- Once ply supplies its own kernel, its own guest init (the N-layer overlay, the spec disk, the control channel — none of which is libkrun's init's job), and its own switch (per-stack L2, `<name>.ply` DNS, `BindScope` rules — none of which libkrun has), what is left of libkrun is the HVF vCPU loop, the FDT, virtio-blk and virtio-console. That is precisely what plyvm's ~850 lines already are, working, today, on this hardware.
- For that remainder we would pay: a 3.9 MiB dylib with an install_name to patch, 208 crates, a branch whose own README says the API/ABI is not stable (or a stable branch that lags), a C ABI seam through the middle of a Rust program, a process model that forces a helper child per instance, and the fd/MMIO fragility above.
- What libkrun genuinely offers that plyvm lacks: a harder virtio-blk (flush/sync modes, qcow2), multiport virtio-console, balloon, rng, pause/resume, and someone else's maintenance. All real — and all inside the parts the spec already budgets milestone 3's four days for.

**Take two things from libkrun anyway**, for free:

1. Its virtio-net unixstream framing — **4-byte big-endian length + raw Ethernet frame** — as the `--vswitch` wire protocol, instead of the u16 prefix the spec sketched. It is the format passt and gvproxy already speak, so the switch stays debuggable with existing tools.
2. Its aarch64 FDT as a cross-check for plyvm's, especially the GIC and timer nodes.

## Consequences carried into plan 2

Discovered here, budgeted there rather than found mid-implementation:

1. `kernel/plyvm.config` has **no `CONFIG_EXT4_FS`** — the spec's ext4 volumes need it.
2. It has **no `CONFIG_VIRTIO_CONSOLE`** — the spec's second console (the control channel) needs it.
3. The initramfs must contain **`/dev/null`** (char 1:3), not just `/dev/console`, or a Rust `std` guest init dies before `main` whenever the console is missing or late.
4. plyvm's PL011 DTB node is correct where libkrun's is not (`"arm,pl011", "arm,primecell"`); keep it, and keep the console on `ttyAMA0` for stdout with `hvc0` reserved for the control channel.

## Rulings

- **R0-1** — Adopt plyvm's VMM, not libkrun. *Why:* the spec's rule fails on port-mapping-with-our-kernel, and the relaxed comparison also favours plyvm once ply's own kernel, init and switch are given. *Cost if wrong:* we maintain ~850 lines of VMM and its device models ourselves; the escape hatch is that `Backend` is a trait, so a libkrun-backed `VmBackend` remains a drop-in replacement later.
- **R0-2** — Use libkrun's 4-byte big-endian length framing for `--vswitch`, superseding the spec's "u16 length prefix". *Why:* same protocol as passt/gvproxy, so existing tools can read the wire, at no cost. *Cost if wrong:* two bytes per frame.
- **R0-3** — The spike is throwaway: no libkrun code, no probe-init, and no spike scripts land in the repo. Only this document does. *Why:* the spec calls milestone 0 a throwaway whose deliverable is a written result. *Cost if wrong:* the spike is cheap to rebuild from this document.
