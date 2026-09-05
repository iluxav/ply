//! The machine itself: guest memory, the vCPU loop, the device tree the
//! kernel boots on, and MMIO exit dispatch to the device models.
//!
//! Ported from the plyvm spike (`/Users/iluxav/Documents/plyvm/src/machine.rs`),
//! which boots on this exact hardware. What changed, and why:
//!
//!  * **N virtio-mmio devices instead of two hardcoded ones.** Each gets its
//!    own page-sized window at `VIRTIO_BASE + i * VIRTIO_STRIDE` and its own
//!    SPI at `VIRTIO_INTID_BASE + i`, and the device tree names them **in
//!    attach order** — see [`build_dtb`] for why that order is a contract.
//!  * **The generic half of virtio-mmio lives here** ([`Mmio`], [`Queue`],
//!    [`Device`]) so `blk.rs` and `console.rs` carry only what is specific to
//!    a block device and a console.
//!  * **`boot` does not block.** It spawns the vCPU thread and returns a
//!    [`Running`] handle. The supervisor's main loop owns the instance's
//!    lifetime, so a VMM that blocks — or that calls `exit()`, which is what
//!    disqualified libkrun in milestone 0 — cannot be used here at all.
//!  * **PSCI `SYSTEM_OFF` is handled** (it already was in plyvm; keep it):
//!    the guest init ends with `reboot(RB_POWER_OFF)` and without this the
//!    VM would spin forever after the app exited.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use applevisor::prelude::*;
use vm_fdt::FdtWriter;

use super::switch::FrameSink;
use super::{blk, console, net, pl011};

/// Guest RAM starts here; the kernel Image, the DTB and the initramfs all
/// live inside it at the offsets below.
const RAM_BASE: u64 = 0x4000_0000;
/// 128 MiB into RAM, 8-byte aligned, clear of the kernel's own image.
const DTB_OFFSET: u64 = 0x0800_0000;
/// 256 MiB into RAM.
const INITRD_OFFSET: u64 = 0x1000_0000;
const GICD_GPA: u64 = 0x0800_0000;
/// Apple's redistributor window is larger than 64K, so it does not fit the
/// usual qemu layout; this is the address plyvm established.
const GICR_GPA: u64 = 0x0A00_0000;

/// Base of the virtio-mmio windows. Each device gets a page of its own so
/// the device tree can describe it independently.
pub const VIRTIO_BASE: u64 = 0x0B00_0000;
pub const VIRTIO_STRIDE: u64 = 0x1000;
/// First SPI. GICv3 reports 988 of them, so [`MAX_DEVICES`] is our ceiling,
/// not the hardware's.
pub const VIRTIO_INTID_BASE: u32 = 48;
/// Disks + two consoles + rng. A lockfile with more than ~28 layers is not
/// something this runtime needs to support today, and a hard ceiling beats a
/// silent device-tree overflow.
pub const MAX_DEVICES: usize = 32;

/// Largest virtqueue this VMM will accept from a driver.
const QUEUE_MAX: u32 = 256;

/// Bit 32 of the feature word: this is a modern (1.0) virtio device.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

pub fn device_gpa(index: usize) -> u64 {
    VIRTIO_BASE + (index as u64) * VIRTIO_STRIDE
}

pub fn device_intid(index: usize) -> u32 {
    VIRTIO_INTID_BASE + index as u32
}

/// Create and immediately destroy a VM: the only honest test of whether this
/// process may use Hypervisor.framework (the entitlement is checked at
/// `hv_vm_create`, not at load).
pub fn probe_hypervisor() -> std::result::Result<(), String> {
    VirtualMachine::new().map(|_| ()).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- virtqueue

/// One descriptor as the driver wrote it.
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    /// The DEVICE writes this buffer (`VIRTQ_DESC_F_WRITE`); otherwise the
    /// driver did and we read it.
    pub write: bool,
}

/// One split virtqueue's driver-visible state.
#[derive(Default)]
pub struct Queue {
    pub num: u32,
    pub ready: u32,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    pub last_avail: u16,
}

impl Queue {
    /// The next available chain's head descriptor, or `None` when the driver
    /// has published nothing new.
    pub fn pop(&mut self, ram: &Memory) -> Option<u16> {
        if self.ready == 0 || self.num == 0 || self.avail == 0 {
            return None;
        }
        let avail_idx = ram.read_u16(self.avail + 2).unwrap_or(0);
        if self.last_avail == avail_idx {
            return None;
        }
        let slot = (self.last_avail as u64) % self.num as u64;
        let head = ram.read_u16(self.avail + 4 + slot * 2).unwrap_or(0);
        self.last_avail = self.last_avail.wrapping_add(1);
        Some(head)
    }

