// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A checked reader and loader for ELF64 program images (the System V
//! ABI's ELF format, as the x86-64 psABI uses it).
//!
//! Radix starts programs from statically linked ELF64 executables, of two
//! kinds: fixed ones (`ET_EXEC`), which run at the addresses they were
//! linked at, and position-independent ones (static PIE, `ET_DYN` with no
//! interpreter), which run wherever the loader places them. The loader
//! moves a position-independent program to a random place (address space
//! layout randomization) and applies its relocations itself, so the
//! program needs no code of its own to relocate, and data that is
//! read-only after relocation (`PT_GNU_RELRO`) is mapped read-only from
//! the start.
//!
//! [`read`] checks everything before anyone uses any of it, because an
//! image is data from outside: every offset and size stays inside the
//! file, segments are in order and share no page, none is both writable
//! and executable, the entry point is in an executable segment, and a
//! position-independent image asks for nothing a static loader cannot do
//! (shared libraries, symbol lookups, relocations other than
//! `R_X86_64_RELATIVE`). [`Image::place`] then checks where the image
//! goes, and [`Placed::load`] copies its bytes and applies its
//! relocations through a function the caller gives, so the same code
//! loads programs in the kernel (into frames, by physical address) and in
//! user space (through a mapping). Any input gives a result or an error,
//! never a panic: the property tests feed it garbage.
//!
//! Since Phase 8 the kernel's `exec` also runs programs from files, which
//! it maps rather than copies, so it reads only their first bytes
//! ([`read_program`]): they may then name an interpreter (`PT_INTERP`, the
//! dynamic linker, which does what a static loader cannot) and use
//! thread-local storage, which their C library sets up.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;
use core::ops::Range;

#[cfg(test)]
mod tests;

/// The most loadable segments an image may have.
pub const MAX_SEGMENTS: usize = 8;

/// The most mappings a placed image needs: each segment, and the
/// read-only-after-relocation part of a writable one cut out of it.
pub const MAX_MAPPINGS: usize = MAX_SEGMENTS + 2;

/// The size of a page, which segments are mapped in.
pub const PAGE: u64 = 4096;

const HEADER_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const RELA_SIZE: u64 = 24;
const DYN_SIZE: u64 = 16;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PT_TLS: u32 = 7;
const PT_GNU_STACK: u32 = 0x6474_e551;
const PT_GNU_RELRO: u32 = 0x6474_e552;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;

const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_PLTRELSZ: i64 = 2;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const DT_RELSZ: i64 = 18;
const DT_TEXTREL: i64 = 22;
const DT_FLAGS: i64 = 30;
const DT_RELRSZ: i64 = 35;
const DF_TEXTREL: u64 = 4;

const R_X86_64_NONE: u32 = 0;
const R_X86_64_RELATIVE: u32 = 8;

/// The most entries of a dynamic table that are read: a static PIE has a
/// dozen or so.
const MAX_DYNAMIC: u64 = 256;

/// How a segment's pages are mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Read only.
    Read,
    /// Read and write.
    ReadWrite,
    /// Read and execute.
    ReadExecute,
}

/// Where an image runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// At the addresses it was linked at (`ET_EXEC`).
    Fixed,
    /// Wherever it is placed (a static PIE, `ET_DYN`).
    Relocatable,
}

/// A loadable segment, at the addresses it was linked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Where its first byte goes.
    pub vaddr: u64,
    /// How many bytes it takes in memory; past `filesz` they are zero.
    pub memsz: u64,
    /// Where its bytes start in the file.
    pub offset: u64,
    /// How many bytes come from the file.
    pub filesz: u64,
    /// How its pages are mapped.
    pub access: Access,
}

impl Segment {
    /// The address of its first page.
    pub const fn first_page(&self) -> u64 {
        self.vaddr & !(PAGE - 1)
    }

