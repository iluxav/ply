//! The PL011 UART the kernel logs to before virtio-console exists. Boot
//! diagnostics only — the app never sees it.
//!
//! Ported verbatim from the plyvm spike (`src/pl011.rs`), which is known to
//! boot on this hardware, with exactly one change: **writes go to the
//! parent's stderr, never its stdout.** `hvc0` is the app's stdout; if the
//! kernel log shared it, `ply logs` would interleave a boot message with the
//! app's own output and nothing downstream could tell them apart.
//!
//! The device is a lookup table rather than a state machine because the
//! kernel's console driver only ever interrogates it: the flag register
//! before each byte, the AMBA ID registers at probe. We answer what a real
//! PL011 answers and print what it writes.

use std::io::Write as _;

/// qemu-virt's PL011 address, by convention — the kernel cmdline's
/// `earlycon=pl011,mmio,0x09000000` names the same number.
pub const UART_GPA: u64 = 0x0900_0000;
pub const UART_SIZE: u64 = 0x1000;

/// A register read: what a genuine PL011 would return.
pub fn read(off: u64) -> u64 {
    match off {
        0x18 => 0x90, // FR: TXFE|RXFE — outbox empty, inbox empty (we print instantly)
        0xFE0 => 0x11,
        0xFE4 => 0x10,
        0xFE8 => 0x14,
        0xFEC => 0x00, // PeriphID: PL011 r1p4
        0xFF0 => 0x0D,
        0xFF4 => 0xF0,
        0xFF8 => 0x05,
        0xFFC => 0xB1, // PrimeCell ID
        _ => 0,        // baud/control/mask: reads-as-zero
    }
}

/// A register write. Only the data register (offset 0) carries a character;
/// everything else is baud rate and interrupt masks we do not model.
pub fn write(off: u64, val: u64) {
    if off != 0 {
        return;
    }
    let byte = [val as u8];
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(&byte);
    let _ = err.flush();
}