    /// Walk a descriptor chain from its head. Bounded by the queue size: a
    /// ring whose `next` links form a cycle would otherwise spin the vCPU
    /// thread forever with no way for anyone to notice.
    pub fn chain(&self, ram: &Memory, head: u16) -> Vec<Desc> {
        let mut out = Vec::new();
        let mut idx = head as u64;
        for _ in 0..self.num.max(1) {
            let d = self.desc + idx * 16;
            let addr = ram.read_u64(d).unwrap_or(0);
            let len = ram.read_u32(d + 8).unwrap_or(0);
            let flags = ram.read_u16(d + 12).unwrap_or(0);
            let next = ram.read_u16(d + 14).unwrap_or(0);
            out.push(Desc {
                addr,
                len,
                write: flags & 2 != 0,
            });
            if flags & 1 == 0 {
                break;
            }
            idx = next as u64;
        }
        out
    }

    /// Publish one finished chain. The entry is written before the index,
    /// because the driver reads the index and trusts everything before it.
    pub fn push_used(&mut self, ram: &mut Memory, head: u16, written: u32) {
        if self.used == 0 || self.num == 0 {
            return;
        }
        let used_idx = ram.read_u16(self.used + 2).unwrap_or(0);
        let slot = (used_idx as u64) % self.num as u64;
        let _ = ram.write_u32(self.used + 4 + slot * 8, head as u32);
        let _ = ram.write_u32(self.used + 4 + slot * 8 + 4, written);
        let _ = ram.write_u16(self.used + 2, used_idx.wrapping_add(1));
    }
}

// ------------------------------------------------------- virtio-mmio common

/// The registers every virtio-mmio device answers identically: the magic,
/// the version, the feature words, the selected queue's addresses, the
/// status. A device model adds only its own config space (`>= 0x100`) and
/// its response to the queue doorbell.
pub struct Mmio {
    device_id: u32,
    features: u64,
    feat_sel: u32,
    drv_feat_sel: u32,
    queue_sel: u32,
    status: u32,
    pub irq_status: u32,
    pub queues: Vec<Queue>,
}

impl Mmio {
    pub fn new(device_id: u32, features: u64, queues: usize) -> Mmio {
        Mmio {
            device_id,
            features: features | VIRTIO_F_VERSION_1,
            feat_sel: 0,
            drv_feat_sel: 0,
            queue_sel: 0,
            status: 0,
            irq_status: 0,
            queues: (0..queues).map(|_| Queue::default()).collect(),
        }
    }

    fn selected(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// `None` means "not a transport register" — the caller's config space.
    pub fn read(&self, off: u64) -> Option<u64> {
        let q = self.queues.get(self.queue_sel as usize);
        Some(match off {
            0x000 => 0x7472_6976, // "virt"
            0x004 => 2,           // modern virtio-mmio
            0x008 => self.device_id as u64,
            0x00c => 0x504c_5900, // vendor: "PLY"
            0x010 => {
                if self.feat_sel == 1 {
                    self.features >> 32
                } else {
                    self.features & 0xffff_ffff
                }
            }
            0x034 => QUEUE_MAX as u64,
            0x044 => q.map(|q| q.ready as u64).unwrap_or(0),
            0x060 => self.irq_status as u64,
            0x070 => self.status as u64,
            0x0fc => 0, // config generation: our config never changes
            _ => return None,
        })
    }

    /// `true` when this was a transport register and has been handled.
    pub fn write(&mut self, off: u64, val: u64) -> bool {
        match off {
            0x014 => self.feat_sel = val as u32,
            0x020 => {} // driver features: we require nothing of the driver
            0x024 => self.drv_feat_sel = val as u32,
            0x030 => self.queue_sel = val as u32,
            0x038 => {
                let num = (val as u32).min(QUEUE_MAX);
                if let Some(q) = self.selected() {
                    q.num = num;
                }
            }
            0x044 => {
                if let Some(q) = self.selected() {
                    q.ready = val as u32;
                }
            }
            0x064 => self.irq_status &= !(val as u32),
            0x070 => self.status = val as u32,
            0x080 => set_lo(self.selected().map(|q| &mut q.desc), val),
            0x084 => set_hi(self.selected().map(|q| &mut q.desc), val),
            0x090 => set_lo(self.selected().map(|q| &mut q.avail), val),
            0x094 => set_hi(self.selected().map(|q| &mut q.avail), val),
            0x0a0 => set_lo(self.selected().map(|q| &mut q.used), val),
            0x0a4 => set_hi(self.selected().map(|q| &mut q.used), val),
            _ => return false,
        }
        let _ = self.drv_feat_sel;
        true
    }

    /// The driver has finished its handshake and the device may serve.
    pub fn driver_ok(&self) -> bool {
        self.status & 4 != 0
    }
}

fn set_lo(field: Option<&mut u64>, val: u64) {
    if let Some(f) = field {
        *f = (*f & !0xffff_ffff) | (val & 0xffff_ffff);
    }
}

fn set_hi(field: Option<&mut u64>, val: u64) {
    if let Some(f) = field {
        *f = (*f & 0xffff_ffff) | (val << 32);
    }
}

/// One MMIO device on the bus. Every method returns whether the device now
/// wants its interrupt raised.
pub trait Device: Send {
    /// What the kernel is told this device is, for the device-tree comment
    /// and the boot log.
    fn kind(&self) -> &'static str;
    fn read(&mut self, off: u64) -> u64;
    fn write(&mut self, off: u64, val: u64, ram: &mut Memory) -> bool;
    /// Host-side work with no guest trap behind it — bytes that arrived for
    /// the guest while it was in WFI. Called on every waker tick.
    fn poll(&mut self, _ram: &mut Memory) -> bool {
        false
    }
}

// --------------------------------------------------------------- virtio-rng

/// virtio-rng (device id 4), one queue.
///
/// Not optional, and not a nicety. Apple Silicon does not advertise
/// `FEAT_RNG`, so the arm64 arch-random path gives the guest nothing, and a
/// microVM has almost no interrupt jitter to harvest — without this device
/// anything calling `getrandom()` early (openssl, node, the JVM) stalls for
/// **seconds** while the kernel's entropy pool fills. The symptom is a VM
/// that looks hung, which is the worst kind of startup bug to debug.
struct VirtioRng {
    mmio: Mmio,
    source: Option<std::fs::File>,
}

impl VirtioRng {
    fn new() -> VirtioRng {
        VirtioRng {
            mmio: Mmio::new(4, 0, 1),
            // The host's own CSPRNG. Opened once: a per-request open would
            // put a syscall pair in the guest's entropy path for nothing.
            source: std::fs::File::open("/dev/urandom").ok(),
        }
    }

