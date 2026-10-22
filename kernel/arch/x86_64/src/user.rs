// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! User mode: the SYSCALL entry, entering user mode, faults in user mode,
//! copying to and from user memory, and memory shared with programs.
//!
//! A program enters the kernel with `syscall`. The CPU masks interrupts
//! (FMASK also clears DF, AC, TF, NT and IOPL), and the entry stub swaps GS
//! to this CPU's data, parks the program's stack pointer in a scratch word,
//! switches to the running thread's kernel stack (`gs:[24]`, set at every
//! context switch) and saves the program's registers there as a
//! [`SyscallFrame`], whose first eleven words are the HAL's
//! [`SyscallRegs`]. The kernel's handler reads its arguments from them and
//! writes its results back; the stub then restores every register from
//! the frame, so nothing of the kernel's leaks out, and returns with
//! `sysretq`. Before that, [`radix_syscall_dispatch`] makes sure the
//! return address is a user address, since SYSRET with a non-canonical one
//! would fault in kernel mode on Intel CPUs with the program's stack, and
//! that the flags the program gets back are ones it may have.
//!
//! Interrupts and exceptions from user mode switch to the same kernel stack
//! through the TSS, and their entry stub swaps GS by looking at the saved
//! CS (`crate::interrupts`). An exception in user mode runs the user fault
//! handler, which ends the thread.
//!
//! The kernel reads and writes user memory only through
//! [`copy_from_user`] and [`copy_to_user`], which check that the range is
//! in the user half and lift SMAP for the one `rep movsb` that copies. A
//! fault on that instruction does not stop the kernel: the fault handler
//! moves it on to the copy's end ([`fixup`]), and the copy reports how much
//! was left.

use core::fmt;
use core::sync::atomic::{AtomicU32, Ordering};

use radix_hal::{SyscallRegs, UserFault, UserFaultKind};
use radix_sync::Once;

use crate::cpu::{self, msr};
use crate::interrupts::TrapFrame;
use crate::memory::{self, PAGE};
use crate::paging;
use crate::percpu::{self, KERNEL_CS, KERNEL_RSP_OFFSET, SCRATCH_OFFSET, USER_BASE};
use crate::space::USER_END;

/// The first address above the user half: the last page of the lower half
/// stays unmapped, so no `syscall` instruction can sit at its very end and
/// return to a non-canonical address.
pub const USER_TOP: u64 = USER_END - PAGE;

/// The lowest address programs may map: the pages below stay unmapped, so
/// null pointers fault in user mode too.
pub const USER_START: u64 = 0x10000;

/// RFLAGS bits.
const FLAG_CARRY: u64 = 1 << 0;
const FLAG_RESERVED: u64 = 1 << 1;
const FLAG_PARITY: u64 = 1 << 2;
const FLAG_ADJUST: u64 = 1 << 4;
const FLAG_ZERO: u64 = 1 << 6;
const FLAG_SIGN: u64 = 1 << 7;
const FLAG_TRAP: u64 = 1 << 8;
const FLAG_INTERRUPT: u64 = 1 << 9;
const FLAG_DIRECTION: u64 = 1 << 10;
const FLAG_OVERFLOW: u64 = 1 << 11;
const FLAG_IOPL: u64 = 3 << 12;
const FLAG_NESTED: u64 = 1 << 14;
const FLAG_ALIGNMENT: u64 = 1 << 18;

/// The flags SYSCALL clears on entry: interrupts, single-stepping, the
/// direction and alignment-check flags (so the kernel runs with SMAP in
/// force), I/O privilege and nested task.
const ENTRY_MASK: u64 =
    FLAG_TRAP | FLAG_INTERRUPT | FLAG_DIRECTION | FLAG_IOPL | FLAG_NESTED | FLAG_ALIGNMENT;

/// The flags a program may hold, of those SYSRET restores.
const USER_FLAGS: u64 = FLAG_CARRY
    | FLAG_PARITY
    | FLAG_ADJUST
    | FLAG_ZERO
    | FLAG_SIGN
    | FLAG_DIRECTION
    | FLAG_OVERFLOW
    | FLAG_ALIGNMENT;

/// What the system call entry saves, lowest address first. The first
/// eleven words are [`SyscallRegs`]: the number (RAX) and the ten argument
/// registers in the ABI's order.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SyscallFrame {
    pub regs: SyscallRegs,
    pub rbx: u64,
    pub rbp: u64,
    /// The return address (from RCX).
    pub rip: u64,
    /// The program's flags (from R11).
    pub rflags: u64,
    pub rsp: u64,
}

const _: () = {
    assert!(core::mem::size_of::<SyscallRegs>() == 11 * 8);
    assert!(core::mem::size_of::<SyscallFrame>() == 16 * 8);
};

