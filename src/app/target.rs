//! Which native target a runtime executable is for, read from its header, so
//! `neyuki build` bundles only the package libraries that runtime loads.
//!
//! Targets are named as `pkg::native::current_target` names them:
//! `x86_64-linux`, `aarch64-macos`, `x86_64-windows`, `x86_64-linux-musl`.

/// The targets `exe` can run as. A universal macOS binary holds several
/// architectures, and a statically linked Linux executable may be glibc or
/// musl, so both are listed. `None` when the format or architecture is not
/// one this code knows; the caller then keeps every library.
pub fn runtime_targets(exe: &[u8]) -> Option<Vec<String>> {
    let targets = match exe.get(..4)? {
        [b'M', b'Z', _, _] => vec![format!("{}-windows", pe_arch(exe)?)],
        [0x7f, b'E', b'L', b'F'] => elf_targets(exe)?,
        [0xca, 0xfe, 0xba, 0xbe] => fat_macho_targets(exe)?,
        [0xfe, 0xed, 0xfa, 0xce | 0xcf] => vec![macho_target(be_u32(exe, 4)?)?],
        [0xce | 0xcf, 0xfa, 0xed, 0xfe] => vec![macho_target(le_u32(exe, 4)?)?],
        _ => return None,
    };
    Some(targets)
}

fn le_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The machine field of the PE header that `e_lfanew` points to.
fn pe_arch(exe: &[u8]) -> Option<&'static str> {
    let pe = le_u32(exe, 0x3c)? as usize;
    if exe.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    match le_u16(exe, pe + 4)? {
        0x8664 => Some("x86_64"),
        0xaa64 => Some("aarch64"),
        0x014c => Some("x86"),
        _ => None,
    }
}

/// `cputype` of a Mach-O image.
fn macho_target(cpu: u32) -> Option<String> {
    let arch = match cpu {
        0x0100_0007 => "x86_64",
        0x0100_000c => "aarch64",
        _ => return None,
    };
    Some(format!("{}-macos", arch))
}

/// Every architecture of a universal binary (`fat_header` and `fat_arch`
/// entries are big-endian). Unknown slices are skipped.
fn fat_macho_targets(exe: &[u8]) -> Option<Vec<String>> {
    let count = be_u32(exe, 4)? as usize;
    // Java class files share the magic; real universal binaries hold a few.
    if count == 0 || count > 16 {
        return None;
    }
    let targets: Vec<String> = (0..count)
        .filter_map(|i| macho_target(be_u32(exe, 8 + i * 20)?))
        .collect();
    (!targets.is_empty()).then_some(targets)
}

/// The ELF target: architecture from `e_machine`, OS from `EI_OSABI`, and
/// for Linux the C library from the program interpreter.
fn elf_targets(exe: &[u8]) -> Option<Vec<String>> {
    let wide = match exe.get(4)? {
        1 => false,
        2 => true,
        _ => return None,
    };
    // Every target Neyuki ships for is little-endian.
    if *exe.get(5)? != 1 {
        return None;
    }
    let arch = match (le_u16(exe, 18)?, wide) {
        (0x3e, true) => "x86_64",
        (0xb7, true) => "aarch64",
        (0xf3, true) => "riscv64",
        (0x102, true) => "loongarch64",
        (0x03, false) => "x86",
        (0x28, false) => "arm",
        _ => return None,
    };
    let os = match exe.get(7)? {
        // Linux toolchains leave EI_OSABI at System V.
        0 | 3 => "linux",
        9 => return Some(vec![format!("{}-freebsd", arch)]),
        _ => return None,
    };
    let glibc = format!("{}-{}", arch, os);
    let musl = format!("{}-{}-musl", arch, os);
    Some(match elf_interpreter(exe, wide) {
        Some(interp) if interp.windows(4).any(|w| w == b"musl") => vec![musl],
        Some(_) => vec![glibc],
        None => vec![glibc, musl],
    })
}

