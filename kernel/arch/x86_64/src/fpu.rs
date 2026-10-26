// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Extended processor state: the x87 FPU and the SSE, AVX and AVX-512
//! registers.
//!
//! The kernel itself never uses these registers (it is built without SSE,
//! see `kernel/targets`), but threads do once they run user code, and the
//! FPU self-test does now. Each thread has a save area in its context block
//! (`thread.rs`), and every switch saves the outgoing thread's registers
//! and restores the incoming one's. Switching is eager: lazy switching,
//! which traps a thread's first use instead, leaked one thread's registers
//! to another through speculative execution (CVE-2018-3665), and costs a
//! trap per switch on any thread that computes.
//!
//! With XSAVE the kernel enables in XCR0 each of x87, SSE, AVX, AVX-512,
//! protection keys (PKRU) and AMX that the CPU has, and saves with
//! XSAVEOPT where it can, which skips registers unchanged since the last
//! restore. Nothing uses protection keys yet (CR4.PKE stays off), and AMX
//! is held off: extended feature disable (XFD) makes each AMX instruction
//! fault, as in Linux until a program asks for AMX, so its 8 KiB of tile
//! data is never saved and the save area stays small. Saves and restores
//! leave it out of the components they ask for, as Linux's do: a CPU may
//! check that it could reach every component asked for, even one it does
//! not touch (an Intel Xeon 8573C faulted at an XRSTOR on the last byte of
//! the tile data's place, past the area, though the header said the tile
//! data was in its initial state). Both are on
//! because a hypervisor runs with its own XCR0: when a guest's differs,
//! KVM changes XCR0 with XSETBV at every entry to the guest and every exit
//! from it, and nested under another hypervisor on Intel CPUs each XSETBV
//! leaves for that hypervisor too (ADR 0120). Linux guests enable what
//! their host does, so their XCR0 matches it, and so does Radix's. MPX
//! stays off, as Linux leaves it. Without XSAVE, FXSAVE saves x87 and SSE.

use core::arch::asm;
use core::fmt::Write;
use core::ops::Range;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use radix_klog::TextBuf;
use radix_sync::Once;

use crate::cpu::{self, msr};

/// Room for a save area in a context block.
pub const AREA_MAX: usize = 4096 - 64;

/// How state is saved.
const NONE: u8 = 0;
const FXSAVE: u8 = 1;
const XSAVE: u8 = 2;
const XSAVEOPT: u8 = 3;

static MODE: AtomicU8 = AtomicU8::new(NONE);
static XCR0: AtomicU64 = AtomicU64::new(0);
static SIZE: AtomicU32 = AtomicU32::new(512);
/// The components XCR0 enables that XFD holds off: tile data, with AMX.
static HELD: AtomicU64 = AtomicU64::new(0);
/// The components saves and restores ask for (EDX:EAX): those XCR0
/// enables, but for those held off.
static ASKED: AtomicU64 = AtomicU64::new(0);
/// Where each component beyond SSE that XCR0 enables (2 to 18) starts and
/// ends in an image, from CPUID leaf 0xD; 0 for one not enabled or held
/// off.
static STARTS: [AtomicU32; 19] = [const { AtomicU32::new(0) }; 19];
static ENDS: [AtomicU32; 19] = [const { AtomicU32::new(0) }; 19];
static DESCRIPTION: Once<TextBuf<128>> = Once::new();

/// The legacy region and the XSAVE header: the head of an image, which a
/// signal frame always holds.
const HEAD: usize = 576;

/// The initial state as an image as large as any save area, as
/// [`init_area`] writes it: what [`load_initial`] restores, as Linux
/// restores its `init_fpstate`. No thread saves into it, so an XSAVEOPT
/// into a thread's area after a restore from it writes the whole state.
#[repr(C, align(64))]
struct Initial([u8; AREA_MAX]);

static INITIAL: Initial = Initial({
    let mut image = [0; AREA_MAX];
    // FCW 0x37f and MXCSR 0x1f80; the XSAVE header at 512 is zero.
    image[0] = 0x7f;
    image[1] = 0x03;
    image[24] = 0x80;
    image[25] = 0x1f;
    image
});

