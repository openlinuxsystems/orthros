//! Writing the kernel symbol table into the linked kernel.
//!
//! The kernel turns code addresses into function names for its backtraces
//! and for `kdb` with a table in the `radix-ksyms` format, kept in the
//! `.ksyms` section of its own image. The linker leaves that section empty
//! (zeros); after every build, `ob` reads the function symbols from the
//! ELF symbol table, demangles them, and writes the encoded table over the
//! section's bytes in the file. The kernel needs no relinking, and the
//! table is loaded with the rest of the image.

use std::path::Path;

use radix_ksyms::Symbol;

use crate::{Context, Result, bail};

/// ELF section type of a symbol table.
const SHT_SYMTAB: u32 = 2;
/// ELF section flag: holds instructions.
const SHF_EXECINSTR: u64 = 4;
/// ELF symbol types kept: functions, and untyped labels from assembly.
const STT_NOTYPE: u8 = 0;
const STT_FUNC: u8 = 2;

/// One section header.
#[derive(Clone, Debug)]
struct Section {
    name: String,
    kind: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
}

/// The parts of an ELF64 little-endian file this module reads.
struct Elf<'a> {
    bytes: &'a [u8],
    sections: Vec<Section>,
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn c_string(bytes: &[u8], at: usize) -> Option<&str> {
    let tail = bytes.get(at..)?;
    let len = tail.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&tail[..len]).ok()
}

impl<'a> Elf<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Elf<'a>> {
        if bytes.get(..4)? != b"\x7fELF" || bytes[4] != 2 || bytes[5] != 1 {
            return None;
        }
        let shoff = usize::try_from(read_u64(bytes, 0x28)?).ok()?;
        let shentsize = usize::from(read_u16(bytes, 0x3a)?);
        let shnum = usize::from(read_u16(bytes, 0x3c)?);
        let shstrndx = usize::from(read_u16(bytes, 0x3e)?);
        let header = |index: usize| -> Option<(u32, Section)> {
            let at = shoff + index * shentsize;
            let section = Section {
                name: String::new(),
                kind: read_u32(bytes, at + 4)?,
                flags: read_u64(bytes, at + 8)?,
                addr: read_u64(bytes, at + 16)?,
                offset: read_u64(bytes, at + 24)?,
                size: read_u64(bytes, at + 32)?,
                link: read_u32(bytes, at + 40)?,
            };
            Some((read_u32(bytes, at)?, section))
        };
        let (_, names) = header(shstrndx)?;
        let mut sections = Vec::with_capacity(shnum);
        for index in 0..shnum {
            let (name_offset, mut section) = header(index)?;
            section.name =
                c_string(bytes, usize::try_from(names.offset + u64::from(name_offset)).ok()?)?
                    .to_string();
            sections.push(section);
        }
        Some(Elf { bytes, sections })
    }

    fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    fn data(&self, section: &Section) -> Option<&'a [u8]> {
        let start = usize::try_from(section.offset).ok()?;
        self.bytes.get(start..start + usize::try_from(section.size).ok()?)
    }

    /// Code symbols as (address, size, raw name, is a function).
    fn code_symbols(&self) -> Option<Vec<(u64, u64, &'a str, bool)>> {
        let symtab = self.sections.iter().find(|s| s.kind == SHT_SYMTAB)?;
        let strtab = self.sections.get(usize::try_from(symtab.link).ok()?)?;
        let (symbols, strings) = (self.data(symtab)?, self.data(strtab)?);
        let mut out = Vec::new();
        for entry in symbols.as_chunks::<24>().0 {
            let name = read_u32(entry, 0)?;
            let kind = entry[4] & 0xf;
            let shndx = usize::from(read_u16(entry, 6)?);
            let (value, size) = (read_u64(entry, 8)?, read_u64(entry, 16)?);
            let in_code = self.sections.get(shndx).is_some_and(|s| s.flags & SHF_EXECINSTR != 0);
            if !in_code || !(kind == STT_FUNC || kind == STT_NOTYPE) || name == 0 {
                continue;
            }
            let name = c_string(strings, usize::try_from(name).ok()?)?;
            // Local labels and the section-boundary symbols of the linker
            // script say nothing about which function an address is in.
            if name.starts_with(".L") || name.starts_with("__kernel_") || name.is_empty() {
                continue;
            }
            out.push((value, size, name, kind == STT_FUNC));
        }
        Some(out)
    }
}

