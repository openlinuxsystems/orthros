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
//! With XSAVE the kernel enables x87, SSE, AVX and AVX-512 where the CPU
//! has them (XCR0) and saves with XSAVEOPT where it can, which skips
//! registers unchanged since the last restore. MPX, protection keys and AMX
//! are left off: nothing uses them yet, and AMX's 8 KiB of tile state would
//! not fit the save area. Without XSAVE, FXSAVE saves x87 and SSE.

use core::arch::asm;
use core::fmt::Write;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use radix_klog::TextBuf;
use radix_sync::Once;

use crate::cpu;

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
static DESCRIPTION: Once<TextBuf<96>> = Once::new();

/// XCR0 bits: x87, SSE, AVX, and the three AVX-512 components.
const X87: u64 = 1;
const SSE: u64 = 1 << 1;
const AVX: u64 = 1 << 2;
const AVX512: u64 = 0b111 << 5;

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
        let mut wanted = supported & (X87 | SSE | AVX | AVX512);
        if wanted & AVX512 != AVX512 || wanted & AVX == 0 {
            wanted &= !AVX512;
        }
        if wanted & SSE == 0 {
            wanted &= !AVX;
        }
        XCR0.store(wanted | X87, Ordering::Relaxed);
    }
    MODE.store(mode, Ordering::Relaxed);
    init_cpu();
    if mode >= XSAVE {
        // With XCR0 set, EBX is the size of an area for what it enables.
        let size = cpu::cpuid(0xd, 0).ebx;
        if size as usize > AREA_MAX {
            // Cannot happen with the components above; fall back to the
            // legacy area rather than overrun the block.
            MODE.store(FXSAVE, Ordering::Relaxed);
        } else {
            SIZE.store(size, Ordering::Relaxed);
        }
    }
    let mut text = TextBuf::<96>::new();
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
        // reported, with x87 on and AVX only with SSE.
        unsafe { cpu::xsetbv(0, XCR0.load(Ordering::Relaxed)) };
    }
    // SAFETY: FNINIT resets the x87 FPU to its initial state.
    unsafe { asm!("fninit", options(nomem, nostack)) };
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
    match MODE.load(Ordering::Relaxed) {
        // SAFETY: the caller guarantees the area: 64-byte aligned and large
        // enough for what XCR0 enables (`SIZE`); EDX:EAX all ones saves
        // every enabled component.
        XSAVEOPT => unsafe {
            asm!("xsaveopt64 [{}]", in(reg) area, in("eax") u32::MAX, in("edx") u32::MAX,
                 options(nostack, preserves_flags))
        },
        // SAFETY: as above.
        XSAVE => unsafe {
            asm!("xsave64 [{}]", in(reg) area, in("eax") u32::MAX, in("edx") u32::MAX,
                 options(nostack, preserves_flags))
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
    match MODE.load(Ordering::Relaxed) {
        // SAFETY: the caller guarantees the area holds a valid image (its
        // reserved header bytes are zero, MXCSR has no reserved bits set).
        XSAVE | XSAVEOPT => unsafe {
            asm!("xrstor64 [{}]", in(reg) area, in("eax") u32::MAX, in("edx") u32::MAX,
                 options(nostack, preserves_flags, readonly))
        },
        // SAFETY: as above.
        FXSAVE => unsafe {
            asm!("fxrstor64 [{}]", in(reg) area, options(nostack, preserves_flags, readonly))
        },
        _ => {}
    }
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
        )
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
            )
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
        )
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
            )
        };
        ok = high.iter().enumerate().all(|(i, &v)| v == seed.wrapping_add(i as u64));
        // Put XMM0 and the high half of YMM0 back as `fill` left them.
        fill(seed);
    }
    ok
}