/// XCR0 bits: x87, SSE, AVX, the three AVX-512 components, protection
/// keys, and AMX's tile configuration and tile data.
const X87: u64 = 1;
const SSE: u64 = 1 << 1;
const AVX: u64 = 1 << 2;
const AVX512: u64 = 0b111 << 5;
const PKRU: u64 = 1 << 9;
const TILE_DATA: u64 = 1 << 18;
const AMX: u64 = 1 << 17 | TILE_DATA;

/// Chooses how to save extended state, on the boot CPU, then sets this
/// CPU up.
pub fn init_boot_cpu() {
    let mode = if cpu::feature::xsaveopt() {
        XSAVEOPT
    } else if cpu::feature::xsave() {
        XSAVE
    } else if cpu::feature::fxsr() {
        FXSAVE
    } else {
        NONE
    };
    if mode >= XSAVE {
        let leaf = cpu::cpuid(0xd, 0);
        let supported = u64::from(leaf.eax) | (u64::from(leaf.edx) << 32);
        let mut wanted = supported & (X87 | SSE | AVX | AVX512 | PKRU | AMX);
        if wanted & AVX512 != AVX512 || wanted & AVX == 0 {
            wanted &= !AVX512;
        }
        if wanted & SSE == 0 {
            wanted &= !AVX;
        }
        // AMX only with XFD (CPUID 0xD.1 EAX bit 4) able to hold its tile
        // data off (that component's ECX bit 2), which keeps the data out
        // of every image: 8 KiB would not fit the save area.
        let xfd = cpu::cpuid(0xd, 1).eax & (1 << 4) != 0 && cpu::cpuid(0xd, 18).ecx & (1 << 2) != 0;
        if wanted & AMX != AMX || !xfd {
            wanted &= !AMX;
        }
        if wanted & AMX != 0 {
            HELD.store(TILE_DATA, Ordering::Relaxed);
        }
        XCR0.store(wanted | X87, Ordering::Relaxed);
        ASKED.store((wanted | X87) & !HELD.load(Ordering::Relaxed), Ordering::Relaxed);
    }
    MODE.store(mode, Ordering::Relaxed);
    init_cpu();
    if mode >= XSAVE {
        // An image ends where the last component it can hold does; CPUID
        // leaf 0 would count the tile data, which XFD keeps out.
        let xcr0 = XCR0.load(Ordering::Relaxed) & !HELD.load(Ordering::Relaxed);
        let mut size = HEAD as u32;
        for (component, (start, end)) in STARTS.iter().zip(&ENDS).enumerate().skip(2) {
            if xcr0 & (1 << component) != 0 {
                // EBX is the component's offset, EAX its size.
                let leaf = cpu::cpuid(0xd, component as u32);
                start.store(leaf.ebx, Ordering::Relaxed);
                end.store(leaf.ebx + leaf.eax, Ordering::Relaxed);
                size = size.max(leaf.ebx + leaf.eax);
            }
        }
        if size as usize > AREA_MAX {
            // Cannot happen with the components above; fall back to the
            // legacy area rather than overrun the block.
            MODE.store(FXSAVE, Ordering::Relaxed);
        } else {
            SIZE.store(size, Ordering::Relaxed);
        }
    }
    let mut text = TextBuf::<128>::new();
    let xcr0 = XCR0.load(Ordering::Relaxed);
    let _ = match MODE.load(Ordering::Relaxed) {
        NONE => write!(text, "not saved (no FXSAVE)"),
        mode => {
            let name = match mode {
                FXSAVE => "FXSAVE",
                XSAVE => "XSAVE",
                _ => "XSAVEOPT",
            };
            let _ = write!(text, "{name} on every switch: x87, SSE");
            if mode >= XSAVE && xcr0 & AVX != 0 {
                let _ = write!(text, ", AVX");
            }
            if mode >= XSAVE && xcr0 & AVX512 != 0 {
                let _ = write!(text, ", AVX-512");
            }
            if mode >= XSAVE && xcr0 & PKRU != 0 {
                let _ = write!(text, ", PKRU");
            }
            if mode >= XSAVE && xcr0 & AMX != 0 {
                let _ = write!(text, ", AMX held off");
            }
            write!(text, "; {}-byte areas", SIZE.load(Ordering::Relaxed))
        }
    };
    let _ = DESCRIPTION.set(text);
}

