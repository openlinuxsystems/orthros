// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! CRC-32C with the processor's instruction for it, for the filesystems,
//! which are compartments and cannot run it themselves: on x86-64, SSE4.2's
//! `crc32`, which takes eight bytes at a time in general registers, so the
//! kernel's vector registers stay unused.
//!
//! One `crc32` takes three cycles but the processor starts one each cycle,
//! so data is taken in three runs at once, and the runs' registers are put
//! together as the polynomial's arithmetic allows: CRC-32C is linear, so
//! the register after runs `a` then `b` is the register after `a`, moved
//! on over as many zero bytes as `b` has, xored with the register `b`
//! gives from zero. Moving a register on over a run of zeros is a fixed
//! linear map, worked out at compile time as four tables, one per byte of
//! the register.

use core::sync::atomic::{AtomicU8, Ordering};

/// CRC-32C's reflected polynomial (the Castagnoli polynomial).
const CASTAGNOLI: u32 = 0x82f6_3b78;

/// The bytes of each of the three runs taken at once: three of them fit a
/// 4 KiB sector, with 16 bytes over.
const RUN: usize = 1360;

/// The register after one byte, from each byte value with the rest of the
/// register zero.
const BYTES: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ CASTAGNOLI } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// `SKIP[k][v]`: the register after [`RUN`] zero bytes, from byte `k` of
/// the register holding `v` and the rest zero.
const SKIP: [[u32; 256]; 4] = {
    // Where each bit of the register goes.
    let mut bits = [0u32; 32];
    let mut bit = 0;
    while bit < 32 {
        let mut crc = 1u32 << bit;
        let mut n = 0;
        while n < RUN {
            crc = (crc >> 8) ^ BYTES[(crc & 0xff) as usize];
            n += 1;
        }
        bits[bit] = crc;
        bit += 1;
    }
    let mut tables = [[0u32; 256]; 4];
    let mut k = 0;
    while k < 4 {
        let mut value = 0;
        while value < 256 {
            let mut out = 0;
            let mut b = 0;
            while b < 8 {
                if (value >> b) & 1 != 0 {
                    out ^= bits[8 * k + b];
                }
                b += 1;
            }
            tables[k][value] = out;
            value += 1;
        }
        k += 1;
    }
    tables
};

/// The register `crc` moved on over [`RUN`] zero bytes.
fn skip(crc: u32) -> u32 {
    SKIP[0][(crc & 0xff) as usize]
        ^ SKIP[1][((crc >> 8) & 0xff) as usize]
        ^ SKIP[2][((crc >> 16) & 0xff) as usize]
        ^ SKIP[3][(crc >> 24) as usize]
}

/// Whether the processor has the instruction: 0 not asked yet, 1 no, 2 yes.
static HAS: AtomicU8 = AtomicU8::new(0);

/// Whether the processor has the instruction, asked once: CPUID can cost
/// a trip to the hypervisor.
fn has() -> bool {
    match HAS.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let has = cpu_has();
            HAS.store(if has { 2 } else { 1 }, Ordering::Relaxed);
            has
        }
    }
}

/// SSE4.2: CPUID leaf 1, ECX bit 20.
#[cfg(target_arch = "x86_64")]
fn cpu_has() -> bool {
    core::arch::x86_64::__cpuid(1).ecx & (1 << 20) != 0
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu_has() -> bool {
    false
}

/// The register `crc` after the eight bytes of `word`, little-endian.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn word(crc: u64, word: u64) -> u64 {
    let mut crc = crc;
    // SAFETY: `crc32` reads and writes the two registers alone, and runs
    // only once `has` found it (CPUID leaf 1, ECX bit 20).
    #[allow(unsafe_code)]
    unsafe {
        core::arch::asm!(
            "crc32 {crc}, {word}",
            crc = inout(reg) crc,
            word = in(reg) word,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    crc
}

/// The register `crc` after `byte`.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn byte(crc: u32, byte: u8) -> u32 {
    let mut crc = crc;
    // SAFETY: as in `word`.
    #[allow(unsafe_code)]
    unsafe {
        core::arch::asm!(
            "crc32 {crc:e}, {byte}",
            crc = inout(reg) crc,
            byte = in(reg_byte) byte,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    crc
}

#[cfg(not(target_arch = "x86_64"))]
fn word(_: u64, _: u64) -> u64 {
    unreachable!("no CRC-32C instruction")
}

#[cfg(not(target_arch = "x86_64"))]
fn byte(_: u32, _: u8) -> u32 {
    unreachable!("no CRC-32C instruction")
}

/// The CRC-32C register `crc` after `data`, with neither the first nor
/// the last inversion, as the processor's instruction gives it, or `None`
/// when the processor has none.
pub fn crc32c(crc: u32, data: &[u8]) -> Option<u32> {
    if !has() {
        return None;
    }
    let mut crc = u64::from(crc);
    let mut rest = data;
    while let Some((runs, after)) = rest.split_first_chunk::<{ 3 * RUN }>() {
        let (a, bc) = runs.split_at(RUN);
        let (b, c) = bc.split_at(RUN);
        let (mut x, mut y, mut z) = (crc, 0, 0);
        for ((a, b), c) in
            a.as_chunks::<8>().0.iter().zip(b.as_chunks::<8>().0).zip(c.as_chunks::<8>().0)
        {
            x = word(x, u64::from_le_bytes(*a));
            y = word(y, u64::from_le_bytes(*b));
            z = word(z, u64::from_le_bytes(*c));
        }
        // The registers fill 32 bits.
        crc = u64::from(skip(skip(x as u32) ^ y as u32) ^ z as u32);
        rest = after;
    }
    let (words, bytes) = rest.as_chunks::<8>();
    for w in words {
        crc = word(crc, u64::from_le_bytes(*w));
    }
    let mut crc = crc as u32;
    for &b in bytes {
        crc = byte(crc, b);
    }
    Some(crc)
}
