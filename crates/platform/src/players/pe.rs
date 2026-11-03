//! The CPU of a Windows executable, from its PE header: whether a player can run on this PC. Pure, so it is tested on
//! every system. The file is untrusted: every offset is checked against what was read.

use super::Arch;

/// `IMAGE_FILE_MACHINE_*`, the COFF header's Machine field.
const MACHINE_AMD64: u16 = 0x8664;
const MACHINE_ARM64: u16 = 0xAA64;
const MACHINE_I386: u16 = 0x014C;

/// Where the DOS header keeps `e_lfanew`, the offset of the PE signature.
const E_LFANEW: usize = 0x3C;

/// The CPU an executable is built for. `None` when `data` (the first few KiB of the file: the headers sit at its
/// start) is not a PE file, or its PE header lies beyond what was read. An ARM64EC or ARM64X file says x86_64 here,
/// as its header does: it runs where x64 apps do.
pub fn pe_architecture(data: &[u8]) -> Option<Arch> {
    let b = &data[..data.len().min(4096)];
    if b.get(..2)? != b"MZ" {
        return None;
    }
    let lfanew = u32::from_le_bytes(b.get(E_LFANEW..E_LFANEW + 4)?.try_into().ok()?);
    let pe = usize::try_from(lfanew).ok()?;
    // The signature, then the COFF header, whose first field is Machine.
    if b.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    let at = pe.checked_add(4)?;
    let machine = u16::from_le_bytes(b.get(at..at.checked_add(2)?)?.try_into().ok()?);
    Some(match machine {
        MACHINE_AMD64 => Arch::X86_64,
        MACHINE_ARM64 => Arch::Arm64,
        MACHINE_I386 => Arch::X86,
        _ => Arch::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DOS header pointing at `lfanew`, the signature there, and a COFF header for `machine`.
    fn exe(machine: u16, lfanew: u32) -> Vec<u8> {
        let mut d = vec![0u8; 0x40];
        d[..2].copy_from_slice(b"MZ");
        d[E_LFANEW..E_LFANEW + 4].copy_from_slice(&lfanew.to_le_bytes());
        d.resize(lfanew as usize, 0);
        d.extend(b"PE\0\0");
        d.extend(machine.to_le_bytes());
        d.extend([0u8; 18]);
        d
    }

    #[test]
    fn the_coff_machine_names_the_cpu() {
        assert_eq!(pe_architecture(&exe(MACHINE_AMD64, 0x80)), Some(Arch::X86_64));
        assert_eq!(pe_architecture(&exe(MACHINE_ARM64, 0xF8)), Some(Arch::Arm64));
        assert_eq!(pe_architecture(&exe(MACHINE_I386, 0x40)), Some(Arch::X86));
        assert_eq!(pe_architecture(&exe(0x01C4, 0x80)), Some(Arch::Other), "32-bit Arm");
    }

    #[test]
    fn short_or_foreign_files_are_not_pe() {
        assert_eq!(pe_architecture(&[]), None);
        assert_eq!(pe_architecture(b"MZ"), None, "no room for e_lfanew");
        assert_eq!(pe_architecture(b"#!/bin/sh\necho hi\n"), None);
        let full = exe(MACHINE_AMD64, 0x80);
        assert_eq!(pe_architecture(&full[..0x80 + 4]), None, "cut after the signature");
        assert_eq!(pe_architecture(&full[..0x80 + 5]), None, "cut inside Machine");
        let mut no_mz = full.clone();
        no_mz[0] = b'X';
        assert_eq!(pe_architecture(&no_mz), None);
    }

    #[test]
    fn a_bad_signature_is_not_pe() {
        let mut d = exe(MACHINE_AMD64, 0x80);
        d[0x80 + 2] = b'X';
        assert_eq!(pe_architecture(&d), None);
        let mut ne = exe(MACHINE_I386, 0x80);
        ne[0x80..0x82].copy_from_slice(b"NE");
        assert_eq!(pe_architecture(&ne), None, "a 16-bit New Executable");
    }

    #[test]
    fn e_lfanew_out_of_range_is_not_pe() {
        for lfanew in [0x1000, 0x2000, u32::MAX - 1, u32::MAX] {
            let mut d = exe(MACHINE_AMD64, 0x80);
            d[E_LFANEW..E_LFANEW + 4].copy_from_slice(&lfanew.to_le_bytes());
            assert_eq!(pe_architecture(&d), None, "{lfanew:#x}");
        }
        // A header past the first 4 KiB is not looked for, even when more was read.
        let far = exe(MACHINE_AMD64, 0x1000);
        assert_eq!(pe_architecture(&far), None);
    }
}