    fn serve(&mut self, ram: &mut Memory) -> bool {
        let Some(q) = self.mmio.queues.get_mut(0) else {
            return false;
        };
        let mut did = false;
        while let Some(head) = q.pop(ram) {
            let mut written = 0u32;
            for d in q.chain(ram, head) {
                if !d.write || d.len == 0 {
                    continue;
                }
                let mut buf = vec![0u8; d.len as usize];
                // No /dev/urandom is survivable — the guest's own pool still
                // stirs — so feed it nothing rather than lying with zeroes.
                let filled = match self.source.as_mut() {
                    Some(f) => f.read_exact(&mut buf).is_ok(),
                    None => false,
                };
                if !filled {
                    break;
                }
                let _ = ram.write(d.addr, &buf);
                written += d.len;
            }
            q.push_used(ram, head, written);
            did = true;
        }
        did
    }
}

impl Device for VirtioRng {
    fn kind(&self) -> &'static str {
        "rng"
    }

    fn read(&mut self, off: u64) -> u64 {
        self.mmio.read(off).unwrap_or(0)
    }

    fn write(&mut self, off: u64, val: u64, ram: &mut Memory) -> bool {
        if self.mmio.write(off, val) {
            return false;
        }
        if off == 0x050 && self.serve(ram) {
            self.mmio.irq_status |= 1;
            return true;
        }
        false
    }
}

// ------------------------------------------------------------- device tree

/// A disk to attach, in attach order.
pub struct DiskSpec {
    pub path: PathBuf,
    pub read_only: bool,
}

/// This machine's network card and the switch it is plugged into.
///
/// `None` is a guest with no NIC at all — what every instance had before
/// Task 12, and what an instance still gets if the parent could not start a
/// switch. It boots and runs; it just cannot be published to or reached.
pub struct NetSpec {
    pub mac: [u8; 6],
    /// Frames the guest transmits.
    pub uplink: FrameSink,
    /// Frames the switch has for the guest.
    pub downlink: mpsc::Receiver<Vec<u8>>,
}

/// Everything one microVM needs to exist.
pub struct MachineConfig {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    /// **Order is the contract.** See [`build_dtb`].
    pub disks: Vec<DiskSpec>,
    pub mem_bytes: u64,
    pub net: Option<NetSpec>,
}

/// The kernel cmdline, fixed and short: everything an instance differs by
/// travels in the spec disk, which is neither world-readable inside the
/// guest nor length-limited.
///
/// `reboot=t` and `panic=-1` matter together — a guest that panics must
/// power the machine off rather than sit there, because the host's only
/// other way out is the 10 s ready timeout.
pub const CMDLINE: &str = "earlycon=pl011,mmio,0x09000000 console=ttyAMA0 panic=-1 reboot=t";