core::arch::global_asm!(
    r#"
    .section .text
    .balign 16
    .global radix_syscall_entry
radix_syscall_entry:
    swapgs
    mov gs:[{scratch}], rsp
    mov rsp, gs:[{kernel_rsp}]
    push qword ptr gs:[{scratch}]
    push r11
    push rcx
    push rbp
    push rbx
    push r15
    push r14
    push r13
    push r12
    push r9
    push r8
    push r10
    push rdx
    push rsi
    push rdi
    push rax
    /* A zero frame pointer ends kernel backtraces here. */
    xor ebp, ebp
    mov rdi, rsp
    call radix_syscall_dispatch
    cli
    pop rax
    pop rdi
    pop rsi
    pop rdx
    pop r10
    pop r8
    pop r9
    pop r12
    pop r13
    pop r14
    pop r15
    pop rbx
    pop rbp
    pop rcx
    pop r11
    pop rsp
    swapgs
    sysretq

    .global radix_enter_user
radix_enter_user:
    cli
    mov rcx, rdi
    mov r11, {initial_flags}
    mov rsp, rsi
    mov rdi, rdx
    xor eax, eax
    xor ebx, ebx
    xor edx, edx
    xor esi, esi
    xor ebp, ebp
    xor r8d, r8d
    xor r9d, r9d
    xor r10d, r10d
    xor r12d, r12d
    xor r13d, r13d
    xor r14d, r14d
    xor r15d, r15d
    swapgs
    sysretq

    .global radix_user_copy
radix_user_copy:
    mov rcx, rdx
    cmp byte ptr [rip + {smap}], 0
    je 2f
    stac
2:
    .global radix_user_copy_insn
radix_user_copy_insn:
    rep movsb
    .global radix_user_copy_done
radix_user_copy_done:
    cmp byte ptr [rip + {smap}], 0
    je 3f
    clac
3:
    mov rax, rcx
    ret
    "#,
    scratch = const SCRATCH_OFFSET,
    kernel_rsp = const KERNEL_RSP_OFFSET,
    initial_flags = const FLAG_INTERRUPT | FLAG_RESERVED,
    smap = sym paging::SMAP,
);

unsafe extern "C" {
    /// The SYSCALL entry; see the assembly above.
    fn radix_syscall_entry();
    /// Leaves for user mode at `entry` with the stack at `stack` and `arg`
    /// in RDI.
    fn radix_enter_user(entry: u64, stack: u64, arg: u64) -> !;
    /// Copies `len` bytes from `src` to `dst`, one side of them user
    /// memory; returns how many were left when a fault stopped it.
    fn radix_user_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    static radix_user_copy_insn: u8;
    static radix_user_copy_done: u8;
}

static SYSCALL_HANDLER: Once<fn(&mut SyscallRegs)> = Once::new();
static FAULT_HANDLER: Once<fn(&UserFault) -> !> = Once::new();
static RETURN_HANDLER: Once<fn()> = Once::new();

/// Registers the system call handler.
pub fn set_syscall_handler(handler: fn(&mut SyscallRegs)) {
    let _ = SYSCALL_HANDLER.set(handler);
}

/// Registers the user fault handler.
pub fn set_fault_handler(handler: fn(&UserFault) -> !) {
    let _ = FAULT_HANDLER.set(handler);
}

/// Registers the user return handler.
pub fn set_return_handler(handler: fn()) {
    let _ = RETURN_HANDLER.set(handler);
}

/// Runs the user return handler, if there is one: on the way back to user
/// mode from an interrupt.
pub fn on_return() {
    if let Some(handler) = RETURN_HANDLER.get() {
        handler();
    }
}

/// Turns on SYSCALL and points it at the entry stub. Every CPU runs this
/// when it loads its descriptor tables.
pub fn init_cpu() {
    let star = (u64::from(USER_BASE) << 48) | (u64::from(KERNEL_CS) << 32);
    // SAFETY: these MSRs exist on every x86-64 CPU. EFER.SCE only enables
    // the SYSCALL instruction, whose target is the entry stub; STAR names
    // this GDT's segments (kernel code at 0x08 and data at 0x10, user data
    // at 0x20 and code at 0x28); FMASK masks interrupts on entry, so the
    // stub runs with interrupts off until it is on the kernel stack.
    // KERNEL_GS_BASE is what `swapgs` gives user mode: zero, not this
    // CPU's data.
    unsafe {
        cpu::wrmsr(msr::EFER, cpu::rdmsr(msr::EFER) | 1);
        cpu::wrmsr(msr::STAR, star);
        cpu::wrmsr(msr::LSTAR, radix_syscall_entry as *const () as u64);
        cpu::wrmsr(msr::FMASK, ENTRY_MASK);
        cpu::wrmsr(msr::KERNEL_GS_BASE, 0);
    }
}

