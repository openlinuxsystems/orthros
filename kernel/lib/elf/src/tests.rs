// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use proptest::prelude::*;

use super::*;

const RANGE: core::ops::Range<u64> = 0x10000..0x7ff0_0000_0000;

/// A program header for [`build`]: type, flags, address, file offset,
/// file size and memory size.
type Phdr = (u32, u32, u64, u64, u64, u64);

/// A loadable segment for [`build`].
const fn load(flags: u32, vaddr: u64, offset: u64, filesz: u64, memsz: u64) -> Phdr {
    (PT_LOAD, flags, vaddr, offset, filesz, memsz)
}

/// An ELF64 x86-64 file of type `kind` with these program headers, the
/// table right after the header, padded to `len` bytes.
fn build(kind: u16, entry: u64, phdrs: &[Phdr], len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len.max(HEADER_SIZE + phdrs.len() * PHDR_SIZE)];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[16..18].copy_from_slice(&kind.to_le_bytes());
    bytes[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[24..32].copy_from_slice(&entry.to_le_bytes());
    bytes[32..40].copy_from_slice(&(HEADER_SIZE as u64).to_le_bytes());
    bytes[52..54].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    bytes[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    bytes[56..58].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());
    for (i, &(kind, flags, vaddr, offset, filesz, memsz)) in phdrs.iter().enumerate() {
        let at = HEADER_SIZE + i * PHDR_SIZE;
        bytes[at..at + 4].copy_from_slice(&kind.to_le_bytes());
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

fn put(bytes: &mut [u8], at: u64, value: u64) {
    bytes[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
}

/// Text, read-only data and data with a BSS, as the user-test linker
/// script lays them out.
fn typical() -> Vec<u8> {
    build(
        ET_EXEC,
        0x40_1000,
        &[
            load(PF_R, 0x40_0000, 0, 0x100, 0x100),
            load(PF_R | PF_X, 0x40_1000, 0x1000, 0x800, 0x800),
            load(PF_R | PF_W, 0x40_2000, 0x2000, 0x10, 0x3000),
        ],
        0x3000,
    )
}

/// A static PIE as lld lays one out: read-only data from address 0, text,
/// a writable segment that is read-only once relocated (holding the
/// dynamic table and two pointers to relocate), and data with a BSS.
/// `relocations` are (type, offset, addend) and go in the read-only
/// segment's file bytes at 0x200.
fn pie(relocations: &[(u32, u64, u64)], extra_dynamic: &[(i64, u64)]) -> Vec<u8> {
    let rela_size = relocations.len() as u64 * RELA_SIZE;
    let mut bytes = build(
        ET_DYN,
        0x1000,
        &[
            load(PF_R, 0, 0, 0x800, 0x800),
            load(PF_R | PF_X, 0x1000, 0x1000, 0x100, 0x100),
            load(PF_R | PF_W, 0x2000, 0x2000, 0x1000, 0x1000),
            load(PF_R | PF_W, 0x3000, 0x3000, 0x20, 0x2000),
            (PT_DYNAMIC, PF_R | PF_W, 0x2000, 0x2000, 0x100, 0x100),
            (PT_GNU_RELRO, PF_R, 0x2000, 0x2000, 0x1000, 0x1000),
            (PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 0),
        ],
        0x3020,
    );
    let mut dynamic = vec![(DT_RELA, 0x200), (DT_RELASZ, rela_size), (DT_RELAENT, RELA_SIZE)];
    dynamic.extend_from_slice(extra_dynamic);
    for (i, &(tag, value)) in dynamic.iter().enumerate() {
        put(&mut bytes, 0x2000 + i as u64 * DYN_SIZE, tag as u64);
        put(&mut bytes, 0x2008 + i as u64 * DYN_SIZE, value);
    }
    for (i, &(kind, offset, addend)) in relocations.iter().enumerate() {
        let at = 0x200 + i as u64 * RELA_SIZE;
        put(&mut bytes, at, offset);
        put(&mut bytes, at + 8, u64::from(kind));
        put(&mut bytes, at + 16, addend);
    }
    bytes
}

/// Two pointers to relocate: one in the read-only-after-relocation page,
/// one in the data.
fn good_pie() -> Vec<u8> {
    pie(&[(R_X86_64_RELATIVE, 0x2800, 0x1000), (R_X86_64_RELATIVE, 0x3010, 0x3018)], &[])
}

/// Loads `image` from `bytes` at `bias` into a buffer of its pages.
fn loaded(bytes: &[u8], image: &Image, bias: u64) -> Result<Vec<u8>, ElfError> {
    let placed = image.place(bias, RANGE)?;
    let mut memory = vec![0u8; (image.pages() * PAGE) as usize];
    placed.load(bytes, &mut |at, data| {
        memory[at as usize..at as usize + data.len()].copy_from_slice(data);
    })?;
    Ok(memory)
}

fn word(memory: &[u8], at: u64) -> u64 {
    u64::from_le_bytes(memory[at as usize..at as usize + 8].try_into().unwrap())
}

#[test]
fn reads_a_typical_image() {
    let image = read(&typical()).unwrap();
    assert_eq!(image.entry, 0x40_1000);
    assert_eq!(image.kind, Kind::Fixed);
    let segments: Vec<_> = image.segments().copied().collect();
    assert_eq!(segments.len(), 3);
    assert_eq!(segments[1].access, Access::ReadExecute);
    assert_eq!(segments[2].access, Access::ReadWrite);
    assert_eq!(segments[2].pages(), 3);
    assert_eq!(image.pages(), 5);
    assert_eq!(image.span(), 0x40_0000..0x40_5000);
    // A fixed image stays where it was linked.
    assert_eq!(image.random_bias(RANGE, 12345), Ok(0));
    assert_eq!(image.place(0x1000, RANGE).err(), Some(ElfError::OutOfRange));
    let placed = image.place(0, RANGE).unwrap();
    assert_eq!(placed.entry(), 0x40_1000);
    let mappings: Vec<_> = placed.mappings().into_iter().flatten().collect();
    assert_eq!(
        mappings,
        [
            Mapping { address: 0x40_0000, page: 0, pages: 1, access: Access::Read },
            Mapping { address: 0x40_1000, page: 1, pages: 1, access: Access::ReadExecute },
            Mapping { address: 0x40_2000, page: 2, pages: 3, access: Access::ReadWrite },
        ]
    );
}

#[test]
fn loads_a_typical_image() {
    let bytes = typical();
    let image = read(&bytes).unwrap();
    let memory = loaded(&bytes, &image, 0).unwrap();
    // Each segment's file bytes, at its offset in its first page, one
    // segment after another; the BSS stays zero.
    assert_eq!(memory[..0x100], bytes[..0x100]);
    assert_eq!(memory[0x1000..0x1800], bytes[0x1000..0x1800]);
    assert_eq!(memory[0x2000..0x2010], bytes[0x2000..0x2010]);
    assert!(memory[0x2010..].iter().all(|&b| b == 0));
}

#[test]
fn places_loads_and_relocates_a_pie() {
    let bytes = good_pie();
    let image = read(&bytes).unwrap();
    assert_eq!(image.kind, Kind::Relocatable);
    assert_eq!(image.relocation_count(), 2);
    assert_eq!(image.span(), 0..0x5000);
    // Address 0 is below the range: a PIE has to move.
    assert_eq!(image.place(0, RANGE).err(), Some(ElfError::OutOfRange));
    let bias = 0x5555_0000_0000;
    let placed = image.place(bias, RANGE).unwrap();
    assert_eq!(placed.entry(), bias + 0x1000);
    let memory = loaded(&bytes, &image, bias).unwrap();
    assert_eq!(word(&memory, 0x2800), bias + 0x1000);
    assert_eq!(word(&memory, 0x3010), bias + 0x3018);
    // The page read-only once relocated is mapped read-only.
    let mappings: Vec<_> = placed.mappings().into_iter().flatten().collect();
    assert_eq!(
        mappings,
        [
            Mapping { address: bias, page: 0, pages: 1, access: Access::Read },
            Mapping { address: bias + 0x1000, page: 1, pages: 1, access: Access::ReadExecute },
            Mapping { address: bias + 0x2000, page: 2, pages: 1, access: Access::Read },
            Mapping { address: bias + 0x3000, page: 3, pages: 2, access: Access::ReadWrite },
        ]
    );
}

#[test]
fn random_biases_fit_and_spread() {
    let bytes = good_pie();
    let image = read(&bytes).unwrap();
    let window = 0x1000_0000_0000..0x1000_0010_0000;
    let mut seen = std::collections::BTreeSet::new();
    for random in 0..1000u64 {
        let bias =
            image.random_bias(window.clone(), random.wrapping_mul(0x9e37_79b9_7f4a_7c15)).unwrap();
        assert_eq!(bias % PAGE, 0);
        assert!(image.place(bias, window.clone()).is_ok(), "bias {bias:#x}");
        seen.insert(bias);
    }
    // 256 pages of window less 5 of image: 252 places, most of them used.
    assert!(seen.len() > 200, "{} places", seen.len());
    assert_eq!(image.random_bias(0x1000..0x3000, 0), Err(ElfError::OutOfRange));
}

#[test]
fn a_pie_linked_above_its_place_moves_down() {
    // An ET_DYN linked at 0x400000, placed below that: the bias wraps.
    let mut bytes = typical();
    bytes[16..18].copy_from_slice(&ET_DYN.to_le_bytes());
    let image = read(&bytes).unwrap();
    assert_eq!(image.kind, Kind::Relocatable);
    let window = 0x10000..0x20_0000;
    for random in 0..64u64 {
        let bias = image.random_bias(window.clone(), random).unwrap();
        let placed = image.place(bias, window.clone()).unwrap();
        let start = 0x40_0000u64.wrapping_add(bias);
        assert!(window.contains(&start), "bias {bias:#x}");
        assert_eq!(placed.entry(), start + 0x1000);
    }
    // A move that leaves the image wrapped around the top of the
    // addresses, or below the range, is refused.
    assert_eq!(
        image.place(0u64.wrapping_sub(0x40_1000), 0..u64::MAX).err(),
        Some(ElfError::OutOfRange)
    );
    assert_eq!(image.place(0u64.wrapping_sub(0x40_0000), RANGE).err(), Some(ElfError::OutOfRange));
}

#[test]
fn refuses_what_it_must() {
    let rx = PF_R | PF_X;
    let cases: &[(u64, &[Phdr], ElfError)] = &[
        (0x40_0000, &[load(PF_R | PF_W | PF_X, 0x40_0000, 0, 0x10, 0x10)], ElfError::BadAccess),
        (0x40_0000, &[load(PF_W, 0x40_0000, 0, 0x10, 0x10)], ElfError::BadAccess),
        (0x40_0000, &[load(rx, 0x40_0000, 0, 0x20, 0x10)], ElfError::Malformed),
        (0x40_0000, &[load(rx, 0x40_0000, 0x10, 0x10, 0x10)], ElfError::Malformed),
        (0x40_0000, &[load(rx, 0x40_0000, 0, 0x10_0000, 0x10_0000)], ElfError::Truncated),
        (
            0x40_0000,
            &[load(rx, 0x40_0000, 0, 0x10, 0x10), load(PF_R, 0x40_0800, 0x800, 0x10, 0x10)],
            ElfError::Overlap,
        ),
        (
            0x40_2000,
            &[load(rx, 0x40_2000, 0, 0x10, 0x10), load(PF_R, 0x40_0000, 0, 0x10, 0x10)],
            ElfError::Overlap,
        ),
        (0x40_0000, &[load(PF_R, 0x40_0000, 0, 0x10, 0x10)], ElfError::BadEntry),
        (0x50_0000, &[load(rx, 0x40_0000, 0, 0x10, 0x10)], ElfError::BadEntry),
        (
            0x40_0000,
            &[load(rx, 0x40_0000, 0, 0x10, 0x10), (PT_INTERP, PF_R, 0, 0, 0, 0)],
            ElfError::Dynamic,
        ),
        (
            0x40_0000,
            &[load(rx, 0x40_0000, 0, 0x10, 0x10), (PT_TLS, PF_R, 0, 0, 0, 8)],
            ElfError::ThreadLocal,
        ),
        (
            0x40_0000,
            &[load(rx, 0x40_0000, 0, 0x10, 0x10), (PT_GNU_STACK, PF_R | PF_W | PF_X, 0, 0, 0, 0)],
            ElfError::BadAccess,
        ),
        (
            0x40_0000,
            &[load(rx, 0x40_0000, 0, 0x10, 0x10), (PT_GNU_RELRO, PF_R, 0x40_0000, 0, 0, 8)],
            ElfError::Malformed,
        ),
    ];
    for (i, &(entry, phdrs, want)) in cases.iter().enumerate() {
        assert_eq!(read(&build(ET_EXEC, entry, phdrs, 0x2000)), Err(want), "case {i}");
    }
    // Placement checks the range.
    let low = build(ET_EXEC, 0x1000, &[load(rx, 0x1000, 0x1000, 0x10, 0x10)], 0x2000);
    assert_eq!(read(&low).unwrap().place(0, RANGE).err(), Some(ElfError::OutOfRange));
    let high =
        build(ET_EXEC, 0x7fef_ffff_f000, &[load(rx, 0x7fef_ffff_f000, 0, 0x10, 0x2000)], 0x2000);
    assert_eq!(read(&high).map(|i| i.place(0, RANGE).err()), Ok(Some(ElfError::OutOfRange)));

    let mut bytes = typical();
    bytes[0] = 0;
    assert_eq!(read(&bytes), Err(ElfError::NotElf));
    let mut bytes = typical();
    bytes[4] = 1;
    assert_eq!(read(&bytes), Err(ElfError::Unsupported));
    let mut bytes = typical();
    bytes[16] = 4;
    assert_eq!(read(&bytes), Err(ElfError::Unsupported));
    assert_eq!(read(&typical()[..40]), Err(ElfError::Truncated));
    let nine: Vec<Phdr> =
        (0..9).map(|i| load(PF_R | PF_X, 0x40_0000 + i * 0x1000, i * 0x1000, 0x10, 0x10)).collect();
    assert_eq!(read(&build(ET_EXEC, 0x40_0000, &nine, 0x9000)), Err(ElfError::TooManySegments));
}

#[test]
fn refuses_what_only_a_dynamic_linker_does() {
    for tag in [DT_NEEDED, DT_TEXTREL] {
        assert_eq!(read(&pie(&[], &[(tag, 1)])), Err(ElfError::Dynamic), "tag {tag}");
    }
    for tag in [DT_PLTRELSZ, DT_RELSZ, DT_RELRSZ] {
        assert_eq!(read(&pie(&[], &[(tag, 24)])), Err(ElfError::Dynamic), "tag {tag}");
        assert!(read(&pie(&[], &[(tag, 0)])).is_ok(), "empty tag {tag}");
    }
    assert_eq!(read(&pie(&[], &[(DT_FLAGS, DF_TEXTREL)])), Err(ElfError::Dynamic));
    // Relocations through symbols, of other kinds, of code, and past the
    // image all fail when loaded.
    let bias = 0x5555_0000_0000;
    let refused: &[(u32, u64)] = &[
        (1, 0x2800),
        (R_X86_64_RELATIVE, 0x1000),
        (R_X86_64_RELATIVE, 0x4ffc),
        (R_X86_64_RELATIVE, 0x9000),
    ];
    for &(kind, offset) in refused {
        let bytes = pie(&[(kind, offset, 0)], &[]);
        let image = read(&bytes).unwrap();
        assert_eq!(
            loaded(&bytes, &image, bias).err(),
            Some(ElfError::Relocation),
            "{kind} {offset:#x}"
        );
    }
    // A symbol index makes a relative relocation one through a symbol.
    let mut bytes = pie(&[(R_X86_64_RELATIVE, 0x2800, 0)], &[]);
    put(&mut bytes, 0x208, (1 << 32) | u64::from(R_X86_64_RELATIVE));
    let image = read(&bytes).unwrap();
    assert_eq!(loaded(&bytes, &image, bias).err(), Some(ElfError::Relocation));
    // A table that is not in the file, or of the wrong entry size.
    assert_eq!(read(&pie(&[], &[(DT_RELA, 0x3018), (DT_RELASZ, 48)])), Err(ElfError::Malformed));
    assert_eq!(read(&pie(&[], &[(DT_RELAENT, 16)])), Err(ElfError::Malformed));
}

/// A dynamically linked program as `exec` sees one: its program headers
/// in its first page, an interpreter's path, thread-local storage and a
/// dynamic table, with only its first 0x400 bytes read.
fn dynamic_program(interp: (u64, u64)) -> Vec<u8> {
    build(
        ET_DYN,
        0x1000,
        &[
            (PT_PHDR, PF_R, HEADER_SIZE as u64, HEADER_SIZE as u64, 6 * 56, 6 * 56),
            (PT_INTERP, PF_R, 0x300, interp.0, interp.1, interp.1),
            load(PF_R, 0, 0, 0x800, 0x800),
            load(PF_R | PF_X, 0x1000, 0x1000, 0x100, 0x100),
            (PT_TLS, PF_R, 0x2000, 0x2000, 0x10, 0x20),
            (PT_DYNAMIC, PF_R | PF_W, 0x2000, 0x2000, 0x100, 0x100),
        ],
        0x400,
    )
}

#[test]
fn read_program_takes_what_exec_takes() {
    let head = dynamic_program((0x300, 20));
    // Only the head is in memory; the file is longer.
    let program = read_program(&head, 0x2100).unwrap();
    assert_eq!(program.interpreter, Some((0x300, 20)));
    assert_eq!(program.headers, Some(HEADER_SIZE as u64));
    assert_eq!(program.header_count, 6);
    assert_eq!(program.dynamic, Some((0x2000, 0x100)));
    assert_eq!(program.image.entry, 0x1000);
    assert_eq!(program.image.segments().count(), 2);
    // `read` takes none of that: an interpreter is a dynamic linker's.
    let mut whole = head.clone();
    whole.resize(0x2100, 0);
    assert_eq!(read(&whole), Err(ElfError::Dynamic));
}

#[test]
fn read_program_checks_against_the_file() {
    // A segment, a dynamic table or a path past the end of the file.
    assert_eq!(read_program(&dynamic_program((0x300, 20)), 0x1080), Err(ElfError::Truncated));
    assert_eq!(read_program(&dynamic_program((0x2100, 20)), 0x2100), Err(ElfError::Truncated));
    // Paths too short, too long, or given twice.
    assert_eq!(read_program(&dynamic_program((0x300, 1)), 0x2100), Err(ElfError::Malformed));
    let long = INTERPRETER_MAX + 1;
    assert_eq!(read_program(&dynamic_program((0x300, long)), 0x3000), Err(ElfError::Malformed));
    let mut twice = dynamic_program((0x300, 20));
    let first = HEADER_SIZE + PHDR_SIZE;
    let second = HEADER_SIZE + 4 * PHDR_SIZE;
    let interp = twice[first..first + PHDR_SIZE].to_vec();
    twice[second..second + PHDR_SIZE].copy_from_slice(&interp);
    assert_eq!(read_program(&twice, 0x2100), Err(ElfError::Malformed));
    // A head too short for its own program headers.
    assert_eq!(
        read_program(&dynamic_program((0x300, 20))[..0x100], 0x2100),
        Err(ElfError::Truncated)
    );
}

#[test]
fn read_program_finds_headers_a_segment_loads() {
    // No PT_PHDR: the headers are in memory because the first segment
    // loads the start of the file.
    let program = read_program(&typical(), 0x3000).unwrap();
    assert_eq!(program.headers, Some(0x40_0000 + HEADER_SIZE as u64));
    assert_eq!(program.interpreter, None);
    // A static PIE has no interpreter but a dynamic table to relocate.
    let bytes = good_pie();
    let program = read_program(&bytes, bytes.len() as u64).unwrap();
    let (offset, size) = program.dynamic.unwrap();
    let table = &bytes[offset as usize..(offset + size) as usize];
    assert_eq!(program.relocations(table), Ok(read(&bytes).unwrap().relocations));
}

proptest! {
    /// Garbage gives an error, never a panic.
    #[test]
    fn garbage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(image) = read(&bytes) {
            let _ = loaded(&bytes, &image, 0x1000_0000_0000);
        }
        if let Ok(program) = read_program(&bytes, bytes.len() as u64 + 0x1000)
            && let Some((offset, size)) = program.interpreter
        {
            prop_assert!(offset + size <= bytes.len() as u64 + 0x1000);
        }
    }

    /// Whatever bytes of a good image are changed, what is accepted keeps
    /// every promise: inside the file, page-disjoint, in order, never
    /// writable and executable, entered in executable code, and loaded
    /// only into its own pages.
    #[test]
    fn accepted_images_keep_their_promises(
        pie_image in any::<bool>(),
        changes in proptest::collection::vec((0usize..0x3020, any::<u8>()), 0..8),
        random in any::<u64>(),
    ) {
        let mut bytes = if pie_image { good_pie() } else { typical() };
        let header = HEADER_SIZE + 7 * PHDR_SIZE;
        for (at, value) in changes {
            // Mostly the headers, sometimes the rest.
            let at = if at % 4 == 0 { at } else { at % header };
            if at < bytes.len() {
                bytes[at] = value;
            }
        }
        if let Ok(image) = read(&bytes) {
            let mut last_end = 0;
            for s in image.segments() {
                prop_assert!(s.offset + s.filesz <= bytes.len() as u64);
                prop_assert!(s.filesz <= s.memsz);
                prop_assert!(s.first_page() >= last_end);
                last_end = s.first_page() + s.pages() * PAGE;
            }
            prop_assert!(image.segments().any(|s| s.access == Access::ReadExecute
                && s.vaddr <= image.entry && image.entry < s.vaddr + s.memsz));
            let window = 0x1000_0000_0000..0x2000_0000_0000;
            if let Ok(bias) = image.random_bias(window.clone(), random)
                && let Ok(placed) = image.place(bias, window.clone())
            {
                let limit = image.pages() * PAGE;
                let mut inside = true;
                let _ = placed.load(&bytes, &mut |at, data| {
                    inside &= at + data.len() as u64 <= limit;
                });
                prop_assert!(inside);
                for m in placed.mappings().into_iter().flatten() {
                    prop_assert!(window.start <= m.address);
                    prop_assert!(m.address + m.pages * PAGE <= window.end);
                    prop_assert!(m.page + m.pages <= image.pages());
                }
            }
        }
    }
}