/// The demangled function symbols of `elf` in the same 4 GiB as `near`,
/// sorted, without overlaps: a label without a size extends to the next
/// symbol or the end of its section. (The boot code that runs before the
/// kernel's page tables, in low memory, is left out.)
fn collect(elf: &Elf<'_>, near: u64) -> Result<Vec<(u64, u64, String)>> {
    let Some(mut raw) = elf.code_symbols() else {
        bail!("the kernel has no readable ELF symbol table");
    };
    raw.retain(|s| s.0 >> 32 == near >> 32);
    // Functions before labels at the same address, then by name for a
    // stable order.
    raw.sort_by(|a, b| a.0.cmp(&b.0).then(b.3.cmp(&a.3)).then(a.2.cmp(b.2)));
    raw.dedup_by_key(|s| s.0);
    let code_end = |addr: u64| {
        elf.sections
            .iter()
            .filter(|s| s.flags & SHF_EXECINSTR != 0 && (s.addr..s.addr + s.size).contains(&addr))
            .map(|s| s.addr + s.size)
            .next()
            .unwrap_or(addr)
    };
    let mut out: Vec<(u64, u64, String)> = Vec::with_capacity(raw.len());
    for (i, &(addr, size, name, _)) in raw.iter().enumerate() {
        let next = raw.get(i + 1).map_or(u64::MAX, |s| s.0).min(code_end(addr));
        let size = if size == 0 { next.saturating_sub(addr) } else { size.min(next - addr) };
        if size == 0 {
            continue;
        }
        out.push((addr, size, format!("{:#}", rustc_demangle::demangle(name))));
    }
    Ok(out)
}

