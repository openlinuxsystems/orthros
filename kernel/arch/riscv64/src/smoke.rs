// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The RISC-V smoke boot (ADR 0270), from the *RISC-V Supervisor Binary
//! Interface Specification* (2.0) and the *RISC-V Privileged
//! Architecture* (20211203).
//!
//! OpenSBI, which QEMU's `virt` machine runs in machine mode, jumps to
//! `_start` in supervisor mode with the hart's ID and the device tree's
//! address. `_start` sets the boot stack up, zeroes the data, and calls
//! [`main`], which:
//!
//! 1. asks SBI's base extension which SBI this is, and which extensions
//!    it has, and prints the banner through the Debug Console extension
//!    (DBCN), or the legacy console where there is none, with the
//!    machine's model from the device tree;
//! 2. turns Sv39 paging on, with the first gigabyte of RAM mapped where
//!    it is and again where the kernel's half of the address space
//!    starts, and reads the same word through both;
//! 3. asks the Timer extension for an interrupt a millisecond later, and
//!    waits for it, its trap handler clearing it;
//! 4. powers the machine off through the System Reset extension (SRST):
//!    a "system failure" reset if anything went wrong, which QEMU ends
//!    with a status of its own.
//!
//! Nothing of the common kernel runs here: the stubs beside it are what
//! keeps that portable.

#![allow(unsafe_code)]

use core::arch::{asm, global_asm};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

global_asm!(
    ".section .text.entry, \"ax\"",
    ".globl _start",
    "_start:",
    "    .option push",
    "    .option norelax",
    "    la sp, __boot_stack_top",
    "    .option pop",
    // The floating-point registers on, as Rust's code may use them.
    "    li t0, 1 << 13",
    "    csrs sstatus, t0",
    "    la t0, __bss_start",
    "    la t1, __bss_end",
    "1:  bgeu t0, t1, 2f",
    "    sd zero, 0(t0)",
    "    addi t0, t0, 8",
    "    j 1b",
    "2:  call {main}",
    "3:  wfi",
    "    j 3b",
    "",
    // Supervisor traps: the registers a call may change are saved, the
    // handler is called with scause, sepc and stval, and the trap returns.
    ".section .text",
    ".balign 4",
    ".globl radix_smoke_trap",
    "radix_smoke_trap:",
    "    addi sp, sp, -128",
    "    sd ra, 0(sp)",
    "    sd t0, 8(sp)",
    "    sd t1, 16(sp)",
    "    sd t2, 24(sp)",
    "    sd t3, 32(sp)",
    "    sd t4, 40(sp)",
    "    sd t5, 48(sp)",
    "    sd t6, 56(sp)",
    "    sd a0, 64(sp)",
    "    sd a1, 72(sp)",
    "    sd a2, 80(sp)",
    "    sd a3, 88(sp)",
    "    sd a4, 96(sp)",
    "    sd a5, 104(sp)",
    "    sd a6, 112(sp)",
    "    sd a7, 120(sp)",
    "    csrr a0, scause",
    "    csrr a1, sepc",
    "    csrr a2, stval",
    "    call {trap}",
    "    ld ra, 0(sp)",
    "    ld t0, 8(sp)",
    "    ld t1, 16(sp)",
    "    ld t2, 24(sp)",
    "    ld t3, 32(sp)",
    "    ld t4, 40(sp)",
    "    ld t5, 48(sp)",
    "    ld t6, 56(sp)",
    "    ld a0, 64(sp)",
    "    ld a1, 72(sp)",
    "    ld a2, 80(sp)",
    "    ld a3, 88(sp)",
    "    ld a4, 96(sp)",
    "    ld a5, 104(sp)",
    "    ld a6, 112(sp)",
    "    ld a7, 120(sp)",
    "    addi sp, sp, 128",
    "    sret",
    main = sym main,
    trap = sym trap,
);

unsafe extern "C" {
    /// The trap entry above.
    fn radix_smoke_trap();
}

/// SBI's extensions, by their IDs.
mod eid {
    pub const LEGACY_SET_TIMER: usize = 0x00;
    pub const LEGACY_PUTCHAR: usize = 0x01;
    pub const LEGACY_SHUTDOWN: usize = 0x08;
    pub const BASE: usize = 0x10;
    pub const TIMER: usize = 0x5449_4d45;
    pub const SYSTEM_RESET: usize = 0x5352_5354;
    pub const DEBUG_CONSOLE: usize = 0x4442_434e;
}