/// Turns on the FPU and the extended state this kernel saves, on this CPU.
pub fn init_cpu() {
    let mode = MODE.load(Ordering::Relaxed);
    // CR0: MP (bit 1) and NE (bit 5) on, EM (bit 2) and TS (bit 3) off, so
    // FPU and SSE instructions run without trapping and report errors as
    // exceptions.
    let cr0 = (cpu::cr0() | (1 << 1) | (1 << 5)) & !((1 << 2) | (1 << 3));
    // SAFETY: these bits only control how the FPU reports errors and
    // whether its instructions trap; protected mode and paging stay on.
    unsafe { cpu::write_cr0(cr0) };
    if mode == NONE {
        return;
    }
    // CR4: OSFXSR (9) and OSXMMEXCPT (10) for SSE, OSXSAVE (18) for XSAVE.
    let mut cr4 = cpu::cr4() | (1 << 9) | (1 << 10);
    if mode >= XSAVE {
        cr4 |= 1 << 18;
    }
    // SAFETY: the CPU supports each bit set (CPUID says so, checked on the
    // boot CPU; every CPU of a machine has the same features).
    unsafe { cpu::write_cr4(cr4) };
    if mode >= XSAVE {
        // SAFETY: OSXSAVE is now set, and XCR0 holds only components CPUID
        // reported, with x87 on, AVX only with SSE, and AVX-512's three
        // and AMX's two each all or none.
        unsafe { cpu::xsetbv(0, XCR0.load(Ordering::Relaxed)) };
        let held = HELD.load(Ordering::Relaxed);
        if held != 0 {
            // SAFETY: CPUID reported XFD for the held component (checked
            // on the boot CPU), so both MSRs exist and take its bit. Held
            // off, it is never saved or loaded, and its instructions fault
            // with #NM, which records it in XFD_ERR, cleared here.
            unsafe {
                cpu::wrmsr(msr::XFD, held);
                cpu::wrmsr(msr::XFD_ERR, 0);
            }
        }
    }
    // SAFETY: FNINIT resets the x87 FPU to its initial state.
    unsafe { asm!("fninit", options(nomem, nostack)) };
}

/// Ends a device-not-available fault (#NM) from user mode, which only an
/// instruction of a component XFD holds off raises here (CR0.TS and EM
/// stay clear): clears XFD_ERR, which the CPU leaves naming the component.
pub fn end_held_fault() {
    if HELD.load(Ordering::Relaxed) != 0 {
        // SAFETY: XFD is in use (`init_cpu`), so the MSR exists.
        unsafe { cpu::wrmsr(msr::XFD_ERR, 0) };
    }
}

/// How extended state is saved, for the boot report.
pub fn describe() -> &'static str {
    DESCRIPTION.get().map_or("not set up", |text| text.as_str())
}

/// Writes the initial state into the save area at `area` (64-byte aligned,
/// [`AREA_MAX`] bytes, zeroed): the x87 control word and MXCSR at their
/// power-on values, and an XSAVE header saying every component is in its
/// initial state.
///
/// # Safety
///
/// `area` must point at [`AREA_MAX`] writable bytes that nothing else uses.
pub unsafe fn init_area(area: usize) {
    // SAFETY: the caller guarantees the area; offset 0 is FCW, offset 24
    // MXCSR in the legacy region, and the XSAVE header at 512 is left
    // zero (XSTATE_BV = 0: everything initial).
    unsafe {
        (area as *mut u16).write(0x37f);
        ((area + 24) as *mut u32).write(0x1f80);
    }
}

/// Saves this CPU's extended state into `area`.
///
/// # Safety
///
/// `area` must be a save area set up by [`init_area`] that only the
/// running thread uses.
pub unsafe fn save(area: usize) {
    let (low, high) = asked();
    match MODE.load(Ordering::Relaxed) {
        // SAFETY: the caller guarantees the area: 64-byte aligned and large
        // enough for the components asked for (`SIZE`), every enabled one
        // but those held off.
        XSAVEOPT => unsafe {
            asm!("xsaveopt64 [{}]", in(reg) area, in("eax") low, in("edx") high,
                 options(nostack, preserves_flags));
        },
        // SAFETY: as above.
        XSAVE => unsafe {
            asm!("xsave64 [{}]", in(reg) area, in("eax") low, in("edx") high,
                 options(nostack, preserves_flags));
        },
        // SAFETY: as above, for the 512-byte legacy area.
        FXSAVE => unsafe { asm!("fxsave64 [{}]", in(reg) area, options(nostack, preserves_flags)) },
        _ => {}
    }
}

