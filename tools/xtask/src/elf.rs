// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Reading ELF64 little-endian files: the kernel `ob` post-processes after
//! linking (its symbol table, `ksyms`, and its relocation table, `krelocs`).

/// Section type of a symbol table.
pub const SHT_SYMTAB: u32 = 2;
/// Section type of a table of relocations with addends.
pub const SHT_RELA: u32 = 4;
/// Section flag: occupies memory when the program runs.
pub const SHF_ALLOC: u64 = 2;
/// Section flag: holds instructions.
pub const SHF_EXECINSTR: u64 = 4;
/// Program header type of a loadable segment.
const PT_LOAD: u32 = 1;

/// One section header.
#[derive(Clone, Debug)]
pub struct Section {
    pub name: String,
    pub kind: u32,
    pub flags: u64,
    pub addr: u64,
    pub offset: u64,
    pub size: u64,
    pub link: u32,
    pub info: u32,
}

/// One loadable segment: where it is linked and where it is loaded.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub vaddr: u64,
    pub paddr: u64,
    pub memsz: u64,
}

/// The parts of an ELF64 little-endian file `ob` reads.
pub struct Elf<'a> {
    pub bytes: &'a [u8],
    pub sections: Vec<Section>,
    pub segments: Vec<Segment>,
}

pub fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

pub fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

pub fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

pub fn c_string(bytes: &[u8], at: usize) -> Option<&str> {
    let tail = bytes.get(at..)?;
    let len = tail.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&tail[..len]).ok()
}

impl<'a> Elf<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Elf<'a>> {
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
                info: read_u32(bytes, at + 44)?,
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
        let phoff = usize::try_from(read_u64(bytes, 0x20)?).ok()?;
        let phentsize = usize::from(read_u16(bytes, 0x36)?);
        let phnum = usize::from(read_u16(bytes, 0x38)?);
        let mut segments = Vec::new();
        for index in 0..phnum {
            let at = phoff + index * phentsize;
            if read_u32(bytes, at)? == PT_LOAD {
                segments.push(Segment {
                    vaddr: read_u64(bytes, at + 16)?,
                    paddr: read_u64(bytes, at + 24)?,
                    memsz: read_u64(bytes, at + 40)?,
                });
            }
        }
        Some(Elf { bytes, sections, segments })
    }

    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    pub fn data(&self, section: &Section) -> Option<&'a [u8]> {
        let start = usize::try_from(section.offset).ok()?;
        self.bytes.get(start..start + usize::try_from(section.size).ok()?)
    }

    /// The load (physical) address of linked address `vaddr`.
    pub fn physical(&self, vaddr: u64) -> Option<u64> {
        self.segments
            .iter()
            .find(|s| (s.vaddr..s.vaddr + s.memsz).contains(&vaddr))
            .map(|s| s.paddr + (vaddr - s.vaddr))
    }

    /// The file offset holding linked address `vaddr`, if a section with
    /// contents in the file covers it.
    pub fn file_offset(&self, vaddr: u64) -> Option<usize> {
        let section = self.sections.iter().find(|s| {
            s.flags & SHF_ALLOC != 0 && s.kind != 8 && (s.addr..s.addr + s.size).contains(&vaddr)
        })?;
        usize::try_from(section.offset + (vaddr - section.addr)).ok()
    }
}