/// An SBI call: `(error, value)`.
fn sbi(extension: usize, function: usize, args: [usize; 3]) -> (isize, usize) {
    let (error, value);
    // SAFETY: an environment call to the SBI implementation, which
    // changes only a0 and a1 (2.1); the arguments are plain numbers, and
    // the one call given an address (DBCN's write) gives that of bytes
    // that live through the call.
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") args[0] => error,
            inlateout("a1") args[1] => value,
            in("a2") args[2],
            in("a6") function,
            in("a7") extension,
            options(nostack),
        );
    }
    (error, value)
}

/// A legacy SBI call (v0.1), whose answer is in a0 alone.
fn sbi_legacy(extension: usize, arg: usize) {
    // SAFETY: as for `sbi`; legacy calls change only a0.
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") arg => _,
            in("a7") extension,
            options(nostack),
        );
    }
}

/// Whether the SBI has `extension` (the base extension's probe).
fn has(extension: usize) -> bool {
    let (error, value) = sbi(eid::BASE, 3, [extension, 0, 0]);
    error == 0 && value != 0
}

/// Whether the console writes through DBCN, as SBI 2.0 has it.
static DBCN: AtomicBool = AtomicBool::new(false);

/// The SBI console.
struct Console;

impl Write for Console {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if DBCN.load(Ordering::Relaxed) {
            let mut rest = text.as_bytes();
            while !rest.is_empty() {
                // Memory is mapped where it is, so the address is the
                // physical one DBCN wants.
                let (error, written) =
                    sbi(eid::DEBUG_CONSOLE, 0, [rest.len(), rest.as_ptr() as usize, 0]);
                if error != 0 {
                    return Err(fmt::Error);
                }
                rest = &rest[written.min(rest.len())..];
            }
        } else {
            for &byte in text.as_bytes() {
                sbi_legacy(eid::LEGACY_PUTCHAR, usize::from(byte));
            }
        }
        Ok(())
    }
}

macro_rules! say {
    ($($arg:tt)*) => {{
        let _ = writeln!(Console, $($arg)*);
    }};
}

/// Powers the machine off: a shutdown, or one for a system failure.
fn power_off(failed: bool) -> ! {
    if has(eid::SYSTEM_RESET) {
        let _ = sbi(eid::SYSTEM_RESET, 0, [0, usize::from(failed), 0]);
    }
    sbi_legacy(eid::LEGACY_SHUTDOWN, 0);
    loop {
        // SAFETY: waits for an interrupt; nothing else.
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}

/// The smoke boot's panic handler: says why, and fails.
pub fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    say!("Panic: {}", info.message());
    if let Some(place) = info.location() {
        say!("Panic: at {}:{}", place.file(), place.line());
    }
    power_off(true)
}

/// The names of SBI implementations (the base extension's IDs).
fn implementation(id: usize) -> &'static str {
    match id {
        0 => "the Berkeley Boot Loader",
        1 => "OpenSBI",
        2 => "Xvisor",
        3 => "KVM",
        4 => "RustSBI",
        5 => "Diosix",
        6 => "Coffer",
        _ => "an SBI implementation",
    }
}

/// What the device tree says: the machine's model, and how fast `time`
/// counts.
struct Tree {
    model: [u8; 64],
    model_len: usize,
    timebase: u64,
}