/// Loads this CPU's extended state from `area`.
///
/// # Safety
///
/// As for [`save`]; the area must hold state saved by [`save`] or written
/// by [`init_area`].
pub unsafe fn restore(area: usize) {
    let (low, high) = asked();
    match MODE.load(Ordering::Relaxed) {
        // SAFETY: the caller guarantees the area holds a valid image (its
        // reserved header bytes are zero, MXCSR has no reserved bits set),
        // as large as the components asked for.
        XSAVE | XSAVEOPT => unsafe {
            asm!("xrstor64 [{}]", in(reg) area, in("eax") low, in("edx") high,
                 options(nostack, preserves_flags, readonly));
        },
        // SAFETY: as above.
        FXSAVE => unsafe {
            asm!("fxrstor64 [{}]", in(reg) area, options(nostack, preserves_flags, readonly));
        },
        _ => {}
    }
}

/// The components saves and restores ask for, as EAX and EDX.
fn asked() -> (u32, u32) {
    let asked = ASKED.load(Ordering::Relaxed);
    (asked as u32, (asked >> 32) as u32)
}

/// Loads the initial state, as a new thread starts with: what a signal
/// handler starts with.
pub fn load_initial() {
    // SAFETY: the image is valid, 64-byte aligned, and as large as any
    // area, and nothing writes it.
    unsafe { restore(INITIAL.0.as_ptr() as usize) };
}

/// The size of a save area's image: what [`save`] writes and [`restore`]
/// reads, and what a signal frame holds.
pub fn image_size() -> usize {
    SIZE.load(Ordering::Relaxed) as usize
}

/// The length of an image's head: the legacy region and, with XSAVE, the
/// header; with FXSAVE, the whole image.
pub fn head_size() -> usize {
    if MODE.load(Ordering::Relaxed) >= XSAVE { HEAD } else { image_size() }
}

/// The parts of an image a signal frame holds, as ranges of it in order,
/// from its head: the head, then each component the header names as not
/// in its initial state, since XRSTOR reads no other; parts that meet are
/// one. Linux's XSAVE into the frame writes no others: for a program that
/// never used AVX-512, 832 bytes of the 2.7 KiB image, and protection keys'
/// 8 bytes past AVX-512's place, where the CPU counts them in use, do not
/// bring the bytes between along. With FXSAVE, the whole image.
pub fn used_parts(head: &[u8]) -> Parts {
    let size = image_size();
    let bv = match head.get(512..520) {
        Some(bv) if MODE.load(Ordering::Relaxed) >= XSAVE => {
            u64::from_le_bytes(bv.try_into().unwrap())
        }
        _ => return Parts { next: 0..size, rest: 0 },
    };
    let rest = bv & XCR0.load(Ordering::Relaxed) & !(HELD.load(Ordering::Relaxed) | X87 | SSE);
    Parts { next: 0..HEAD.min(size), rest }
}

/// The parts [`used_parts`] gives.
pub struct Parts {
    /// The next part; empty once every part is given.
    next: Range<usize>,
    /// The components not yet given, one bit each.
    rest: u64,
}

impl Iterator for Parts {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Range<usize>> {
        if self.next.is_empty() {
            return None;
        }
        let size = image_size();
        let mut part = core::mem::replace(&mut self.next, 0..0);
        while self.rest != 0 {
            let component = self.rest.trailing_zeros() as usize;
            self.rest &= self.rest - 1;
            let place = |table: &[AtomicU32]| {
                table.get(component).map_or(0, |at| at.load(Ordering::Relaxed) as usize).min(size)
            };
            let (start, end) = (place(&STARTS), place(&ENDS));
            if start >= end {
                continue;
            }
            // Components lie in the order of their numbers.
            if start <= part.end {
                part.end = part.end.max(end);
            } else {
                self.next = start..end;
                break;
            }
        }
        Some(part)
    }
}