    /// How many pages it covers.
    pub const fn pages(&self) -> u64 {
        // Checked when the image was read: the end does not overflow.
        (self.vaddr + self.memsz).div_ceil(PAGE) - self.first_page() / PAGE
    }

    /// The address just past its last page.
    const fn end_page(&self) -> u64 {
        self.first_page() + self.pages() * PAGE
    }

    /// Whether the `len` bytes at `address` lie inside it.
    const fn holds(&self, address: u64, len: u64) -> bool {
        address >= self.vaddr
            && match address.checked_add(len) {
                Some(end) => end <= self.vaddr + self.memsz,
                None => false,
            }
    }
}

/// A program image, checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Image {
    /// Where the program starts, as linked.
    pub entry: u64,
    /// Whether it can be moved.
    pub kind: Kind,
    segments: [Option<Segment>; MAX_SEGMENTS],
    /// Its relocations: where the table starts in the file, and how many
    /// entries it has.
    relocations: (u64, u64),
    /// The addresses, as linked, of the data that is read-only once
    /// relocated, if it marks any.
    relro: Option<(u64, u64)>,
}

impl Image {
    /// Its loadable segments, in address order.
    pub fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().flatten()
    }

    /// How many pages its segments cover in all: the pages a loader fills,
    /// one segment after another.
    pub fn pages(&self) -> u64 {
        self.segments().map(Segment::pages).sum()
    }

    /// How many relocations it has.
    pub fn relocation_count(&self) -> u64 {
        self.relocations.1
    }

    /// The addresses it spans as linked, from its first page to the end of
    /// its last.
    pub fn span(&self) -> Range<u64> {
        let first = self.segments().next().map_or(0, Segment::first_page);
        let end = self.segments().last().map_or(0, Segment::end_page);
        first..end
    }

    /// A page-aligned amount to move the image by so that it lies inside
    /// `window`, chosen from every such amount by `random`: 0 for a fixed
    /// image, which cannot move. Fails if the image does not fit.
    pub fn random_bias(&self, window: Range<u64>, random: u64) -> Result<u64, ElfError> {
        if self.kind == Kind::Fixed {
            return Ok(0);
        }
        let span = self.span();
        let start = window.start.checked_next_multiple_of(PAGE).ok_or(ElfError::OutOfRange)?;
        let room = window.end.checked_sub(start).ok_or(ElfError::OutOfRange)?;
        let len = span.end - span.start;
        let places = room.checked_sub(len).ok_or(ElfError::OutOfRange)? / PAGE + 1;
        let place = start + (random % places) * PAGE;
        Ok(place.wrapping_sub(span.start))
    }

    /// The image moved by `bias` (0 for a fixed image), if that keeps it
    /// inside `range`.
    pub fn place(&self, bias: u64, range: Range<u64>) -> Result<Placed<'_>, ElfError> {
        if !bias.is_multiple_of(PAGE) || (self.kind == Kind::Fixed && bias != 0) {
            return Err(ElfError::OutOfRange);
        }
        let span = self.span();
        let start = span.start.checked_add(bias).ok_or(ElfError::OutOfRange)?;
        let end = span.end.checked_add(bias).ok_or(ElfError::OutOfRange)?;
        if start < range.start || end > range.end {
            return Err(ElfError::OutOfRange);
        }
        Ok(Placed { image: self, bias })
    }
}

/// A run of an image's pages to map: from page `page` of what was loaded,
/// `pages` pages at `address`, with `access`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
    /// Where the run goes.
    pub address: u64,
    /// The first of its pages among the loaded ones.
    pub page: u64,
    /// How many pages.
    pub pages: u64,
    /// How they are mapped.
    pub access: Access,
}

/// An image with its place chosen.
#[derive(Clone, Copy, Debug)]
pub struct Placed<'a> {
    image: &'a Image,
    bias: u64,
}

