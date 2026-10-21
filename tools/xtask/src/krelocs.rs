// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Writing the kernel relocation table into the linked kernel (KASLR).
//!
//! The kernel is linked at a fixed address with `--emit-relocs`, so its
//! ELF file still lists every place the linker filled in an address. This
//! module keeps the ones the kernel must change when it moves itself at
//! boot: absolute addresses (64-bit, or 32-bit sign-extended) that point
//! into the higher-half image. It writes their load addresses, in the
//! `radix-relocs` format, over the zeros of the image's `.krelocs` section,
//! the same way `ksyms` fills `.ksyms`.

use crate::elf::{Elf, SHF_ALLOC, SHT_RELA, read_u32, read_u64};
use crate::{Result, bail};

/// Where the higher half starts: the kernel is linked in the top 2 GiB.
const HIGHER_HALF: u64 = 0xFFFF_FFFF_8000_0000;

const R_X86_64_NONE: u32 = 0;
const R_X86_64_64: u32 = 1;
const R_X86_64_PC32: u32 = 2;
const R_X86_64_PLT32: u32 = 4;
const R_X86_64_GOTPCREL: u32 = 9;
const R_X86_64_32: u32 = 10;
const R_X86_64_32S: u32 = 11;
const R_X86_64_PC64: u32 = 24;
const R_X86_64_GOTPCRELX: u32 = 41;
const R_X86_64_REX_GOTPCRELX: u32 = 42;

/// The locations to relocate: (64-bit, 32-bit), as sorted load addresses.
pub fn collect(elf: &Elf<'_>) -> Result<(Vec<u32>, Vec<u32>)> {
    let (mut abs64, mut abs32) = (Vec::new(), Vec::new());
    let load_address = |vaddr: u64| -> Result<u32> {
        match elf.physical(vaddr).map(u32::try_from) {
            Some(Ok(phys)) => Ok(phys),
            _ => bail!("the relocation at {vaddr:#x} is not loaded below 4 GiB"),
        }
    };
    for rela in elf.sections.iter().filter(|s| s.kind == SHT_RELA) {
        let Some(target) = elf.sections.get(rela.info as usize) else { continue };
        // Debug information is not loaded, so nothing there moves.
        if target.flags & SHF_ALLOC == 0 {
            continue;
        }
        let Some(entries) = elf.data(rela) else {
            bail!("cannot read relocation section {}", rela.name);
        };
        for entry in entries.as_chunks::<24>().0 {
            let (Some(location), Some(info)) = (read_u64(entry, 0), read_u64(entry, 8)) else {
                continue;
            };
            let kind = info as u32;
            let Some(offset) = elf.file_offset(location) else {
                bail!("relocation at {location:#x} in {} is outside the file", target.name);
            };
            match kind {
                R_X86_64_64 => {
                    let Some(value) = read_u64(elf.bytes, offset) else { continue };
                    if value >= HIGHER_HALF {
                        abs64.push(load_address(location)?);
                    }
                }
                R_X86_64_32S => {
                    let Some(value) = read_u32(elf.bytes, offset) else { continue };
                    // Negative values sign-extend into the top 2 GiB.
                    if (value as i32) < 0 {
                        abs32.push(load_address(location)?);
                    }
                }
                // Relative addresses move with their code; 32-bit
                // zero-extended ones can only reach the low boot code.
                R_X86_64_NONE | R_X86_64_PC32 | R_X86_64_PLT32 | R_X86_64_PC64 | R_X86_64_32 => {}
                R_X86_64_GOTPCREL | R_X86_64_GOTPCRELX | R_X86_64_REX_GOTPCRELX => {}
                other => bail!(
                    "relocation type {other} at {location:#x} in {} is not supported by KASLR",
                    target.name
                ),
            }
        }
    }
    // The global offset table holds absolute addresses with no relocation
    // records of their own.
    if let Some(got) = elf.section(".got")
        && let Some(data) = elf.data(got)
    {
        for (index, slot) in data.as_chunks::<8>().0.iter().enumerate() {
            if u64::from_le_bytes(*slot) >= HIGHER_HALF {
                abs64.push(load_address(got.addr + 8 * index as u64)?);
            }
        }
    }
    abs64.sort_unstable();
    abs64.dedup();
    abs32.sort_unstable();
    abs32.dedup();
    Ok((abs64, abs32))
}