/// Makes an image that came from user memory (a signal frame's) safe to
/// load: MXCSR without reserved bits and, for XSAVE, a header naming only
/// the components this kernel enables in the standard format, with its
/// reserved bytes zero. Loading anything else would fault in the kernel.
/// AMX's components are never loaded from one: held off, they are always
/// initial, and a tile configuration XRSTOR found invalid would fault.
pub fn sanitize(image: &mut [u8]) {
    if image.len() < 512 {
        return;
    }
    let word = |image: &[u8], at: usize| u32::from_le_bytes(image[at..at + 4].try_into().unwrap());
    let mask = match word(image, 28) {
        // A zero MXCSR_MASK means the default mask.
        0 => 0xffbf,
        mask => mask,
    };
    // The mask this CPU accepts is what FXSAVE reports on it; a program's
    // image may claim any mask, so only the bits of both are kept.
    let mxcsr = word(image, 24) & mask & cpu_mxcsr_mask();
    image[24..28].copy_from_slice(&mxcsr.to_le_bytes());
    if MODE.load(Ordering::Relaxed) >= XSAVE && image.len() >= 576 {
        let bv = u64::from_le_bytes(image[512..520].try_into().unwrap());
        let bv = bv & XCR0.load(Ordering::Relaxed) & !AMX;
        image[512..520].copy_from_slice(&bv.to_le_bytes());
        image[520..576].fill(0);
    }
}

/// The MXCSR bits this CPU accepts, from FXSAVE's MXCSR_MASK.
fn cpu_mxcsr_mask() -> u32 {
    static MASK: AtomicU32 = AtomicU32::new(0);
    let known = MASK.load(Ordering::Relaxed);
    if known != 0 {
        return known;
    }
    #[repr(C, align(16))]
    struct Legacy([u8; 512]);
    let mut legacy = Legacy([0; 512]);
    if MODE.load(Ordering::Relaxed) != NONE {
        // SAFETY: FXSAVE writes 512 bytes at a 16-byte aligned address,
        // which `legacy` is; it reads the FPU and changes nothing.
        unsafe {
            asm!("fxsave64 [{}]", in(reg) legacy.0.as_mut_ptr(), options(nostack, preserves_flags));
        };
    }
    let mask = match u32::from_le_bytes(legacy.0[28..32].try_into().unwrap()) {
        0 => 0xffbf,
        mask => mask,
    };
    MASK.store(mask, Ordering::Relaxed);
    mask
}

/// Whether the AVX registers' upper halves are saved, so the self-test
/// checks them too.
fn avx() -> bool {
    MODE.load(Ordering::Relaxed) >= XSAVE && XCR0.load(Ordering::Relaxed) & AVX != 0
}

/// Fills XMM0 to XMM15 (and with AVX the upper halves of YMM0 to YMM15)
/// with values made from `seed`.
pub fn fill(seed: u64) {
    if MODE.load(Ordering::Relaxed) == NONE {
        return;
    }
    // SAFETY: the kernel never uses the vector registers, so overwriting
    // them affects only the running thread's own extended state, which the
    // context switch keeps. RAX is the only general register used.
    unsafe {
        asm!(
            "movq xmm0, rax", "inc rax", "movq xmm1, rax", "inc rax",
            "movq xmm2, rax", "inc rax", "movq xmm3, rax", "inc rax",
            "movq xmm4, rax", "inc rax", "movq xmm5, rax", "inc rax",
            "movq xmm6, rax", "inc rax", "movq xmm7, rax", "inc rax",
            "movq xmm8, rax", "inc rax", "movq xmm9, rax", "inc rax",
            "movq xmm10, rax", "inc rax", "movq xmm11, rax", "inc rax",
            "movq xmm12, rax", "inc rax", "movq xmm13, rax", "inc rax",
            "movq xmm14, rax", "inc rax", "movq xmm15, rax",
            inout("rax") seed => _,
            options(nomem, nostack),
        );
    };
    if avx() {
        // SAFETY: as above; VPERM2F128 with 0 copies each register's low
        // half into its high half (AVX, enabled in XCR0).
        unsafe {
            asm!(
                "vperm2f128 ymm0, ymm0, ymm0, 0",
                "vperm2f128 ymm1, ymm1, ymm1, 0",
                "vperm2f128 ymm2, ymm2, ymm2, 0",
                "vperm2f128 ymm3, ymm3, ymm3, 0",
                "vperm2f128 ymm4, ymm4, ymm4, 0",
                "vperm2f128 ymm5, ymm5, ymm5, 0",
                "vperm2f128 ymm6, ymm6, ymm6, 0",
                "vperm2f128 ymm7, ymm7, ymm7, 0",
                "vperm2f128 ymm8, ymm8, ymm8, 0",
                "vperm2f128 ymm9, ymm9, ymm9, 0",
                "vperm2f128 ymm10, ymm10, ymm10, 0",
                "vperm2f128 ymm11, ymm11, ymm11, 0",
                "vperm2f128 ymm12, ymm12, ymm12, 0",
                "vperm2f128 ymm13, ymm13, ymm13, 0",
                "vperm2f128 ymm14, ymm14, ymm14, 0",
                "vperm2f128 ymm15, ymm15, ymm15, 0",
                options(nomem, nostack, preserves_flags),
            );
        };
    }
}