impl Placed<'_> {
    /// How far it was moved from the addresses it was linked at.
    pub fn bias(&self) -> u64 {
        self.bias
    }

    /// Where the program starts.
    pub fn entry(&self) -> u64 {
        self.image.entry.wrapping_add(self.bias)
    }

    /// The pages of segment `index` start at this page of what is loaded:
    /// segments are loaded one after another, so gaps between them take no
    /// memory.
    fn first_page_of(&self, index: usize) -> u64 {
        self.image.segments().take(index).map(Segment::pages).sum()
    }

    /// Where `len` bytes at linked address `address` are in what is
    /// loaded, if they lie inside one segment: that segment and the offset.
    fn locate(&self, address: u64, len: u64) -> Option<(&Segment, u64)> {
        let (index, segment) =
            self.image.segments().enumerate().find(|(_, s)| s.holds(address, len))?;
        let page = self.first_page_of(index);
        Some((segment, page * PAGE + (address - segment.first_page())))
    }

    /// The runs of pages to map, in address order: each segment, with the
    /// pages of a writable one that are read-only once relocated cut out
    /// and mapped read-only.
    pub fn mappings(&self) -> [Option<Mapping>; MAX_MAPPINGS] {
        let mut out = [None; MAX_MAPPINGS];
        let mut count = 0;
        let mut add = |address: u64, page: u64, pages: u64, access| {
            if pages > 0 && count < MAX_MAPPINGS {
                out[count] = Some(Mapping { address, page, pages, access });
                count += 1;
            }
        };
        for (index, segment) in self.image.segments().enumerate() {
            let page = self.first_page_of(index);
            let (first, end) = (segment.first_page(), segment.end_page());
            // Only whole pages of read-only-after-relocation data can be
            // mapped read-only; a page it shares with other data stays
            // writable.
            let relro = match self.image.relro {
                Some((start, stop)) if segment.access == Access::ReadWrite => {
                    let start = start.checked_next_multiple_of(PAGE).unwrap_or(end).max(first);
                    let stop = (stop & !(PAGE - 1)).min(end);
                    (start < stop).then_some((start, stop))
                }
                _ => None,
            };
            let moved = |address: u64| address.wrapping_add(self.bias);
            match relro {
                Some((start, stop)) => {
                    let pages = |from: u64, to: u64| (to - from) / PAGE;
                    add(moved(first), page, pages(first, start), Access::ReadWrite);
                    add(moved(start), page + pages(first, start), pages(start, stop), Access::Read);
                    add(
                        moved(stop),
                        page + pages(first, stop),
                        pages(stop, end),
                        Access::ReadWrite,
                    );
                }
                None => add(moved(first), page, segment.pages(), segment.access),
            }
        }
        out
    }

    /// Loads the image: gives `write` each run of bytes from the file and
    /// each relocated word, with where it goes among the
    /// [`Image::pages`] pages (in bytes from the first), which must start
    /// out zero. `bytes` must be the file the image was read from.
    pub fn load(&self, bytes: &[u8], write: &mut dyn FnMut(u64, &[u8])) -> Result<(), ElfError> {
        for (index, segment) in self.image.segments().enumerate() {
            let at = self.first_page_of(index) * PAGE + segment.vaddr % PAGE;
            // Checked when the image was read: inside the file.
            let start = segment.offset as usize;
            let file =
                bytes.get(start..start + segment.filesz as usize).ok_or(ElfError::Truncated)?;
            write(at, file);
        }
        let (table, count) = self.image.relocations;
        for i in 0..count {
            let entry = (table + i * RELA_SIZE) as usize;
            let entry = bytes.get(entry..entry + RELA_SIZE as usize).ok_or(ElfError::Truncated)?;
            if let Some((at, value)) = self.relocate(entry)? {
                write(at, &value.to_le_bytes());
            }
        }
        Ok(())
    }

    /// What the relocation `entry` (one entry of the table, 24 bytes)
    /// writes: where among the loaded pages, and the word; `None` for
    /// `R_X86_64_NONE`.
    fn relocate(&self, entry: &[u8]) -> Result<Option<(u64, u64)>, ElfError> {
        let r_offset = u64_at(entry, 0)?;
        let r_info = u64_at(entry, 8)?;
        let r_addend = u64_at(entry, 16)?;
        match (r_info & 0xffff_ffff) as u32 {
            R_X86_64_NONE => return Ok(None),
            R_X86_64_RELATIVE if r_info >> 32 == 0 => {}
            _ => return Err(ElfError::Relocation),
        }
        // Only writable data is relocated: text relocations would need
        // code that is written to.
        let (segment, at) = self.locate(r_offset, 8).ok_or(ElfError::Relocation)?;
        if segment.access != Access::ReadWrite {
            return Err(ElfError::Relocation);
        }
        Ok(Some((at, r_addend.wrapping_add(self.bias))))
    }

    /// What the relocation `entry` writes, for a program mapped where it
    /// was placed rather than loaded page after page (`exec`): the address
    /// of the word, and the word; `None` for `R_X86_64_NONE`.
    pub fn relocation(&self, entry: &[u8]) -> Result<Option<(u64, u64)>, ElfError> {
        if entry.len() != RELA_SIZE as usize {
            return Err(ElfError::Malformed);
        }
        self.relocate(entry)?;
        let r_offset = u64_at(entry, 0)?;
        let r_addend = u64_at(entry, 16)?;
        let r_info = u64_at(entry, 8)?;
        if r_info & 0xffff_ffff == u64::from(R_X86_64_NONE) {
            return Ok(None);
        }
        Ok(Some((r_offset.wrapping_add(self.bias), r_addend.wrapping_add(self.bias))))
    }

    /// The size of one relocation entry.
    pub const RELOCATION_SIZE: u64 = RELA_SIZE;
}