impl Tree {
    /// Reads the flattened device tree at `address`, if it is one.
    fn read(address: usize) -> Option<Tree> {
        const MAGIC: u32 = 0xd00d_feed;
        let word = |at: usize| -> u32 {
            // SAFETY: the device tree OpenSBI passes lies in RAM, mapped
            // where it is, and `at` stays within the size it gives.
            u32::from_be(unsafe { core::ptr::read_volatile((address + at) as *const u32) })
        };
        if address == 0 || !address.is_multiple_of(4) || word(0) != MAGIC {
            return None;
        }
        let total = word(4) as usize;
        let structs = word(8) as usize;
        let strings = word(12) as usize;
        let byte = |at: usize| -> u8 {
            // SAFETY: as for `word`.
            unsafe { core::ptr::read_volatile((address + at) as *const u8) }
        };
        let name_is = |at: usize, name: &[u8]| {
            name.iter().enumerate().all(|(i, &b)| at + i < total && byte(at + i) == b)
                && at + name.len() < total
                && byte(at + name.len()) == 0
        };
        let mut tree = Tree { model: [0; 64], model_len: 0, timebase: 0 };
        let mut at = structs;
        let mut depth = 0usize;
        let mut in_cpus = false;
        while at + 4 <= total {
            match word(at) {
                // FDT_BEGIN_NODE: a name, then padding to a word.
                1 => {
                    at += 4;
                    depth += 1;
                    if depth == 2 {
                        in_cpus = name_is(at, b"cpus");
                    }
                    while at < total && byte(at) != 0 {
                        at += 1;
                    }
                    at = (at + 4) & !3;
                }
                // FDT_END_NODE
                2 => {
                    at += 4;
                    depth = depth.saturating_sub(1);
                    if depth < 2 {
                        in_cpus = false;
                    }
                }
                // FDT_PROP: its length, its name's offset, its value.
                3 => {
                    let len = word(at + 4) as usize;
                    let name = strings + word(at + 8) as usize;
                    let value = at + 12;
                    if depth == 1 && name_is(name, b"model") {
                        let n = len.saturating_sub(1).min(tree.model.len());
                        for (i, slot) in tree.model.iter_mut().enumerate().take(n) {
                            *slot = byte(value + i);
                        }
                        tree.model_len = n;
                    } else if depth == 2 && in_cpus && name_is(name, b"timebase-frequency") {
                        tree.timebase = match len {
                            4 => u64::from(word(value)),
                            8 => (u64::from(word(value)) << 32) | u64::from(word(value + 4)),
                            _ => 0,
                        };
                    }
                    at = (value + len + 3) & !3;
                }
                // FDT_NOP
                4 => at += 4,
                // FDT_END, or what is not a token.
                _ => break,
            }
        }
        Some(tree)
    }

    fn model(&self) -> &str {
        core::str::from_utf8(&self.model[..self.model_len]).unwrap_or("a machine")
    }
}

/// Sv39's root page table: the first gigabyte of RAM (0x80000000) mapped
/// where it is and at the start of the kernel's half, as gigapages that
/// may be read, written and run, global, accessed and dirty.
#[repr(C, align(4096))]
struct Table([u64; 512]);

/// Where RAM starts on QEMU's `virt`, and where the kernel's half maps it.
const RAM: u64 = 0x8000_0000;
const HIGH: u64 = 0xffff_ffc0_8000_0000;

const fn gigapage(physical: u64) -> u64 {
    // V, R, W, X, G, A and D.
    ((physical >> 12) << 10) | 0xef
}

const fn root() -> Table {
    let mut entries = [0u64; 512];
    entries[((RAM >> 30) & 0x1ff) as usize] = gigapage(RAM);
    entries[((HIGH >> 30) & 0x1ff) as usize] = gigapage(RAM);
    Table(entries)
}

static ROOT: Table = root();

/// What both mappings of RAM are read for.
static MARK: AtomicU64 = AtomicU64::new(0);

/// Turns Sv39 on; whether it is, and the word read through the high
/// mapping matches.
fn page() -> Result<(), &'static str> {
    let root = core::ptr::addr_of!(ROOT) as u64;
    let satp = (8u64 << 60) | (root >> 12);
    // SAFETY: the table maps the RAM this code, its data and its stack
    // are in where they already are, so execution goes on unchanged; the
    // fence makes the hart use the new table.
    unsafe {
        asm!("csrw satp, {0}", "sfence.vma", in(reg) satp, options(nostack));
    }
    let now: u64;
    // SAFETY: reads a CSR.
    unsafe { asm!("csrr {0}, satp", out(reg) now, options(nomem, nostack)) };
    if now >> 60 != 8 {
        return Err("the hart does not have Sv39");
    }
    MARK.store(0x5241_4449_5853_4d4b, Ordering::SeqCst);
    let low = core::ptr::addr_of!(MARK) as u64;
    let high = low - RAM + HIGH;
    // SAFETY: `high` is MARK's address through the second mapping, which
    // maps the same page.
    let seen = unsafe { core::ptr::read_volatile(high as *const u64) };
    if seen != MARK.load(Ordering::SeqCst) {
        return Err("the high mapping reads another word");
    }
    Ok(())
}

/// The `time` CSR.
fn time() -> u64 {
    let now: u64;
    // SAFETY: reads a counter OpenSBI lets supervisor mode read.
    unsafe { asm!("rdtime {0}", out(reg) now, options(nomem, nostack)) };
    now
}