/// The device tree the kernel boots on.
///
/// # The order of the `virtio_mmio@` nodes IS the guest's disk order
///
/// Linux names virtio-blk disks `vda`, `vdb`, … in probe order, and probe
/// order for platform devices is device-tree order. Nothing else enforces
/// the mapping, and getting it wrong produces no error at all — just an
/// older layer's binary winning, or a database mounting the wrong disk.
///
/// This is not theoretical: milestone 0 found qemu's `virt` machine handing
/// out virtio-mmio transports in **reverse**, so the layer passed first came
/// up as `/dev/vdb`. ply emits its own device tree precisely so it owns this
/// mapping. `spec_disk::volume_devs` documents the half the guest is told
/// about; this function is the half that makes it true.
///
/// The two consoles come after every disk, in this order:
/// **hvc0 first, then hvc1.** The kernel's hvc index is handed out in
/// registration order too, so the first console node becomes `/dev/hvc0`
/// (the app's stdout) and the second `/dev/hvc1` (the control channel).
fn build_dtb(
    cmdline: &str,
    initrd: (u64, u64),
    ram_base: u64,
    ram_size: u64,
    devices: usize,
) -> std::result::Result<Vec<u8>, String> {
    fn e(what: &'static str) -> impl Fn(vm_fdt::Error) -> String {
        move |err| format!("device tree ({what}): {err}")
    }
    let mut f = FdtWriter::new().map_err(e("new"))?;
    let root = f.begin_node("").map_err(e("root"))?;
    f.property_u32("#address-cells", 2).map_err(e("root"))?;
    f.property_u32("#size-cells", 2).map_err(e("root"))?;
    f.property_string("compatible", "linux,dummy-virt")
        .map_err(e("root"))?;
    f.property_u32("interrupt-parent", 1).map_err(e("root"))?;

    let chosen = f.begin_node("chosen").map_err(e("chosen"))?;
    f.property_string("bootargs", cmdline)
        .map_err(e("chosen"))?;
    // "there is an archive in memory, here": the kernel unpacks it into a
    // RAM filesystem and runs /init from it — no disk, no drivers.
    f.property_u64("linux,initrd-start", initrd.0)
        .map_err(e("chosen"))?;
    f.property_u64("linux,initrd-end", initrd.1)
        .map_err(e("chosen"))?;
    f.end_node(chosen).map_err(e("chosen"))?;

    let mem = f.begin_node("memory@40000000").map_err(e("memory"))?;
    f.property_string("device_type", "memory")
        .map_err(e("memory"))?;
    f.property_array_u64("reg", &[ram_base, ram_size])
        .map_err(e("memory"))?;
    f.end_node(mem).map_err(e("memory"))?;

    let cpus = f.begin_node("cpus").map_err(e("cpus"))?;
    f.property_u32("#address-cells", 1).map_err(e("cpus"))?;
    f.property_u32("#size-cells", 0).map_err(e("cpus"))?;
    let cpu = f.begin_node("cpu@0").map_err(e("cpu"))?;
    f.property_string("device_type", "cpu").map_err(e("cpu"))?;
    f.property_string("compatible", "arm,armv8")
        .map_err(e("cpu"))?;
    f.property_u32("reg", 0).map_err(e("cpu"))?;
    f.end_node(cpu).map_err(e("cpu"))?;
    f.end_node(cpus).map_err(e("cpus"))?;

    // How the guest powers itself off: `reboot(RB_POWER_OFF)` in the guest
    // init becomes an HVC the exit loop below answers.
    let psci = f.begin_node("psci").map_err(e("psci"))?;
    f.property_string("compatible", "arm,psci-1.0")
        .map_err(e("psci"))?;
    f.property_string("method", "hvc").map_err(e("psci"))?;
    f.end_node(psci).map_err(e("psci"))?;

    // The architectural timer: PPIs 13-16, level-low.
    let timer = f.begin_node("timer").map_err(e("timer"))?;
    f.property_string("compatible", "arm,armv8-timer")
        .map_err(e("timer"))?;
    f.property_array_u32("interrupts", &[1, 13, 4, 1, 14, 4, 1, 11, 4, 1, 10, 4])
        .map_err(e("timer"))?;
    f.end_node(timer).map_err(e("timer"))?;

    let gic = f
        .begin_node("interrupt-controller@8000000")
        .map_err(e("gic"))?;
    f.property_string("compatible", "arm,gic-v3")
        .map_err(e("gic"))?;
    f.property_u32("#interrupt-cells", 3).map_err(e("gic"))?;
    f.property_null("interrupt-controller").map_err(e("gic"))?;
    f.property_array_u64("reg", &[GICD_GPA, 0x10000, GICR_GPA, 0xF6_0000])
        .map_err(e("gic"))?;
    f.property_u32("phandle", 1).map_err(e("gic"))?;
    f.end_node(gic).map_err(e("gic"))?;

    let clk = f.begin_node("apb-pclk").map_err(e("clk"))?;
    f.property_string("compatible", "fixed-clock")
        .map_err(e("clk"))?;
    f.property_u32("#clock-cells", 0).map_err(e("clk"))?;
    f.property_u32("clock-frequency", 24_000_000)
        .map_err(e("clk"))?;
    f.property_string("clock-output-names", "clk24mhz")
        .map_err(e("clk"))?;
    f.property_u32("phandle", 2).map_err(e("clk"))?;
    f.end_node(clk).map_err(e("clk"))?;

    for i in 0..devices {
        let gpa = device_gpa(i);
        let node = f
            .begin_node(&format!("virtio_mmio@{gpa:x}"))
            .map_err(e("virtio"))?;
        f.property_string("compatible", "virtio,mmio")
            .map_err(e("virtio"))?;
        f.property_array_u64("reg", &[gpa, VIRTIO_STRIDE])
            .map_err(e("virtio"))?;
        // SPI, edge-rising. The device-tree cell is `intid - 32`.
        f.property_array_u32("interrupts", &[0, device_intid(i) - 32, 1])
            .map_err(e("virtio"))?;
        f.end_node(node).map_err(e("virtio"))?;
    }

    // `arm,primecell` is not decoration: without it Linux's AMBA bus never
    // binds the PL011 and the kernel log stops after earlycon. Milestone 0
    // found libkrun getting exactly this wrong.
    let uart = f.begin_node("pl011@9000000").map_err(e("pl011"))?;
    f.property_string_list(
        "compatible",
        vec!["arm,pl011".into(), "arm,primecell".into()],
    )
    .map_err(e("pl011"))?;
    f.property_array_u64("reg", &[pl011::UART_GPA, pl011::UART_SIZE])
        .map_err(e("pl011"))?;
    f.property_array_u32("interrupts", &[0, 1, 4])
        .map_err(e("pl011"))?;
    f.property_array_u32("clocks", &[2, 2])
        .map_err(e("pl011"))?;
    f.property_string_list("clock-names", vec!["uartclk".into(), "apb_pclk".into()])
        .map_err(e("pl011"))?;
    f.end_node(uart).map_err(e("pl011"))?;

    f.end_node(root).map_err(e("root"))?;
    f.finish().map_err(e("finish"))
}