/// Called by the entry stub with the saved registers.
#[unsafe(no_mangle)]
extern "C" fn radix_syscall_dispatch(frame: &mut SyscallFrame) {
    match SYSCALL_HANDLER.get() {
        Some(handler) => handler(&mut frame.regs),
        None => frame.regs.number = u64::MAX,
    }
    // Back to the program with interrupts masked from here to SYSRET.
    cpu::disable_interrupts();
    if frame.rip >= USER_TOP {
        // Only a `syscall` that ends exactly at the top of the user half
        // gets here. Returning would be safe (the page above is never
        // mapped), but ending the thread keeps every SYSRET well below the
        // non-canonical addresses where it would fault in kernel mode.
        fault(UserFault { kind: UserFaultKind::Protection, pc: frame.rip, sp: frame.rsp, code: 0 });
    }
    frame.rflags = (frame.rflags & USER_FLAGS) | FLAG_INTERRUPT | FLAG_RESERVED;
}

/// Ends the running thread for a fault in user mode.
fn fault(fault: UserFault) -> ! {
    match FAULT_HANDLER.get() {
        Some(handler) => handler(&fault),
        None => panic!("User mode: {fault}, and no kernel handler for it"),
    }
}

/// Handles exception `vector` from user mode: the thread ends.
pub fn exception(vector: u8, frame: &TrapFrame) -> ! {
    let kind = match vector {
        0 => UserFaultKind::Divide,
        3 => UserFaultKind::Breakpoint,
        6 => UserFaultKind::InvalidOpcode,
        13 => UserFaultKind::Protection,
        14 => UserFaultKind::Page {
            address: cpu::cr2(),
            write: frame.error & (1 << 1) != 0,
            execute: frame.error & (1 << 4) != 0,
            present: frame.error & 1 != 0,
        },
        16 | 19 => UserFaultKind::FloatingPoint,
        17 => UserFaultKind::Alignment,
        v => UserFaultKind::Other(v),
    };
    fault(UserFault { kind, pc: frame.rip, sp: frame.rsp, code: frame.error })
}

/// Where a fault in kernel mode at `rip` resumes, if it is the copy
/// instruction of a user copy and the fault was on a user address.
pub fn fixup(rip: u64, address: Option<u64>) -> Option<u64> {
    let insn = &raw const radix_user_copy_insn as u64;
    if rip != insn || address.is_some_and(|a| a >= USER_TOP) {
        return None;
    }
    Some(&raw const radix_user_copy_done as u64)
}

/// Whether `len` bytes from `address` lie wholly in the user half.
fn user_range(address: u64, len: usize) -> bool {
    len == 0 || address.checked_add(len as u64).is_some_and(|end| end <= USER_TOP)
}

/// Copies user memory at `src` into `dst`; see the HAL.
pub fn copy_from_user(dst: &mut [u8], src: u64) -> bool {
    if !user_range(src, dst.len()) {
        return false;
    }
    // SAFETY: `dst` is kernel memory the caller lent for its whole length;
    // `src..src + len` is in the user half (checked above), where a fault
    // ends the copy early instead of stopping the kernel (`fixup`), so
    // whatever the program mapped there, or did not, the copy is sound.
    unsafe { radix_user_copy(dst.as_mut_ptr(), src as *const u8, dst.len()) == 0 }
}

/// Copies `src` into user memory at `dst`; see the HAL.
pub fn copy_to_user(dst: u64, src: &[u8]) -> bool {
    if !user_range(dst, src.len()) {
        return false;
    }
    // SAFETY: as in `copy_from_user`, the other way round: the kernel side
    // is `src`, which is only read.
    unsafe { radix_user_copy(dst as *mut u8, src.as_ptr(), src.len()) == 0 }
}

/// Leaves for user mode; see the HAL.
pub fn enter_user(entry: u64, stack_top: u64, arg: u64) -> ! {
    assert!(entry < USER_TOP && stack_top <= USER_TOP, "entering user mode outside the user half");
    let cpu = percpu::current();
    assert!(cpu.space.load(Ordering::Relaxed) != 0, "entering user mode without an address space");
    // SAFETY: the running context has an address space loaded (checked
    // above), `entry` and the stack are user addresses, and the stub
    // clears every other register, so the program starts with nothing of
    // the kernel's. The kernel stack below is abandoned: the next system
    // call or interrupt starts again at its top.
    unsafe { radix_enter_user(entry, stack_top, arg) }
}

/// Memory shared between the kernel and programs: pages from the mapping
/// window, which the kernel reads and writes through its own mapping.
pub struct Shared {
    virt: usize,
    pages: usize,
}

impl Shared {
    /// `pages` zeroed pages.
    pub fn new(pages: usize) -> Option<Shared> {
        let virt = paging::map_area(pages).ok()?;
        Some(Shared { virt, pages })
    }