/// The `PT_INTERP` path, which names the dynamic loader (`ld-linux-*` for
/// glibc, `ld-musl-*` for musl). A static executable has none.
fn elf_interpreter(exe: &[u8], wide: bool) -> Option<&[u8]> {
    let word = |at: usize| -> Option<usize> {
        if wide {
            let b = exe.get(at..at + 8)?;
            usize::try_from(u64::from_le_bytes(b.try_into().ok()?)).ok()
        } else {
            le_u32(exe, at).map(|n| n as usize)
        }
    };
    let (phoff, phentsize, phnum) = if wide {
        (word(32)?, le_u16(exe, 54)?, le_u16(exe, 56)?)
    } else {
        (word(28)?, le_u16(exe, 42)?, le_u16(exe, 44)?)
    };
    for i in 0..phnum as usize {
        let ph = phoff.checked_add(i.checked_mul(phentsize as usize)?)?;
        const PT_INTERP: u32 = 3;
        if le_u32(exe, ph)? != PT_INTERP {
            continue;
        }
        let (offset, size) = if wide {
            (word(ph + 8)?, word(ph + 32)?)
        } else {
            (word(ph + 4)?, word(ph + 16)?)
        };
        return exe.get(offset..offset.checked_add(size)?);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-bit little-endian ELF header with one program header, which is
    /// `PT_INTERP` naming `interp` when one is given.
    fn elf(machine: u16, osabi: u8, interp: Option<&str>) -> Vec<u8> {
        let mut b = vec![0u8; 64 + 56];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[7] = osabi;
        b[18..20].copy_from_slice(&machine.to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes());
        b[54..56].copy_from_slice(&56u16.to_le_bytes());
        b[56..58].copy_from_slice(&1u16.to_le_bytes());
        // PT_LOAD unless there is an interpreter.
        let kind: u32 = if interp.is_some() { 3 } else { 1 };
        b[64..68].copy_from_slice(&kind.to_le_bytes());
        if let Some(interp) = interp {
            let offset = b.len() as u64;
            b[64 + 8..64 + 16].copy_from_slice(&offset.to_le_bytes());
            b[64 + 32..64 + 40].copy_from_slice(&(interp.len() as u64).to_le_bytes());
            b.extend_from_slice(interp.as_bytes());
        }
        b
    }

    fn pe(machine: u16) -> Vec<u8> {
        let mut b = vec![0u8; 0x80 + 6];
        b[..2].copy_from_slice(b"MZ");
        b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        b
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reads_windows_machines() {
        assert_eq!(
            runtime_targets(&pe(0x8664)),
            Some(names(&["x86_64-windows"]))
        );
        assert_eq!(
            runtime_targets(&pe(0xaa64)),
            Some(names(&["aarch64-windows"]))
        );
        assert_eq!(runtime_targets(&pe(0x1234)), None);
        assert_eq!(runtime_targets(b"MZ"), None);
    }

    #[test]
    fn reads_linux_libc_from_the_interpreter() {
        let glibc = elf(0x3e, 0, Some("/lib64/ld-linux-x86-64.so.2\0"));
        assert_eq!(runtime_targets(&glibc), Some(names(&["x86_64-linux"])));
        let musl = elf(0xb7, 0, Some("/lib/ld-musl-aarch64.so.1\0"));
        assert_eq!(runtime_targets(&musl), Some(names(&["aarch64-linux-musl"])));
        let fixed = elf(0x3e, 0, None);
        assert_eq!(
            runtime_targets(&fixed),
            Some(names(&["x86_64-linux", "x86_64-linux-musl"])),
            "a static executable may be either"
        );
        assert_eq!(
            runtime_targets(&elf(0x3e, 9, None)),
            Some(names(&["x86_64-freebsd"]))
        );
        assert_eq!(runtime_targets(&elf(0x9999, 0, None)), None);
    }

    #[test]
    fn reads_macos_and_universal_binaries() {
        let mut thin = vec![0xcf, 0xfa, 0xed, 0xfe];
        thin.extend_from_slice(&0x0100_000cu32.to_le_bytes());
        assert_eq!(runtime_targets(&thin), Some(names(&["aarch64-macos"])));

        let mut fat = vec![0xca, 0xfe, 0xba, 0xbe];
        fat.extend_from_slice(&2u32.to_be_bytes());
        for cpu in [0x0100_0007u32, 0x0100_000c] {
            fat.extend_from_slice(&cpu.to_be_bytes());
            fat.extend_from_slice(&[0; 16]);
        }
        assert_eq!(
            runtime_targets(&fat),
            Some(names(&["x86_64-macos", "aarch64-macos"]))
        );
    }

    #[test]
    fn knows_the_running_executable() {
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let found = runtime_targets(&exe).expect("the test binary's target");
        assert!(
            found.contains(&crate::pkg::native::current_target()),
            "{:?}",
            found
        );
    }
}
