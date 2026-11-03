//! The CPU slices in a Mach-O header: whether an app can run on this Mac. Pure, so it is tested on every system.

use super::Arch;

const CPU_ARM64: u32 = 0x0100_000C;
const CPU_X86_64: u32 = 0x0100_0007;
const CPU_I386: u32 = 7;

fn family(cpu: u32) -> Arch {
    match cpu {
        CPU_ARM64 => Arch::Arm64,
        CPU_X86_64 => Arch::X86_64,
        CPU_I386 => Arch::X86,
        _ => Arch::Other,
    }
}

/// The architectures in the header of a Mach-O file, thin or fat, each once and sorted. `None` when `data` (the first
/// few KiB of the file are enough) is not a Mach-O file at all.
pub fn architectures(data: &[u8]) -> Option<Vec<Arch>> {
    let b = &data[..data.len().min(4096)];
    if b.len() < 8 {
        return None;
    }
    let be32 = |o: usize| b.get(o..o + 4).map(|w| u32::from_be_bytes([w[0], w[1], w[2], w[3]]));
    let le32 = |o: usize| b.get(o..o + 4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
    let magic = be32(0)?;
    let mut out = match magic {
        0xCAFE_BABE | 0xCAFE_BABF => {
            // Fat header, always big-endian: magic, nfat_arch, then one entry per slice (20 bytes; 32 for the 64-bit
            // variant). A Java class file shares 0xCAFEBABE; its next word is a version number (45 and up), a real fat
            // file has a handful of slices.
            let n = be32(4)?;
            if !(1..=16).contains(&n) {
                return None;
            }
            let entry = if magic == 0xCAFE_BABE { 20 } else { 32 };
            (0..n as usize).map(|i| be32(8 + i * entry).map(family)).collect::<Option<Vec<_>>>()?
        }
        _ => {
            // Thin files are in the byte order of their CPU; every Mac that matters is little-endian.
            let le = le32(0)?;
            if le == 0xFEED_FACF || le == 0xFEED_FACE {
                vec![family(le32(4)?)]
            } else if magic == 0xFEED_FACE || magic == 0xFEED_FACF {
                vec![family(be32(4)?)]
            } else {
                return None;
            }
        }
    };
    out.sort();
    out.dedup();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }

    fn thin(cpu: u32) -> Vec<u8> {
        let mut d = 0xFEED_FACFu32.to_le_bytes().to_vec();
        d.extend(cpu.to_le_bytes());
        d.extend([0u8; 24]);
        d
    }

    fn fat(cpus: &[u32], magic: u32) -> Vec<u8> {
        let mut d = be(magic).to_vec();
        d.extend(be(cpus.len() as u32));
        let entry = if magic == 0xCAFE_BABE { 20 } else { 32 };
        for &c in cpus {
            let mut e = be(c).to_vec();
            e.resize(entry, 0);
            d.extend(e);
        }
        d
    }

    #[test]
    fn thin_and_fat_headers_name_their_slices() {
        assert_eq!(architectures(&thin(CPU_ARM64)), Some(vec![Arch::Arm64]));
        assert_eq!(architectures(&thin(CPU_X86_64)), Some(vec![Arch::X86_64]));
        assert_eq!(architectures(&thin(0x12)), Some(vec![Arch::Other]), "32-bit PowerPC");
        assert_eq!(architectures(&thin(CPU_I386)), Some(vec![Arch::X86]), "32-bit Intel");
        assert_eq!(architectures(&fat(&[CPU_X86_64, CPU_ARM64], 0xCAFE_BABE)), Some(vec![Arch::Arm64, Arch::X86_64]));
        assert_eq!(architectures(&fat(&[CPU_X86_64], 0xCAFE_BABE)), Some(vec![Arch::X86_64]));
        assert_eq!(architectures(&fat(&[CPU_ARM64, CPU_X86_64], 0xCAFE_BABF)), Some(vec![Arch::Arm64, Arch::X86_64]));
        // A big-endian thin header (no Mac runs one, but it is read).
        let mut big = be(0xFEED_FACF).to_vec();
        big.extend(be(CPU_X86_64));
        assert_eq!(architectures(&big), Some(vec![Arch::X86_64]));
    }

    #[test]
    fn scripts_java_classes_and_short_files_are_not_mach_o() {
        assert_eq!(architectures(b"#!/bin/sh\necho hi\n"), None);
        assert_eq!(architectures(&[]), None);
        assert_eq!(architectures(&[0xFE, 0xED]), None);
        let mut java = be(0xCAFE_BABE).to_vec();
        java.extend(be(65));
        java.extend([0u8; 64]);
        assert_eq!(architectures(&java), None, "a Java class file");
        let mut cut = be(0xCAFE_BABE).to_vec();
        cut.extend(be(3));
        cut.extend(be(CPU_ARM64));
        assert_eq!(architectures(&cut), None, "a fat header cut short");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn this_macs_own_tools_are_mach_o() {
        let head = std::fs::read("/bin/ls").unwrap();
        assert!(architectures(&head).is_some_and(|a| !a.is_empty()));
        assert_eq!(architectures(&std::fs::read("/etc/hosts").unwrap()), None);
    }
}