    /// The frame of page `page`.
    pub fn phys(&self, page: usize) -> u64 {
        assert!(page < self.pages, "page {page} of a {}-page shared area", self.pages);
        paging::translate(self.virt + page * PAGE as usize).expect("shared memory is mapped")
    }

    fn word(&self, offset: usize) -> &AtomicU32 {
        assert!(
            offset.is_multiple_of(4) && offset + 4 <= self.pages * PAGE as usize,
            "word {offset:#x} of a {}-page shared area",
            self.pages
        );
        // SAFETY: the word is inside the area (checked above), 4-aligned,
        // and mapped for as long as `self` lives. Programs write it too,
        // so it is only ever accessed atomically from the kernel's side.
        unsafe { &*((self.virt + offset) as *const AtomicU32) }
    }

    /// Reads a word with acquire ordering.
    pub fn load(&self, offset: usize) -> u32 {
        self.word(offset).load(Ordering::Acquire)
    }

    /// Writes a word with release ordering.
    pub fn store(&self, offset: usize, value: u32) {
        self.word(offset).store(value, Ordering::Release);
    }

    fn check(&self, offset: usize, len: usize) {
        assert!(
            offset.checked_add(len).is_some_and(|end| end <= self.pages * PAGE as usize),
            "bytes {offset:#x}+{len} of a {}-page shared area",
            self.pages
        );
    }

    /// Copies bytes out from `offset`: in whole words when both are
    /// 8-byte aligned, as ring entries are.
    pub fn read(&self, offset: usize, dst: &mut [u8]) {
        self.check(offset, dst.len());
        let base = self.virt + offset;
        if base.is_multiple_of(8) && dst.len().is_multiple_of(8) {
            for (i, chunk) in dst.as_chunks_mut::<8>().0.iter_mut().enumerate() {
                // SAFETY: the words are inside the area (checked above)
                // and aligned. A program may write them meanwhile;
                // volatile reads see some value for each, never undefined
                // behaviour, and the kernel then works only with its copy.
                let word = unsafe { core::ptr::read_volatile((base + 8 * i) as *const u64) };
                *chunk = word.to_le_bytes();
            }
            return;
        }
        for (i, byte) in dst.iter_mut().enumerate() {
            // SAFETY: as above, a byte at a time.
            *byte = unsafe { core::ptr::read_volatile((base + i) as *const u8) };
        }
    }

    /// Copies bytes in at `offset`, in whole words when aligned.
    pub fn write(&self, offset: usize, src: &[u8]) {
        self.check(offset, src.len());
        let base = self.virt + offset;
        if base.is_multiple_of(8) && src.len().is_multiple_of(8) {
            for (i, chunk) in src.as_chunks::<8>().0.iter().enumerate() {
                let word = u64::from_le_bytes(*chunk);
                // SAFETY: as in `read`, writing.
                unsafe { core::ptr::write_volatile((base + 8 * i) as *mut u64, word) };
            }
            return;
        }
        for (i, &byte) in src.iter().enumerate() {
            // SAFETY: as in `read`, writing a byte at a time.
            unsafe { core::ptr::write_volatile((base + i) as *mut u8, byte) };
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        paging::unmap_area(self.virt, self.pages);
    }
}

/// Fills `pages` pages from `phys` with zeroes.
pub fn zero_frames(phys: u64, pages: u64) {
    assert!(memory::is_ram(phys..phys + pages * PAGE), "zeroing {phys:#x}: not RAM");
    // SAFETY: the range is RAM (checked above), which the direct map
    // covers; the caller owns the frames and nothing else uses them.
    unsafe {
        core::ptr::write_bytes(memory::phys_to_virt(phys) as *mut u8, 0, (pages * PAGE) as usize)
    };
}

/// Copies `src` into RAM at `phys`.
pub fn write_frames(phys: u64, src: &[u8]) {
    let end = phys + src.len() as u64;
    assert!(memory::is_ram(phys..end), "writing {phys:#x}: not RAM");
    // SAFETY: as in `zero_frames`.
    unsafe {
        core::ptr::copy_nonoverlapping(
            src.as_ptr(),
            memory::phys_to_virt(phys) as *mut u8,
            src.len(),
        )
    };
}

/// Describes user mode for the boot report.
pub fn describe(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
    let smap = paging::SMAP.load(Ordering::Relaxed);
    line(format_args!(
        "User mode: SYSCALL/SYSRET entry, user half {:#x}-{:#x}; SMEP {}, SMAP {}",
        USER_START,
        USER_TOP,
        if paging::SMEP.load(Ordering::Relaxed) { "on" } else { "off" },
        if smap { "on (lifted for user copies only)" } else { "off" },
    ));
}