/// arm64 Image header: text_offset at +8, image_size at +16 (LE u64).
fn image_geometry(image: &[u8]) -> std::result::Result<(u64, u64), String> {
    if image.len() < 64 {
        return Err("the kernel image is too short to carry an arm64 Image header".into());
    }
    let word = |o: usize| {
        u64::from_le_bytes(
            image[o..o + 8]
                .try_into()
                .expect("an 8-byte window inside a 64-byte header"),
        )
    };
    Ok((word(8), word(16)))
}

const XREGS: [Reg; 31] = [
    Reg::X0,
    Reg::X1,
    Reg::X2,
    Reg::X3,
    Reg::X4,
    Reg::X5,
    Reg::X6,
    Reg::X7,
    Reg::X8,
    Reg::X9,
    Reg::X10,
    Reg::X11,
    Reg::X12,
    Reg::X13,
    Reg::X14,
    Reg::X15,
    Reg::X16,
    Reg::X17,
    Reg::X18,
    Reg::X19,
    Reg::X20,
    Reg::X21,
    Reg::X22,
    Reg::X23,
    Reg::X24,
    Reg::X25,
    Reg::X26,
    Reg::X27,
    Reg::X28,
    Reg::X29,
    Reg::LR,
];

// ------------------------------------------------------------- the handle

/// A booted machine, as the host holds it.
///
/// Dropping this tears the VM down: the vCPU thread is asked to stop and
/// joined, which destroys the vCPU, the memory and the VM in that order.
/// Hypervisor.framework allows exactly ONE VM per process, so leaking a
/// machine would make the next `ply run` in this process impossible.
pub struct Running {
    shutdown: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    control: console::ControlHandle,
    stdout: Option<std::fs::File>,
    lines: Option<mpsc::Receiver<ply_vm_proto::GuestLine>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    /// The app's stdout and stderr, byte for byte, as `hvc0` carried it.
    /// Taken once — the supervisor tees it exactly as it tees the namespace
    /// backend's pipe.
    pub fn take_stdout(&mut self) -> Option<std::fs::File> {
        self.stdout.take()
    }

    /// Guest → host control lines, taken once.
    pub fn take_control(&mut self) -> Option<mpsc::Receiver<ply_vm_proto::GuestLine>> {
        self.lines.take()
    }

    /// Host → guest. Best effort: a guest that has stopped reading its
    /// control port must never be the reason a `ply stop` fails.
    pub fn send_control(&self, line: &ply_vm_proto::HostLine) {
        self.control.send(line);
    }

    /// Is the vCPU still running the guest?
    pub fn running(&self) -> bool {
        !self.stopped.load(Ordering::Relaxed)
    }

