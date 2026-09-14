//! Classify a compiled native addon (`*.node`) by platform/arch from its
//! header magic, so `ply build` can REFUSE one built for the wrong target —
//! e.g. macOS `node_modules` packed into a Linux image — instead of shipping an
//! image that fails to `require()` the addon at runtime, a break that surfaces
//! far from its cause. We only ever refuse on a POSITIVE identification; an
//! unrecognized header is left alone (never a false refusal).

use crate::image::name::Arch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddonKind {
    ElfX64,
    ElfArm64,
    MachO,
    Pe,
    Unknown,
}

impl AddonKind {
    /// A short label for an error message.
    pub fn label(self) -> &'static str {
        match self {
            AddonKind::ElfX64 => "a linux-x64",
            AddonKind::ElfArm64 => "a linux-arm64",
            AddonKind::MachO => "a macOS (Mach-O)",
            AddonKind::Pe => "a Windows (PE)",
            AddonKind::Unknown => "an unrecognized",
        }
    }

    /// Whether this binary loads under the given (Linux) target arch.
    pub fn matches(self, target: Arch) -> bool {
        matches!(
            (self, target),
            (AddonKind::ElfX64, Arch::X64) | (AddonKind::ElfArm64, Arch::Arm64)
        )
    }

    /// A positively-foreign binary for `target` (a definite refusal). `Unknown`
    /// is never foreign — we do not refuse on a guess.
    pub fn is_foreign_for(self, target: Arch) -> bool {
        !matches!(self, AddonKind::Unknown) && !self.matches(target)
    }
}

/// Classify a binary from the first bytes of its header.
pub fn classify(b: &[u8]) -> AddonKind {
    // ELF: 0x7f 'E' 'L' 'F'; e_machine is a u16 at offset 18, endianness per
    // EI_DATA (byte 5: 2 = big-endian, else little). Linux x64/arm64 are LE.
    if b.len() >= 20 && b[0] == 0x7f && &b[1..4] == b"ELF" {
        let machine = if b[5] == 2 {
            u16::from_be_bytes([b[18], b[19]])
        } else {
            u16::from_le_bytes([b[18], b[19]])
        };
        return match machine {
            0x3e => AddonKind::ElfX64,   // EM_X86_64
            0xb7 => AddonKind::ElfArm64, // EM_AARCH64
            _ => AddonKind::Unknown,
        };
    }
    if b.len() >= 4 {
        // Mach-O thin (FEEDFACE/FEEDFACF, either endianness) and fat
        // (CAFEBABE/BEBAFECA). A `.node` is never a Java class, so treating
        // CAFEBABE as Mach-O fat here is safe.
        const MACHO: [[u8; 4]; 6] = [
            [0xfe, 0xed, 0xfa, 0xce],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xfe, 0xed, 0xfa, 0xcf],
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xca, 0xfe, 0xba, 0xbe],
            [0xbe, 0xba, 0xfe, 0xca],
        ];
        if MACHO.contains(&[b[0], b[1], b[2], b[3]]) {
            return AddonKind::MachO;
        }
    }
    if b.len() >= 2 && b[0] == b'M' && b[1] == b'Z' {
        return AddonKind::Pe;
    }
    AddonKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elf(machine_lo: u8, machine_hi: u8) -> Vec<u8> {
        let mut h = vec![0u8; 20];
        h[0] = 0x7f;
        h[1] = b'E';
        h[2] = b'L';
        h[3] = b'F';
        h[4] = 2; // 64-bit
        h[5] = 1; // little-endian
        h[16] = 2; // ET_DYN-ish; unused by classify
        h[18] = machine_lo;
        h[19] = machine_hi;
        h
    }

    #[test]
    fn classifies_binary_headers() {
        assert_eq!(classify(&elf(0x3e, 0)), AddonKind::ElfX64);
        assert_eq!(classify(&elf(0xb7, 0)), AddonKind::ElfArm64);
        assert_eq!(classify(&elf(0x28, 0)), AddonKind::Unknown); // EM_ARM (32-bit) → not a target
        assert_eq!(
            classify(&[0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0]),
            AddonKind::MachO
        );
        assert_eq!(
            classify(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 0]),
            AddonKind::MachO
        );
        assert_eq!(classify(&[b'M', b'Z', 0, 0]), AddonKind::Pe);
        assert_eq!(classify(&[0, 1, 2]), AddonKind::Unknown);
    }

    #[test]
    fn foreign_and_matches_track_the_target() {
        assert!(AddonKind::ElfX64.matches(Arch::X64));
        assert!(!AddonKind::ElfX64.matches(Arch::Arm64));
        assert!(AddonKind::MachO.is_foreign_for(Arch::X64));
        assert!(AddonKind::ElfArm64.is_foreign_for(Arch::X64));
        assert!(!AddonKind::ElfX64.is_foreign_for(Arch::X64));
        // Unknown is never a refusal.
        assert!(!AddonKind::Unknown.is_foreign_for(Arch::X64));
    }
}
