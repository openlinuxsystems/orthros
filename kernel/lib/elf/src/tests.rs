// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use proptest::prelude::*;

use super::*;

const RANGE: core::ops::Range<u64> = 0x10000..0x7ff0_0000_0000;

/// A segment for [`build`]: flags, address, file offset, file size and
/// memory size.
type Phdr = (u32, u64, u64, u64, u64);

/// An ELF64 x86-64 executable with these program headers, the table right
/// after the header, padded to `len` bytes.
fn build(entry: u64, phdrs: &[Phdr], len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len.max(HEADER_SIZE + phdrs.len() * PHDR_SIZE)];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
    bytes[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[24..32].copy_from_slice(&entry.to_le_bytes());
    bytes[32..40].copy_from_slice(&(HEADER_SIZE as u64).to_le_bytes());
    bytes[52..54].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    bytes[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    bytes[56..58].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());
    for (i, &(flags, vaddr, offset, filesz, memsz)) in phdrs.iter().enumerate() {
        let at = HEADER_SIZE + i * PHDR_SIZE;
        bytes[at..at + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        bytes[at + 4..at + 8].copy_from_slice(&flags.to_le_bytes());
        bytes[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
        bytes[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
        bytes[at + 24..at + 32].copy_from_slice(&vaddr.to_le_bytes());
        bytes[at + 32..at + 40].copy_from_slice(&filesz.to_le_bytes());
        bytes[at + 40..at + 48].copy_from_slice(&memsz.to_le_bytes());
        bytes[at + 48..at + 56].copy_from_slice(&PAGE.to_le_bytes());
    }
    bytes
}

/// Text, read-only data and data with a BSS, as the user-test linker
/// script lays them out.
fn typical() -> Vec<u8> {
    build(
        0x40_1000,
        &[
            (PF_R, 0x40_0000, 0, 0x100, 0x100),
            (PF_R | PF_X, 0x40_1000, 0x1000, 0x800, 0x800),
            (PF_R | PF_W, 0x40_2000, 0x2000, 0x10, 0x3000),
        ],
        0x3000,
    )
}

#[test]
fn reads_a_typical_image() {
    let image = read(&typical(), RANGE).unwrap();
    assert_eq!(image.entry, 0x40_1000);
    let segments: Vec<_> = image.segments().copied().collect();
    assert_eq!(segments.len(), 3);
    assert_eq!(segments[1].access, Access::ReadExecute);
    assert_eq!(segments[2].access, Access::ReadWrite);
    assert_eq!(segments[2].pages(), 3);
    assert_eq!(image.pages(), 5);
}

#[test]
fn refuses_what_it_must() {
    let rx = PF_R | PF_X;
    let cases: &[(u64, &[Phdr], ElfError)] = &[
        (0x40_0000, &[(PF_R | PF_W | PF_X, 0x40_0000, 0, 0x10, 0x10)], ElfError::BadAccess),
        (0x40_0000, &[(PF_W, 0x40_0000, 0, 0x10, 0x10)], ElfError::BadAccess),
        (0x40_0000, &[(rx, 0x40_0000, 0, 0x20, 0x10)], ElfError::Malformed),
        (0x40_0000, &[(rx, 0x40_0000, 0x10, 0x10, 0x10)], ElfError::Malformed),
        (0x40_0000, &[(rx, 0x40_0000, 0, 0x10_0000, 0x10_0000)], ElfError::Truncated),
        (0x1000, &[(rx, 0x1000, 0x1000, 0x10, 0x10)], ElfError::OutOfRange),
        (0x40_0000, &[(rx, 0x7fef_ffff_f000, 0, 0x10, 0x2000)], ElfError::OutOfRange),
        (
            0x40_0000,
            &[(rx, 0x40_0000, 0, 0x10, 0x10), (PF_R, 0x40_0800, 0x800, 0x10, 0x10)],
            ElfError::Overlap,
        ),
        (
            0x40_2000,
            &[(rx, 0x40_2000, 0, 0x10, 0x10), (PF_R, 0x40_0000, 0, 0x10, 0x10)],
            ElfError::Overlap,
        ),
        (0x40_0000, &[(PF_R, 0x40_0000, 0, 0x10, 0x10)], ElfError::BadEntry),
        (0x50_0000, &[(rx, 0x40_0000, 0, 0x10, 0x10)], ElfError::BadEntry),
    ];
    for (i, &(entry, phdrs, want)) in cases.iter().enumerate() {
        assert_eq!(read(&build(entry, phdrs, 0x2000), RANGE), Err(want), "case {i}");
    }
    let mut bytes = typical();
    bytes[0] = 0;
    assert_eq!(read(&bytes, RANGE), Err(ElfError::NotElf));
    let mut bytes = typical();
    bytes[4] = 1;
    assert_eq!(read(&bytes, RANGE), Err(ElfError::Unsupported));
    assert_eq!(read(&typical()[..40], RANGE), Err(ElfError::Truncated));
    let nine: Vec<Phdr> =
        (0..9).map(|i| (PF_R | PF_X, 0x40_0000 + i * 0x1000, i * 0x1000, 0x10, 0x10)).collect();
    assert_eq!(read(&build(0x40_0000, &nine, 0x9000), RANGE), Err(ElfError::TooManySegments));
}

proptest! {
    /// Garbage gives an error, never a panic.
    #[test]
    fn garbage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = read(&bytes, RANGE);
    }

    /// Whatever bytes of a good image are changed, what is accepted keeps
    /// every promise: inside the file, inside the range, page-disjoint, in
    /// order, never writable and executable, and entered in executable
    /// code.
    #[test]
    fn accepted_images_keep_their_promises(
        changes in proptest::collection::vec((0usize..HEADER_SIZE + 3 * PHDR_SIZE, any::<u8>()), 0..8)
    ) {
        let mut bytes = typical();
        for (at, value) in changes {
            bytes[at] = value;
        }
        if let Ok(image) = read(&bytes, RANGE) {
            let mut last_end = 0;
            for s in image.segments() {
                prop_assert!(s.offset + s.filesz <= bytes.len() as u64);
                prop_assert!(s.filesz <= s.memsz);
                prop_assert!(RANGE.start <= s.vaddr && s.vaddr + s.memsz <= RANGE.end);
                prop_assert!(s.first_page() >= last_end);
                last_end = s.first_page() + s.pages() * PAGE;
            }
            prop_assert!(image.segments().any(|s| s.access == Access::ReadExecute
                && s.vaddr <= image.entry && image.entry < s.vaddr + s.memsz));
        }
    }
}