    /// Tear the machine down now. This is what `Instance::signal(SIGKILL)`
    /// does: there is no process to kill, so the machine itself goes.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------- the boot

/// Boot a machine and return immediately.
///
/// Everything that can fail on the host — reading the kernel, opening the
/// disks, creating the VM, mapping its memory — is reported through this
/// call's `Err`, before the caller is handed a machine it thinks is running.
/// Everything after that lives on the vCPU thread.
pub fn boot(cfg: MachineConfig) -> std::result::Result<Running, String> {
    let kernel = read_file(&cfg.kernel)?;
    let initramfs = read_file(&cfg.initramfs)?;
    let (text_offset, _image_size) = image_geometry(&kernel)?;

    // hvc0 = the app's stdout; hvc1 = the control channel. Two SINGLE-port
    // consoles rather than one two-port device, deliberately: a multiport
    // virtio-console needs the whole control-queue handshake before a port
    // is usable, and Linux only makes `/dev/hvcN` for a port the device
    // announced with VIRTIO_CONSOLE_CONSOLE_PORT. A device with the
    // multiport feature *off* is a console by definition, so the driver
    // registers hvc0 for the first one it probes and hvc1 for the second —
    // the exact device names `ply-guest-init` opens, with none of the
    // handshake to get wrong.
    let (stdout_dev, stdout_reader) = console::VirtioConsole::to_pipe()?;
    let (control_dev, control_handle, control_lines) = console::VirtioConsole::control();

    let mut devices: Vec<Box<dyn Device>> = Vec::new();
    for disk in &cfg.disks {
        devices.push(Box::new(blk::VirtioBlk::open(&disk.path, disk.read_only)?));
    }
    let disk_count = devices.len();
    devices.push(Box::new(stdout_dev));
    devices.push(Box::new(control_dev));
    devices.push(Box::new(VirtioRng::new()));
    // Last, deliberately: the disks' positions are a contract with
    // `spec_disk::volume_devs` and the consoles' relative order is what
    // makes hvc0 the app's stdout, so a new device goes on the end where it
    // can disturb neither.
    if let Some(net) = cfg.net {
        devices.push(Box::new(net::VirtioNet::new(
            net.mac,
            net.uplink,
            net.downlink,
        )));
    }
    if devices.len() > MAX_DEVICES {
        return Err(format!(
            "this instance needs {} virtio devices ({disk_count} disks, two consoles, an rng \
             and a NIC) and the machine has room for {MAX_DEVICES}",
            devices.len()
        ));
    }
    let device_count = devices.len();
    // The one place the attach order is visible to a person. `PLY_VM_DEBUG=1`
    // prints it next to the guest's own `/proc/partitions`, which is how a
    // mismatch between this list and `/dev/vdX` gets found at all.
    if std::env::var_os("PLY_VM_DEBUG").is_some() {
        for (i, dev) in devices.iter().enumerate() {
            eprintln!(
                "ply: virtio device {i} @ {:#x} intid {} = {}",
                device_gpa(i),
                device_intid(i),
                dev.kind()
            );
        }
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();

    let mem_bytes = cfg.mem_bytes;
    let thread_shutdown = shutdown.clone();
    let thread_stopped = stopped.clone();
    let thread = std::thread::Builder::new()
        .name("ply-vcpu".into())
        .stack_size(1 << 20)
        .spawn(move || {
            // The vCPU is bound to the thread that created it, so every
            // Hypervisor.framework object this machine owns is born and dies
            // here.
            let outcome = run_machine(
                kernel,
                initramfs,
                text_offset,
                mem_bytes,
                device_count,
                devices,
                &thread_shutdown,
                &ready_tx,
            );
            thread_stopped.store(true, Ordering::Relaxed);
            match outcome {
                Ok(reason) => {
                    if std::env::var_os("PLY_VM_DEBUG").is_some() {
                        eprintln!("ply: microVM {reason}");
                    }
                }
                Err(e) => {
                    // Setup errors already went back through `ready_tx`; this
                    // is a failure mid-run, which nothing else would report.
                    let _ = ready_tx.send(Err(e.clone()));
                    eprintln!("ply: microVM stopped: {e}");
                }
            }
        })
        .map_err(|e| format!("spawning the vCPU thread: {e}"))?;

    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let _ = thread.join();
            return Err(e);
        }
        Err(_) => {
            let _ = thread.join();
            return Err("the vCPU thread ended before the machine started".into());
        }
    }

    Ok(Running {
        shutdown,
        stopped,
        control: control_handle,
        stdout: Some(stdout_reader),
        lines: Some(control_lines),
        thread: Some(thread),
    })
}

fn read_file(path: &Path) -> std::result::Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