/// Asks for a timer interrupt at `when`, or none for `u64::MAX`.
fn set_timer(when: u64) {
    if has(eid::TIMER) {
        let _ = sbi(eid::TIMER, 0, [when as usize, 0, 0]);
    } else {
        sbi_legacy(eid::LEGACY_SET_TIMER, when as usize);
    }
}

/// When the timer interrupt came, once it has.
static FIRED: AtomicU64 = AtomicU64::new(0);

/// scause of the supervisor timer interrupt.
const TIMER_INTERRUPT: usize = (1 << 63) | 5;

extern "C" fn trap(scause: usize, sepc: usize, stval: usize) {
    if scause == TIMER_INTERRUPT {
        // The next one far off, which also clears this one.
        set_timer(u64::MAX);
        FIRED.store(time().max(1), Ordering::SeqCst);
        return;
    }
    say!(
        "Trap: scause {scause:#x} at {sepc:#x}, stval {stval:#x}, which the smoke boot does not take"
    );
    power_off(true)
}

/// Waits for one timer interrupt a millisecond off; how long it took, in
/// ticks of `time`.
fn tick(timebase: u64) -> Result<u64, &'static str> {
    let handler = radix_smoke_trap as *const () as usize;
    // SAFETY: points traps at the handler above (direct mode), enables
    // the supervisor timer interrupt and interrupts in supervisor mode.
    unsafe {
        asm!("csrw stvec, {0}", in(reg) handler, options(nostack));
        asm!("csrs sie, {0}", in(reg) 1usize << 5, options(nostack));
        asm!("csrs sstatus, {0}", in(reg) 1usize << 1, options(nostack));
    }
    let start = time();
    let wait = (timebase / 1000).max(1);
    set_timer(start + wait);
    // Ten seconds of the timebase, or a billion ticks if it is not known.
    let give_up = start + if timebase == 0 { 1_000_000_000 } else { timebase * 10 };
    while FIRED.load(Ordering::SeqCst) == 0 {
        if time() > give_up {
            return Err("no timer interrupt came in ten seconds");
        }
        // SAFETY: waits for an interrupt; the timer's wakes it.
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
    // SAFETY: interrupts off again.
    unsafe { asm!("csrc sstatus, {0}", in(reg) 1usize << 1, options(nostack)) };
    Ok(FIRED.load(Ordering::SeqCst).saturating_sub(start))
}

/// The smoke boot, from `_start`.
extern "C" fn main(hart: usize, tree: usize) -> ! {
    DBCN.store(has(eid::DEBUG_CONSOLE), Ordering::Relaxed);
    let tree = Tree::read(tree);
    let model = tree.as_ref().map_or("a machine with no device tree", Tree::model);
    let timebase = tree.as_ref().map_or(0, |tree| tree.timebase);
    say!(
        "Radix {} (riscv64): the smoke boot, on hart {hart} of {model}",
        env!("CARGO_PKG_VERSION")
    );
    let (_, spec) = sbi(eid::BASE, 0, [0; 3]);
    let (_, id) = sbi(eid::BASE, 1, [0; 3]);
    let (_, version) = sbi(eid::BASE, 2, [0; 3]);
    say!(
        "SBI: version {}.{}, by {} {}.{}; the console is {}",
        (spec >> 24) & 0x7f,
        spec & 0xff_ffff,
        implementation(id),
        version >> 16,
        version & 0xffff,
        if DBCN.load(Ordering::Relaxed) { "its Debug Console" } else { "its legacy console" }
    );
    match page() {
        Ok(()) => say!(
            "Paging: Sv39 is on; the first gigabyte of RAM is mapped at {RAM:#x} and at {HIGH:#x}, \
             and both read the same"
        ),
        Err(why) => {
            say!("Paging: Sv39 could not be turned on: {why}");
            power_off(true);
        }
    }
    match tick(timebase) {
        Ok(ticks) if timebase > 0 => say!(
            "Timer: the SBI timer interrupt came {} µs after it was asked for, at {} MHz",
            ticks * 1_000_000 / timebase,
            timebase / 1_000_000
        ),
        Ok(ticks) => {
            say!("Timer: the SBI timer interrupt came {ticks} ticks after it was asked for");
        }
        Err(why) => {
            say!("Timer: {why}");
            power_off(true);
        }
    }
    say!("Power: the smoke boot is done; powering off through SBI");
    power_off(false)
}