/// Whether the registers [`fill`] wrote still hold the values made from
/// `seed`.
pub fn holds(seed: u64) -> bool {
    if MODE.load(Ordering::Relaxed) == NONE {
        return true;
    }
    let mut low = [0u64; 16];
    // SAFETY: reads the vector registers into `low`, a local array of 16
    // words, through RDI; nothing else is touched.
    unsafe {
        asm!(
            "movq [rdi], xmm0", "movq [rdi + 8], xmm1", "movq [rdi + 16], xmm2",
            "movq [rdi + 24], xmm3", "movq [rdi + 32], xmm4", "movq [rdi + 40], xmm5",
            "movq [rdi + 48], xmm6", "movq [rdi + 56], xmm7", "movq [rdi + 64], xmm8",
            "movq [rdi + 72], xmm9", "movq [rdi + 80], xmm10", "movq [rdi + 88], xmm11",
            "movq [rdi + 96], xmm12", "movq [rdi + 104], xmm13", "movq [rdi + 112], xmm14",
            "movq [rdi + 120], xmm15",
            in("rdi") low.as_mut_ptr(),
            options(nostack, preserves_flags),
        );
    };
    let mut ok = low.iter().enumerate().all(|(i, &v)| v == seed.wrapping_add(i as u64));
    if ok && avx() {
        let mut high = [0u64; 16];
        // SAFETY: as above; VEXTRACTF128 moves each high half into XMM0
        // (clearing the rest of YMM0, so YMM0's own goes first), and
        // `fill` puts the pattern back afterwards.
        unsafe {
            asm!(
                "vextractf128 xmm0, ymm0, 1", "movq [rdi], xmm0",
                "vextractf128 xmm0, ymm1, 1", "movq [rdi + 8], xmm0",
                "vextractf128 xmm0, ymm2, 1", "movq [rdi + 16], xmm0",
                "vextractf128 xmm0, ymm3, 1", "movq [rdi + 24], xmm0",
                "vextractf128 xmm0, ymm4, 1", "movq [rdi + 32], xmm0",
                "vextractf128 xmm0, ymm5, 1", "movq [rdi + 40], xmm0",
                "vextractf128 xmm0, ymm6, 1", "movq [rdi + 48], xmm0",
                "vextractf128 xmm0, ymm7, 1", "movq [rdi + 56], xmm0",
                "vextractf128 xmm0, ymm8, 1", "movq [rdi + 64], xmm0",
                "vextractf128 xmm0, ymm9, 1", "movq [rdi + 72], xmm0",
                "vextractf128 xmm0, ymm10, 1", "movq [rdi + 80], xmm0",
                "vextractf128 xmm0, ymm11, 1", "movq [rdi + 88], xmm0",
                "vextractf128 xmm0, ymm12, 1", "movq [rdi + 96], xmm0",
                "vextractf128 xmm0, ymm13, 1", "movq [rdi + 104], xmm0",
                "vextractf128 xmm0, ymm14, 1", "movq [rdi + 112], xmm0",
                "vextractf128 xmm0, ymm15, 1", "movq [rdi + 120], xmm0",
                in("rdi") high.as_mut_ptr(),
                options(nostack, preserves_flags),
            );
        };
        ok = high.iter().enumerate().all(|(i, &v)| v == seed.wrapping_add(i as u64));
        // Put XMM0 and the high half of YMM0 back as `fill` left them.
        fill(seed);
    }
    ok
}