/// The vCPU thread's whole life. Ported from plyvm's `machine::boot`.
#[allow(clippy::too_many_arguments)]
fn run_machine(
    kernel: Vec<u8>,
    initramfs: Vec<u8>,
    text_offset: u64,
    mem_bytes: u64,
    device_count: usize,
    mut devices: Vec<Box<dyn Device>>,
    shutdown: &Arc<AtomicBool>,
    ready: &mpsc::Sender<std::result::Result<(), String>>,
) -> std::result::Result<String, String> {
    // Every step up to `ready.send(Ok(()))` reports through `ready`; a
    // closure keeps that from being said eight times.
    let setup = (|| -> std::result::Result<_, String> {
        let dtb_gpa = RAM_BASE + DTB_OFFSET;
        let initrd_gpa = RAM_BASE + INITRD_OFFSET;

        let mut gic_config = GicConfig::new();
        gic_config
            .set_distributor_base(GICD_GPA)
            .map_err(|e| format!("gic distributor: {e}"))?;
        gic_config
            .set_redistributor_base(GICR_GPA)
            .map_err(|e| format!("gic redistributor: {e}"))?;
        let vm =
            VirtualMachine::with_gic(VirtualMachineConfig::new(), gic_config).map_err(|e| {
                format!(
                    "hv_vm_create with a GIC failed ({e}) — this needs an Apple Silicon Mac on \
                 macOS 15+, and a `ply` binary signed with com.apple.security.hypervisor"
                )
            })?;
        let mut ram = vm
            .memory_create(mem_bytes as usize)
            .map_err(|e| format!("allocating {} MiB of guest RAM: {e}", mem_bytes >> 20))?;
        ram.map(RAM_BASE, MemPerms::RWX)
            .map_err(|e| format!("mapping guest RAM: {e}"))?;

        let kernel_gpa = RAM_BASE + text_offset;
        ram.write(kernel_gpa, &kernel)
            .map_err(|e| format!("loading the kernel: {e}"))?;
        ram.write(initrd_gpa, &initramfs)
            .map_err(|e| format!("loading the initramfs: {e}"))?;
        let dtb = build_dtb(
            CMDLINE,
            (initrd_gpa, initrd_gpa + initramfs.len() as u64),
            RAM_BASE,
            mem_bytes,
            device_count,
        )?;
        ram.write(dtb_gpa, &dtb)
            .map_err(|e| format!("loading the device tree: {e}"))?;

        let vcpu = vm
            .vcpu_create()
            .map_err(|e| format!("creating the vCPU: {e}"))?;
        // The GIC routes by affinity; a vCPU that does not know its
        // coordinates (bit 31 RES1, affinity 0) matches no redistributor.
        vcpu.set_sys_reg(SysReg::MPIDR_EL1, 0x8000_0000)
            .map_err(|e| format!("mpidr: {e}"))?;
        let _ = vcpu.set_trap_debug_exceptions(false);
        let _ = vcpu.set_trap_debug_reg_accesses(false);
        vcpu.set_reg(Reg::CPSR, 0x3C5)
            .map_err(|e| format!("cpsr: {e}"))?; // EL1h, DAIF masked
        vcpu.set_reg(Reg::X0, dtb_gpa)
            .map_err(|e| format!("x0: {e}"))?;
        vcpu.set_reg(Reg::PC, kernel_gpa)
            .map_err(|e| format!("pc: {e}"))?;
        Ok((vm, ram, vcpu))
    })();

    let (vm, mut ram, vcpu) = match setup {
        Ok(parts) => {
            let _ = ready.send(Ok(()));
            parts
        }
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };

    // A steady 5 ms waker returns control to us even while the guest sleeps
    // in WFI, so host-side bytes reach it and `shutdown` is noticed promptly.
    let handle = vcpu.get_handle();
    let vm_waker = vm.clone();
    let waker_shutdown = shutdown.clone();
    let waker_done = Arc::new(AtomicBool::new(false));
    let waker_done_thread = waker_done.clone();
    let waker = std::thread::Builder::new()
        .name("ply-vcpu-waker".into())
        .spawn(move || {
            while !waker_done_thread.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(5));
                let _ = vm_waker.vcpus_exit(std::slice::from_ref(&handle));
                if waker_shutdown.load(Ordering::Relaxed) {
                    break;
                }
            }
            let _ = vm_waker.vcpus_exit(std::slice::from_ref(&handle));
        })
        .map_err(|e| format!("spawning the waker thread: {e}"))?;

    let outcome = vcpu_loop(&vm, &vcpu, &mut ram, &mut devices, device_count, shutdown);
    waker_done.store(true, Ordering::Relaxed);
    let _ = waker.join();
    outcome
}

