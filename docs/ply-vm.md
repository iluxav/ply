# ply-vm — requirements (design doc, not scheduled)

A purpose-built microVM monitor for ply. Linux guests only, embedded in the
`ply` binary as the `isolation = "vm"` arm of the existing seam. Goal: the
same composed, content-addressed rootfs entering a hardware-isolated VM
instead of namespaces — on Linux hosts (multi-tenant isolation) and later
macOS/Windows hosts (native `ply run` off-Linux, no Docker-Desktop-style
VM product).

Status 2026-08-20: requirements only, **parked** — decision: Lima is the
macOS middleground for now (docs/macos.md), WSL2 covers Windows. Revisit
when hostile multi-tenancy or native-Mac demand materializes. Queue
position: after the CI deployment verbs (`ply push`/`status`/`rollback`).
Before building, spend one day evaluating **libkrun** (Linux KVM + macOS
HVF as an embeddable library) — building our own wins only if we want the
blk-per-image design, Windows support, or zero C dependencies badly
enough; if libkrun fits, M1+M3 collapse to ~2 weeks of integration.

---

## 1. Product requirements

- **R1 — same UX.** `ply run --isolation vm app.img` (or `isolation = "vm"`
  in ply.toml / host policy). Everything else unchanged: foreground process,
  SIGTERM works, exit code propagates, `ply ps`/`stats`/`deploy` behave
  identically. No daemon appears.
- **R2 — same image, same lockfile.** The exact `.img` files that run under
  namespaces run under VM isolation. No VM-specific build step, no image
  conversion, no second format.
- **R3 — per-instance VM.** One microVM per instance (like one netns per
  instance today). VM dies with the parent process; no VM lifecycle state
  beyond the existing `/run/ply` files.
- **R4 — startup budget.** ≤ 250 ms added over the namespace path
  (Firecracker boots ~125 ms; we target the same class). Memory overhead
  ≤ 40 MiB per instance beyond the app's own usage.
- **R5 — capability detection.** Hosts without virtualization
  (`/dev/kvm` absent — common on cheap shared VPSes; Hyper-V disabled) get
  a loud, actionable error, and `ply check` reports VM capability. `ns`
  remains the default everywhere.
- **R6 — the discipline holds.** No resident helper processes, no
  management daemon, no GUI. One static binary (+ one guest-kernel package).

## 2. Architecture requirements

- **A1 — Linux guests only, direct kernel boot.** No BIOS/UEFI, no ACPI
  beyond the minimum, no legacy devices. Load kernel + cmdline directly
  (x86_64 boot protocol; arm64 Image + DTB).
- **A2 — guest kernel is a ply package.** `microvm-kernel-<ver>-linux-<arch>.img`
  in the official registry: content-addressed, fetched by hash, pinned in
  the lockfile like any dependency. ~5 MiB custom config (virtio-mmio,
  squashfs, overlayfs, ext4; no modules).
- **A3 — rootfs via blk-per-image, not virtio-fs.** Each squashfs from the
  lockfile attaches as its own read-only virtio-blk disk. A ~50-line guest
  init mounts them and assembles the same overlay ply builds on Linux
  (same topological order, tmpfs upper). Content-addressing, store dedup,
  and determinism carry over unchanged. Volumes attach as one writable
  ext4 disk per instance.
- **A4 — devices: exactly three.** virtio-console (stdout/stderr/exit
  code), virtio-blk (N images + 1 volume disk), virtio-net. virtio-fs is
  explicitly out of v1 (dev-mode `--link` is ns-only until v2).
- **A5 — Rust, rust-vmm crates.** `kvm-ioctls`, `vm-memory`,
  `linux-loader`, virtio queue crates on Linux. Platform backends behind
  one trait: KVM (Linux) → HVF (macOS, Apple Silicon first; `hv_gic` for
  interrupts) → WHP (Windows, last — WSL2 already covers Windows ~free).
- **A6 — arch honesty.** No emulation ever: arm64 hosts run arm64 guests,
  x64 hosts run x64 guests. Requires the registry's arm64 track
  (apk2pkg `--arch aarch64`) for Apple Silicon to be useful.
- **A7 — target size.** ~10–15k lines. Every device or feature that pushes
  past that needs a requirement in this file justifying it.

## 3. Networking requirements

- **N1 — Linux hosts:** tap device attached to the existing ply bridge —
  VM instances get IPs from the same 10.77.0.0/16 allocator, appear in
  `ply ps`, hosts entries, and LB emitters exactly like ns instances.
- **N2 — macOS/Windows hosts:** userspace NAT stack (gvisor-tap-vsock
  style; no tap available). Inbound port access via the same emitted-proxy
  story. This is the sleeper cost — budget it as its own milestone.

## 4. Security requirements

- **S1 —** the VM boundary replaces tiers 1–3 for the guest; the VMM
  process itself still runs with minimal privileges (seccomp'd, no
  ambient capabilities beyond /dev/kvm access).
- **S2 —** cgroup limits (`[resources]`) apply to the VMM process, so
  memory/CPU/pids caps keep working with zero manifest changes.
- **S3 —** this is the sanctioned answer to hostile multi-tenancy
  (see TASKS.md "Multi-tenancy"); until it ships, ply makes no
  hostile-tenant claims.

## 5. Platform milestones & cost estimate

| Milestone | Scope | Estimate |
|---|---|---|
| M1 | KVM backend, direct boot, console+blk+net, blk-per-image rootfs, guest kernel package | 4–8 weeks |
| M2 | `isolation = "vm"` wired into run/ps/stats/deploy; capability detection; Linux GA | 2–3 weeks |
| M3 | macOS HVF backend (Apple Silicon), signing/notarization, userspace NAT | 4–8 weeks |
| M4 | Windows WHP backend | 4–8 weeks (or never — WSL2 suffices) |

Solo, realistically: 4–6 months to Linux+macOS solid.

## 6. Non-goals

No x86-on-ARM emulation. No BIOS/UEFI/ACPI completeness. No PCI hotplug,
GPU, USB, graphics, snapshots, or live migration. No multi-guest-OS
support. No VM management CLI (`ply` verbs are the interface). No
Docker-Desktop-style resident VM shared across instances.