/// Writes the relocation table into the `.krelocs` section of the kernel
/// image `bytes`. Returns how many locations it lists.
pub fn embed(bytes: &mut [u8]) -> Result<usize> {
    let (count, offset, encoded) = {
        let Some(elf) = Elf::parse(bytes) else {
            bail!("the kernel is not a 64-bit little-endian ELF file");
        };
        let Some(section) = elf.section(".krelocs").cloned() else {
            bail!("the kernel has no .krelocs section to hold its relocation table");
        };
        let (abs64, abs32) = collect(&elf)?;
        let room = usize::try_from(section.size).unwrap_or(0);
        let mut encoded = vec![0u8; room];
        if let Err(err) = radix_relocs::encode(&abs64, &abs32, &mut encoded) {
            bail!(
                "writing the relocation table into .krelocs ({room} bytes): {err}; enlarge it in \
                 kernel/arch/x86_64/src/boot.rs"
            );
        }
        (abs64.len() + abs32.len(), usize::try_from(section.offset).unwrap_or(usize::MAX), encoded)
    };
    let Some(target) = bytes.get_mut(offset..offset + encoded.len()) else {
        bail!("the .krelocs section lies outside the kernel file");
    };
    target.copy_from_slice(&encoded);
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ELF file with one loadable section linked in the higher half
    /// and loaded at 1 MiB, a `.krelocs` section and relocations.
    fn tiny_elf(relocations: &[(u64, u32)], data: &[u8]) -> Vec<u8> {
        let base = HIGHER_HALF + 0x10_0000;
        let mut rela = Vec::new();
        for &(location, kind) in relocations {
            rela.extend_from_slice(&location.to_le_bytes());
            rela.extend_from_slice(&u64::from(kind).to_le_bytes());
            rela.extend_from_slice(&0u64.to_le_bytes());
        }
        let shstrtab = b"\0.data\0.krelocs\0.rela.data\0.shstrtab\0".to_vec();
        let mut file = vec![0u8; 64 + 56];
        let mut place = |bytes: &[u8]| {
            let at = file.len() as u64;
            file.extend_from_slice(bytes);
            (at, bytes.len() as u64)
        };
        let d = place(data);
        let k = place(&[0u8; 256]);
        let r = place(&rela);
        let s = place(&shstrtab);
        let shoff = file.len() as u64;
        // (name, type, flags, addr, (offset, size), info)
        type Header = (u32, u32, u64, u64, (u64, u64), u32);
        let headers: [Header; 5] = [
            (0, 0, 0, 0, (0, 0), 0),
            (1, 1, 3, base, d, 0),
            (7, 1, 2, base + 0x1000, k, 0),
            (16, SHT_RELA, 0, 0, r, 1),
            (27, 3, 0, 0, s, 0),
        ];
        for (name, kind, flags, addr, (offset, size), info) in headers {
            let mut h = [0u8; 64];
            h[0..4].copy_from_slice(&name.to_le_bytes());
            h[4..8].copy_from_slice(&kind.to_le_bytes());
            h[8..16].copy_from_slice(&flags.to_le_bytes());
            h[16..24].copy_from_slice(&addr.to_le_bytes());
            h[24..32].copy_from_slice(&offset.to_le_bytes());
            h[32..40].copy_from_slice(&size.to_le_bytes());
            h[44..48].copy_from_slice(&info.to_le_bytes());
            file.extend_from_slice(&h);
        }
        // One PT_LOAD segment covering both sections.
        let mut ph = [0u8; 56];
        ph[0..4].copy_from_slice(&1u32.to_le_bytes());
        ph[16..24].copy_from_slice(&base.to_le_bytes());
        ph[24..32].copy_from_slice(&0x10_0000u64.to_le_bytes());
        ph[40..48].copy_from_slice(&0x2000u64.to_le_bytes());
        file[64..120].copy_from_slice(&ph);
        file[0..4].copy_from_slice(b"\x7fELF");
        file[4] = 2;
        file[5] = 1;
        file[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        file[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        file[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        file[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());
        file[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        file[0x3c..0x3e].copy_from_slice(&5u16.to_le_bytes());
        file[0x3e..0x40].copy_from_slice(&4u16.to_le_bytes());
        file
    }

    #[test]
    fn keeps_absolute_addresses_into_the_image() {
        let base = HIGHER_HALF + 0x10_0000;
        let mut data = Vec::new();
        data.extend_from_slice(&(base + 0x40).to_le_bytes()); // 0: a pointer into the image
        data.extend_from_slice(&0x1234u64.to_le_bytes()); // 8: a low address
        data.extend_from_slice(&((base + 0x80) as u32).to_le_bytes()); // 16: 32S, negative
        data.extend_from_slice(&0x10_0000u32.to_le_bytes()); // 20: 32S, low
        data.extend_from_slice(&0u64.to_le_bytes()); // 24: PC-relative
        let mut elf = tiny_elf(
            &[
                (base, R_X86_64_64),
                (base + 8, R_X86_64_64),
                (base + 16, R_X86_64_32S),
                (base + 20, R_X86_64_32S),
                (base + 24, R_X86_64_PC32),
            ],
            &data,
        );
        assert_eq!(embed(&mut elf).unwrap(), 2);
        let parsed = Elf::parse(&elf).unwrap();
        let table =
            radix_relocs::Table::parse(parsed.data(parsed.section(".krelocs").unwrap()).unwrap())
                .unwrap();
        let locations: Vec<_> = table.locations().collect();
        assert_eq!(
            locations,
            [(radix_relocs::Kind::Abs64, 0x10_0000), (radix_relocs::Kind::Abs32, 0x10_0010)]
        );
        let mut bad = tiny_elf(&[(base, 37)], &data);
        assert!(embed(&mut bad).is_err());
    }
}