/// Why an image was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    /// Shorter than its headers say.
    Truncated,
    /// Not an ELF file.
    NotElf,
    /// Not a 64-bit little-endian x86-64 executable.
    Unsupported,
    /// A header field has a value that makes no sense.
    Malformed,
    /// More than [`MAX_SEGMENTS`] loadable segments.
    TooManySegments,
    /// A segment is outside the range it may be loaded in.
    OutOfRange,
    /// Two segments share a page, or are out of order.
    Overlap,
    /// A segment is both writable and executable, or not readable, or the
    /// stack is to be executable.
    BadAccess,
    /// The entry point is not in an executable segment.
    BadEntry,
    /// It needs a dynamic linker: an interpreter, shared libraries,
    /// relocations through symbols or of code.
    Dynamic,
    /// It has thread-local storage, which the loader does not set up.
    ThreadLocal,
    /// A relocation is of a kind the loader does not apply, or changes
    /// something other than the image's writable data.
    Relocation,
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ElfError::Truncated => "the image is shorter than its headers say",
            ElfError::NotElf => "not an ELF file",
            ElfError::Unsupported => "not a 64-bit little-endian x86-64 executable",
            ElfError::Malformed => "a header field makes no sense",
            ElfError::TooManySegments => "too many loadable segments",
            ElfError::OutOfRange => "a segment lies outside the range programs load in",
            ElfError::Overlap => "two segments share a page or are out of order",
            ElfError::BadAccess => {
                "a segment is writable and executable, or not readable, or the stack executable"
            }
            ElfError::BadEntry => "the entry point is not in an executable segment",
            ElfError::Dynamic => "the image needs a dynamic linker; programs are linked statically",
            ElfError::ThreadLocal => "the image uses thread-local storage, which is not set up",
            ElfError::Relocation => {
                "a relocation is not R_X86_64_RELATIVE, or not in the image's writable data"
            }
        })
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, ElfError> {
    let b = bytes.get(at..at.checked_add(2).ok_or(ElfError::Truncated)?);
    let b = b.ok_or(ElfError::Truncated)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, ElfError> {
    let b = bytes.get(at..at.checked_add(4).ok_or(ElfError::Truncated)?);
    let b = b.ok_or(ElfError::Truncated)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, ElfError> {
    let b = bytes.get(at..at.checked_add(8).ok_or(ElfError::Truncated)?);
    let b = b.ok_or(ElfError::Truncated)?;
    let mut word = [0; 8];
    word.copy_from_slice(b);
    Ok(u64::from_le_bytes(word))
}

/// A program header, as far as the reader needs it.
struct Phdr {
    kind: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

fn phdr_at(bytes: &[u8], at: usize) -> Result<Phdr, ElfError> {
    Ok(Phdr {
        kind: u32_at(bytes, at)?,
        flags: u32_at(bytes, at + 4)?,
        offset: u64_at(bytes, at + 8)?,
        vaddr: u64_at(bytes, at + 16)?,
        filesz: u64_at(bytes, at + 32)?,
        memsz: u64_at(bytes, at + 40)?,
    })
}

/// Reads and checks the image in `bytes`. Where it may go is checked when
/// it is placed ([`Image::place`]).
pub fn read(bytes: &[u8]) -> Result<Image, ElfError> {
    let (mut image, extra) = parse(bytes, bytes.len() as u64, false)?;
    match (image.kind, extra.dynamic) {
        (Kind::Relocatable, Some((offset, size))) => {
            image.relocations = relocations(bytes, &image, offset, size)?;
        }
        // A position-independent image without a dynamic table has nothing
        // to relocate; a fixed one is not relocated.
        (Kind::Relocatable, None) | (Kind::Fixed, _) => {}
    }
    Ok(image)
}

/// A program `exec` runs from a file, read from its first bytes
/// ([`read_program`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Program {
    /// Its image: its segments are mapped from the file. Its relocations
    /// are not read: [`Program::relocations`] finds them.
    pub image: Image,
    /// Where its interpreter's path (with its NUL) is in the file, and how
    /// long it is.
    pub interpreter: Option<(u64, u64)>,
    /// Where its program headers are in memory, as linked, if a segment
    /// holds them.
    pub headers: Option<u64>,
    /// How many program headers it has.
    pub header_count: u16,
    /// Where its dynamic table is in the file, and its size.
    pub dynamic: Option<(u64, u64)>,
}

impl Program {
    /// The size of a program header (AT_PHENT).
    pub const HEADER_SIZE: u64 = PHDR_SIZE as u64;

    /// Reads the dynamic table `table` (its bytes from the file) of a
    /// position-independent program without an interpreter, and returns
    /// where its relocation table is in the file and how many entries it
    /// has, as [`read`] does for an image.
    pub fn relocations(&self, table: &[u8]) -> Result<(u64, u64), ElfError> {
        relocations(table, &self.image, 0, table.len() as u64)
    }
}

/// The most bytes an interpreter's path may take.
pub const INTERPRETER_MAX: u64 = 4096;

/// Reads and checks the program whose file is `file_size` bytes long and
/// starts with `head`, which must hold its ELF header and program
/// headers. Everything is checked as [`read`] checks it, against the
/// file's size, but an interpreter and thread-local storage are allowed.
pub fn read_program(head: &[u8], file_size: u64) -> Result<Program, ElfError> {
    let (image, extra) = parse(head, file_size, true)?;
    Ok(Program {
        image,
        interpreter: extra.interpreter,
        headers: extra.headers,
        header_count: extra.header_count,
        dynamic: extra.dynamic,
    })
}

/// What [`parse`] finds besides the image.
#[derive(Default)]
struct Extra {
    dynamic: Option<(u64, u64)>,
    interpreter: Option<(u64, u64)>,
    headers: Option<u64>,
    header_count: u16,
}

/// Reads the headers in `bytes`, of a file `file_size` bytes long; with
/// `exec`, as [`read_program`] does.
fn parse(bytes: &[u8], file_size: u64, exec: bool) -> Result<(Image, Extra), ElfError> {
    if bytes.len() < HEADER_SIZE {
        return Err(ElfError::Truncated);
    }
    if bytes[..4] != *b"\x7fELF" {
        return Err(ElfError::NotElf);
    }
    // 64-bit, little-endian, version 1.
    if bytes[4] != 2 || bytes[5] != 1 || bytes[6] != 1 {
        return Err(ElfError::Unsupported);
    }
    let kind = match u16_at(bytes, 16)? {
        ET_EXEC => Kind::Fixed,
        ET_DYN => Kind::Relocatable,
        _ => return Err(ElfError::Unsupported),
    };
    if u16_at(bytes, 18)? != EM_X86_64 {
        return Err(ElfError::Unsupported);
    }
    let entry = u64_at(bytes, 24)?;
    let phoff = u64_at(bytes, 32)?;
    let phentsize = u16_at(bytes, 54)?;
    let phnum = u16_at(bytes, 56)?;
    if usize::from(phentsize) != PHDR_SIZE {
        return Err(ElfError::Malformed);
    }
    let table = usize::try_from(phoff).map_err(|_| ElfError::Truncated)?;
    let end = table.checked_add(usize::from(phnum) * PHDR_SIZE).ok_or(ElfError::Truncated)?;
    if end > bytes.len() {
        return Err(ElfError::Truncated);
    }
    let mut segments = [None; MAX_SEGMENTS];
    let mut count = 0;
    let mut next_page = 0;
    let mut extra = Extra { header_count: phnum, ..Extra::default() };
    let mut relro = None;
    let in_file =
        |offset: u64, size: u64| offset.checked_add(size).is_some_and(|end| end <= file_size);
    for i in 0..usize::from(phnum) {
        let phdr = phdr_at(bytes, table + i * PHDR_SIZE)?;
        match phdr.kind {
            PT_LOAD => {}
            PT_DYNAMIC => {
                if !in_file(phdr.offset, phdr.filesz) {
                    return Err(ElfError::Truncated);
                }
                extra.dynamic = Some((phdr.offset, phdr.filesz));
                continue;
            }
            PT_INTERP if exec => {
                if !in_file(phdr.offset, phdr.filesz) {
                    return Err(ElfError::Truncated);
                }
                if phdr.filesz < 2 || phdr.filesz > INTERPRETER_MAX || extra.interpreter.is_some() {
                    return Err(ElfError::Malformed);
                }
                extra.interpreter = Some((phdr.offset, phdr.filesz));
                continue;
            }
            PT_INTERP => return Err(ElfError::Dynamic),
            PT_PHDR if exec => {
                extra.headers = Some(phdr.vaddr);
                continue;
            }
            PT_TLS if exec => continue,
            PT_TLS if phdr.memsz > 0 => return Err(ElfError::ThreadLocal),
            PT_GNU_STACK if phdr.flags & PF_X != 0 => return Err(ElfError::BadAccess),
            PT_GNU_RELRO if phdr.memsz > 0 => {
                let end = phdr.vaddr.checked_add(phdr.memsz).ok_or(ElfError::Malformed)?;
                relro = Some((phdr.vaddr, end));
                continue;
            }
            _ => continue,
        }
        if phdr.memsz == 0 {
            continue;
        }
        if phdr.filesz > phdr.memsz || phdr.offset % PAGE != phdr.vaddr % PAGE {
            return Err(ElfError::Malformed);
        }
        if !in_file(phdr.offset, phdr.filesz) {
            return Err(ElfError::Truncated);
        }
        let mem_end = phdr.vaddr.checked_add(phdr.memsz).ok_or(ElfError::OutOfRange)?;
        if phdr.vaddr & !(PAGE - 1) < next_page {
            return Err(ElfError::Overlap);
        }
        let flags = phdr.flags;
        let access = match (flags & PF_R != 0, flags & PF_W != 0, flags & PF_X != 0) {
            (true, false, false) => Access::Read,
            (true, true, false) => Access::ReadWrite,
            (true, false, true) => Access::ReadExecute,
            _ => return Err(ElfError::BadAccess),
        };
        if count == MAX_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }
        // Rounding up must not wrap: every page of every segment needs an
        // address of its own, which later moves are checked against.
        next_page = mem_end.checked_next_multiple_of(PAGE).ok_or(ElfError::OutOfRange)?;
        segments[count] = Some(Segment {
            vaddr: phdr.vaddr,
            memsz: phdr.memsz,
            offset: phdr.offset,
            filesz: phdr.filesz,
            access,
        });
        count += 1;
    }
    let image = Image { entry, kind, segments, relocations: (0, 0), relro };
    let runs_entry = image
        .segments()
        .any(|s| s.access == Access::ReadExecute && s.vaddr <= entry && entry < s.vaddr + s.memsz);
    if !runs_entry {
        return Err(ElfError::BadEntry);
    }
    // Read-only-after-relocation data lies in one writable segment.
    if let Some((start, end)) = relro {
        let inside =
            image.segments().any(|s| s.access == Access::ReadWrite && s.holds(start, end - start));
        if !inside {
            return Err(ElfError::Malformed);
        }
    }
    if extra.headers.is_none() {
        // The program headers are in memory if a segment loads them.
        let size = u64::from(phnum) * PHDR_SIZE as u64;
        extra.headers = image
            .segments()
            .find(|s| s.offset <= phoff && phoff + size <= s.offset + s.filesz)
            .map(|s| s.vaddr + (phoff - s.offset));
    }
    Ok((image, extra))
}

/// Reads the dynamic table at `offset`, `size` bytes long, and returns
/// where the relocation table is in the file and how many entries it has.
/// Refuses anything only a dynamic linker could do.
fn relocations(
    bytes: &[u8],
    image: &Image,
    offset: u64,
    size: u64,
) -> Result<(u64, u64), ElfError> {
    let end = offset.checked_add(size).ok_or(ElfError::Truncated)?;
    if end > bytes.len() as u64 {
        return Err(ElfError::Truncated);
    }
    let (mut rela, mut rela_size, mut rela_entry) = (None, 0, RELA_SIZE);
    for i in 0..(size / DYN_SIZE).min(MAX_DYNAMIC) {
        let at = (offset + i * DYN_SIZE) as usize;
        let tag = u64_at(bytes, at)? as i64;
        let value = u64_at(bytes, at + 8)?;
        match tag {
            DT_NULL => break,
            DT_NEEDED | DT_TEXTREL => return Err(ElfError::Dynamic),
            DT_PLTRELSZ | DT_RELSZ | DT_RELRSZ if value != 0 => return Err(ElfError::Dynamic),
            DT_FLAGS if value & DF_TEXTREL != 0 => return Err(ElfError::Dynamic),
            DT_RELA => rela = Some(value),
            DT_RELASZ => rela_size = value,
            DT_RELAENT => rela_entry = value,
            _ => {}
        }
    }
    let Some(address) = rela else {
        return if rela_size == 0 { Ok((0, 0)) } else { Err(ElfError::Malformed) };
    };
    if rela_entry != RELA_SIZE || rela_size % RELA_SIZE != 0 {
        return Err(ElfError::Malformed);
    }
    // The table is found by its address, in a segment that comes from the
    // file.
    let segment = image
        .segments()
        .find(|s| s.holds(address, rela_size) && address + rela_size <= s.vaddr + s.filesz)
        .ok_or(ElfError::Malformed)?;
    Ok((segment.offset + (address - segment.vaddr), rela_size / RELA_SIZE))
}
