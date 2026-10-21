// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A checked reader for ELF64 program images (the System V ABI's ELF
//! format, as the x86-64 psABI uses it).
//!
//! Radix starts programs from statically linked ELF64 executables. This
//! reads only what starting one takes: the header, and the loadable
//! segments with their addresses, sizes and access. It checks everything
//! before the kernel uses any of it, because an image is data from outside
//! the kernel: every offset and size stays inside the file, every segment
//! inside the user half and clear of the others, none is both writable and
//! executable, and the entry point is in an executable segment. Any input
//! gives a result or an error, never a panic (the property tests feed it
//! garbage).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

#[cfg(test)]
mod tests;

/// The most loadable segments an image may have.
pub const MAX_SEGMENTS: usize = 8;

/// The size of a page, which segments are mapped in.
pub const PAGE: u64 = 4096;

const HEADER_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;

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

/// A loadable segment.
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
}

/// A program image, checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Image {
    /// Where the program starts.
    pub entry: u64,
    segments: [Option<Segment>; MAX_SEGMENTS],
}

impl Image {
    /// Its loadable segments, in address order.
    pub fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().flatten()
    }

    /// How many pages its segments cover in all.
    pub fn pages(&self) -> u64 {
        self.segments().map(Segment::pages).sum()
    }
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
    /// A segment is both writable and executable, or not readable.
    BadAccess,
    /// The entry point is not in an executable segment.
    BadEntry,
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
            ElfError::BadAccess => "a segment is writable and executable, or not readable",
            ElfError::BadEntry => "the entry point is not in an executable segment",
        })
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, ElfError> {
    let b = bytes.get(at..at + 2).ok_or(ElfError::Truncated)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, ElfError> {
    let b = bytes.get(at..at + 4).ok_or(ElfError::Truncated)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, ElfError> {
    let b = bytes.get(at..at + 8).ok_or(ElfError::Truncated)?;
    let mut word = [0; 8];
    word.copy_from_slice(b);
    Ok(u64::from_le_bytes(word))
}

/// Reads and checks the image in `bytes`, whose segments must lie in
/// `range` (addresses from its start up to its end).
pub fn read(bytes: &[u8], range: core::ops::Range<u64>) -> Result<Image, ElfError> {
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
    if u16_at(bytes, 16)? != ET_EXEC || u16_at(bytes, 18)? != EM_X86_64 {
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
    let mut next_page = range.start;
    for i in 0..usize::from(phnum) {
        let at = table + i * PHDR_SIZE;
        if u32_at(bytes, at)? != PT_LOAD {
            continue;
        }
        let flags = u32_at(bytes, at + 4)?;
        let offset = u64_at(bytes, at + 8)?;
        let vaddr = u64_at(bytes, at + 16)?;
        let filesz = u64_at(bytes, at + 32)?;
        let memsz = u64_at(bytes, at + 40)?;
        if memsz == 0 {
            continue;
        }
        if filesz > memsz || offset % PAGE != vaddr % PAGE {
            return Err(ElfError::Malformed);
        }
        let file_end = offset.checked_add(filesz).ok_or(ElfError::Truncated)?;
        if file_end > bytes.len() as u64 {
            return Err(ElfError::Truncated);
        }
        let mem_end = vaddr.checked_add(memsz).ok_or(ElfError::OutOfRange)?;
        if vaddr < range.start || mem_end > range.end {
            return Err(ElfError::OutOfRange);
        }
        if vaddr & !(PAGE - 1) < next_page {
            return Err(ElfError::Overlap);
        }
        let access = match (flags & PF_R != 0, flags & PF_W != 0, flags & PF_X != 0) {
            (true, false, false) => Access::Read,
            (true, true, false) => Access::ReadWrite,
            (true, false, true) => Access::ReadExecute,
            _ => return Err(ElfError::BadAccess),
        };
        if count == MAX_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }
        // `range.end` is at most the top of the address space, so the
        // rounding cannot overflow past it by more than a page.
        next_page = mem_end.checked_next_multiple_of(PAGE).ok_or(ElfError::OutOfRange)?;
        segments[count] = Some(Segment { vaddr, memsz, offset, filesz, access });
        count += 1;
    }
    let image = Image { entry, segments };
    let runs_entry = image
        .segments()
        .any(|s| s.access == Access::ReadExecute && s.vaddr <= entry && entry < s.vaddr + s.memsz);
    if !runs_entry {
        return Err(ElfError::BadEntry);
    }
    Ok(image)
}