fn vcpu_loop(
    vm: &VirtualMachineInstance<GicEnabled>,
    vcpu: &Vcpu,
    ram: &mut Memory,
    devices: &mut [Box<dyn Device>],
    device_count: usize,
    shutdown: &Arc<AtomicBool>,
) -> std::result::Result<String, String> {
    let xreg = |n: u32| -> Reg { XREGS[n as usize] };
    let virtio_end = VIRTIO_BASE + (device_count as u64) * VIRTIO_STRIDE;
    let mut exits: u64 = 0;

    loop {
        vcpu.run().map_err(|e| format!("hv_vcpu_run: {e}"))?;
        exits += 1;
        let e = vcpu.get_exit_info();
        if e.reason == ExitReason::CANCELED {
            // The waker's tick: the only place host-originated work happens.
            for (i, dev) in devices.iter_mut().enumerate() {
                if dev.poll(ram) {
                    let intid = device_intid(i);
                    let _ = vm.gic_set_spi(intid, true);
                    let _ = vm.gic_set_spi(intid, false);
                }
            }
            if shutdown.load(Ordering::Relaxed) {
                return Ok(format!("torn down by the host after {exits} exits"));
            }
            continue;
        }
        let syndrome = e.exception.syndrome;
        let ec = (syndrome >> 26) & 0x3F;
        match ec {
            // Data abort = MMIO. The syndrome names the register, the size
            // and the direction.
            0x24 => {
                let gpa = e.exception.physical_address;
                let write = (syndrome >> 6) & 1 == 1;
                let srt = ((syndrome >> 16) & 0x1F) as u32;
                let uart = (pl011::UART_GPA..pl011::UART_GPA + pl011::UART_SIZE).contains(&gpa);
                let device = if (VIRTIO_BASE..virtio_end).contains(&gpa) {
                    Some(((gpa - VIRTIO_BASE) / VIRTIO_STRIDE) as usize)
                } else {
                    None
                };
                if write {
                    let val = if srt == 31 {
                        0
                    } else {
                        vcpu.get_reg(xreg(srt)).unwrap_or(0)
                    };
                    if uart {
                        pl011::write(gpa - pl011::UART_GPA, val);
                    } else if let Some(i) = device {
                        let off = gpa - device_gpa(i);
                        if devices[i].write(off, val, ram) {
                            let intid = device_intid(i);
                            let _ = vm.gic_set_spi(intid, true);
                            let _ = vm.gic_set_spi(intid, false);
                        }
                    }
                } else if srt != 31 {
                    let value = if uart {
                        pl011::read(gpa - pl011::UART_GPA)
                    } else if let Some(i) = device {
                        devices[i].read(gpa - device_gpa(i))
                    } else {
                        0
                    };
                    let _ = vcpu.set_reg(xreg(srt), value);
                }
                let pc = vcpu.get_reg(Reg::PC).unwrap_or(0);
                let _ = vcpu.set_reg(Reg::PC, pc + 4);
            }
            // HVC = PSCI. SYSTEM_OFF / SYSTEM_RESET is the guest saying it
            // is done — `ply-guest-init` ends with `reboot(RB_POWER_OFF)`.
            0x16 => {
                let func = vcpu.get_reg(Reg::X0).unwrap_or(0) as u32;
                if func == 0x8400_0008 || func == 0x8400_0009 {
                    return Ok(format!("powered off cleanly after {exits} exits"));
                }
                let _ = vcpu.set_reg(Reg::X0, (-1i64) as u64); // NOT_SUPPORTED
            }
            // A trapped system register: reads-as-zero, writes ignored.
            0x18 => {
                let read = syndrome & 1 == 1;
                let rt = ((syndrome >> 5) & 0x1F) as u32;
                if read && rt != 31 {
                    let _ = vcpu.set_reg(xreg(rt), 0);
                }
                let pc = vcpu.get_reg(Reg::PC).unwrap_or(0);
                let _ = vcpu.set_reg(Reg::PC, pc + 4);
            }
            // WFI/WFE: the waker returns control on its own.
            0x01 => {}
            _ => {
                let pc = vcpu.get_reg(Reg::PC).unwrap_or(0);
                return Err(format!(
                    "unhandled guest exit EC={ec:#x} syndrome={syndrome:#x} PC={pc:#x} \
                     after {exits} exits"
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_device_gets_its_own_page_and_its_own_interrupt() {
        // Overlapping windows would make two devices answer one address, and
        // a shared SPI would make one device's completion look like the
        // other's — both silent, both catastrophic.
        assert_eq!(device_gpa(0), VIRTIO_BASE);
        assert_eq!(device_gpa(1), VIRTIO_BASE + 0x1000);
        assert_eq!(device_intid(0), 48);
        assert_eq!(device_intid(1), 49);
        // Every window the machine hands out, against every fixed window it
        // already has. An overlap is not a crash, it is two devices sharing
        // one address for the life of the VM.
        let ranges = [
            ("the PL011", pl011::UART_GPA, pl011::UART_SIZE),
            ("the GIC distributor", GICD_GPA, 0x10000),
            ("the GIC redistributors", GICR_GPA, 0xF6_0000),
        ];
        let first = device_gpa(0);
        let last = device_gpa(MAX_DEVICES - 1) + VIRTIO_STRIDE;
        for (what, base, size) in ranges {
            assert!(
                last <= base || first >= base + size,
                "the virtio window {first:#x}..{last:#x} must not overlap {what}"
            );
        }
    }

    #[test]
    fn the_device_tree_names_one_virtio_node_per_device_in_attach_order() {
        // Node ORDER is the guest's vda/vdb order, and nothing downstream can
        // detect a permutation — see this function's own doc comment.
        let dtb = build_dtb("console=ttyAMA0", (0x1000, 0x2000), RAM_BASE, 1 << 29, 3)
            .expect("the device tree builds");
        let text = String::from_utf8_lossy(&dtb);
        let mut at = 0usize;
        for i in 0..3 {
            let name = format!("virtio_mmio@{:x}", device_gpa(i));
            let found = text[at..]
                .find(&name)
                .unwrap_or_else(|| panic!("{name} is in the device tree"));
            at += found + name.len();
        }
        assert!(
            text.contains("arm,primecell"),
            "without arm,primecell Linux never binds the PL011 and the kernel log stops \
             after earlycon"
        );
    }

    #[test]
    fn the_cmdline_carries_the_early_console_and_a_panic_that_powers_off() {
        assert!(CMDLINE.contains("earlycon=pl011,mmio,0x09000000"));
        assert!(CMDLINE.contains("console=ttyAMA0"));
        // A guest that panics and then sits there is a `ply run` that hangs.
        assert!(CMDLINE.contains("panic=-1"));
        assert!(CMDLINE.contains("reboot=t"));
    }
}