/// Copies the kernel at `built` to `output` with its symbol table filled
/// in. Returns how many functions it lists.
pub fn embed(built: &Path, output: &Path) -> Result<usize> {
    let mut bytes = std::fs::read(built).context(|| format!("reading {}", built.display()))?;
    let (count, offset, encoded) = {
        let Some(elf) = Elf::parse(&bytes) else {
            bail!("{} is not a 64-bit little-endian ELF file", built.display());
        };
        let Some(section) = elf.section(".ksyms").cloned() else {
            bail!("the kernel has no .ksyms section to hold its symbol table");
        };
        let functions = collect(&elf, section.addr)?;
        let symbols: Vec<Symbol<'_>> = functions
            .iter()
            .map(|(address, size, name)| Symbol { address: *address, size: *size, name })
            .collect();
        let needed = radix_ksyms::encoded_len(&symbols);
        let room = usize::try_from(section.size).unwrap_or(0);
        if needed > room {
            bail!(
                "the symbol table needs {needed} bytes but .ksyms holds {room}; enlarge it in \
                 kernel/arch/x86_64/src/boot.rs"
            );
        }
        let mut encoded = vec![0u8; room];
        if let Err(err) = radix_ksyms::encode(&symbols, &mut encoded) {
            bail!("encoding the kernel symbol table: {err}");
        }
        (symbols.len(), usize::try_from(section.offset).unwrap_or(usize::MAX), encoded)
    };
    let Some(target) = bytes.get_mut(offset..offset + encoded.len()) else {
        bail!("the .ksyms section lies outside {}", built.display());
    };
    target.copy_from_slice(&encoded);
    if let Some(parent) = output.parent() {
        crate::process::create_dir(parent)?;
    }
    std::fs::write(output, &bytes).context(|| format!("writing {}", output.display()))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ELF64 file with a code section, a .ksyms section and a
    /// symbol table.
    fn tiny_elf(symbols: &[(&str, u8, u64, u64)]) -> Vec<u8> {
        let mut strtab = vec![0u8];
        let mut symtab = vec![0u8; 24];
        for &(name, kind, value, size) in symbols {
            let name_offset = strtab.len() as u32;
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
            let mut entry = [0u8; 24];
            entry[0..4].copy_from_slice(&name_offset.to_le_bytes());
            entry[4] = 0x10 | kind;
            entry[6..8].copy_from_slice(&1u16.to_le_bytes());
            entry[8..16].copy_from_slice(&value.to_le_bytes());
            entry[16..24].copy_from_slice(&size.to_le_bytes());
            symtab.extend_from_slice(&entry);
        }
        let shstrtab = b"\0.text\0.ksyms\0.symtab\0.strtab\0.shstrtab\0".to_vec();
        let text = vec![0x90u8; 0x100];
        let ksyms = vec![0u8; 4096];
        let mut file = vec![0u8; 64];
        let place = |data: &[u8], file: &mut Vec<u8>| {
            let at = file.len() as u64;
            file.extend_from_slice(data);
            (at, data.len() as u64)
        };
        let t = place(&text, &mut file);
        let k = place(&ksyms, &mut file);
        let s = place(&symtab, &mut file);
        let st = place(&strtab, &mut file);
        let sh = place(&shstrtab, &mut file);
        let shoff = file.len() as u64;
        // (name, type, flags, addr, (offset, size), link)
        type Header = (u32, u32, u64, u64, (u64, u64), u32);
        let headers: [Header; 6] = [
            (0, 0, 0, 0, (0, 0), 0),
            (1, 1, 6, 0x1000, t, 0),
            (7, 1, 2, 0x2000, k, 0),
            (14, SHT_SYMTAB, 0, 0, s, 4),
            (22, 3, 0, 0, st, 0),
            (30, 3, 0, 0, sh, 0),
        ];
        for (name, kind, flags, addr, (offset, size), link) in headers {
            let mut h = [0u8; 64];
            h[0..4].copy_from_slice(&name.to_le_bytes());
            h[4..8].copy_from_slice(&kind.to_le_bytes());
            h[8..16].copy_from_slice(&flags.to_le_bytes());
            h[16..24].copy_from_slice(&addr.to_le_bytes());
            h[24..32].copy_from_slice(&offset.to_le_bytes());
            h[32..40].copy_from_slice(&size.to_le_bytes());
            h[40..44].copy_from_slice(&link.to_le_bytes());
            file.extend_from_slice(&h);
        }
        file[0..4].copy_from_slice(b"\x7fELF");
        file[4] = 2;
        file[5] = 1;
        file[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        file[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        file[0x3c..0x3e].copy_from_slice(&6u16.to_le_bytes());
        file[0x3e..0x40].copy_from_slice(&5u16.to_le_bytes());
        file
    }

    #[test]
    fn embeds_demangled_functions_and_sizes_labels() {
        let elf = tiny_elf(&[
            ("_RNvCs1234_5radix4main", STT_FUNC, 0x1000, 0x40),
            ("isr_common", STT_NOTYPE, 0x1080, 0),
            (".Ltmp0", STT_NOTYPE, 0x10a0, 0),
            ("__kernel_text_end", STT_NOTYPE, 0x1100, 0),
        ]);
        let dir = std::env::temp_dir().join(format!("ob-ksyms-{}", std::process::id()));
        let (input, output) = (dir.join("in"), dir.join("out"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&input, &elf).unwrap();
        assert_eq!(embed(&input, &output).unwrap(), 2);

        let patched = std::fs::read(&output).unwrap();
        let parsed = Elf::parse(&patched).unwrap();
        let section = parsed.section(".ksyms").unwrap().clone();
        let table = radix_ksyms::Symbols::parse(parsed.data(&section).unwrap()).unwrap();
        assert_eq!(table.lookup(0x1010).unwrap().to_string(), "radix::main+0x10");
        // The label runs to the end of the code section.
        assert_eq!(table.lookup(0x10ff).unwrap().to_string(), "isr_common+0x7f");
        assert!(table.lookup(0x1050).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
