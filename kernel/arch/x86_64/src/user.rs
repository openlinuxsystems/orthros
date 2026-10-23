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
//! A call that must give a program back every register, RCX and R11
//! among them (returning from a signal handler, which may have interrupted
//! any instruction), asks for a full return instead: the stub then builds
//! an interrupt frame below the saved registers and leaves with `iretq`,
//! taking RCX and R11 from this CPU's data, where [`radix_syscall_dispatch`]
//! put them with interrupts masked.
//!
//! Interrupts and exceptions from user mode switch to the same kernel stack
//! through the TSS, and their entry stub swaps GS by looking at the saved
//! CS (`crate::interrupts`). An exception in user mode runs the user fault
//! handler, which resolves the fault (a page of anonymous memory made, or
//! copied for the thread that writes it) or raises a signal; the return
//! handler then runs, as after every system call and interrupt from user
//! mode, and delivers signals by writing a frame on the program's stack
//! ([`push_signal_frame`]). Each context's block records how its thread last
//! came in from user mode, so that the kernel finds its registers: a
//! [`SyscallFrame`] or a [`TrapFrame`] at the top of its kernel stack.
//!
//! The kernel reads and writes user memory only through
//! [`copy_from_user`] and [`copy_to_user`], which check that the range is
//! in the user half and lift SMAP for the one `rep movsb` that copies. A
//! fault on that instruction does not stop the kernel. If the copy may
//! sleep (interrupts were on, preemption allowed), the user page handler
//! gets the chance to make the page first, and the copy goes on where it
//! stopped; otherwise, or if it could not, the fault handler moves the
//! copy on to its end ([`fixup`]), and the copy reports how much was left.
//!
//! A signal frame has Linux's layout on x86-64, so that a C library's
//! `ucontext_t` and `siginfo_t` describe it (radix-signal(2)): at the
//! handler's stack pointer the address it returns to, above it the
//! `ucontext_t` (968 bytes, as glibc declares it, which musl's and mlibc's
//! begin with), the `siginfo_t` (128 bytes) and, 64-byte aligned, the
//! extended state as XSAVE (or FXSAVE) wrote it, which the context's
//! `fpregs` points at. The 128-byte red zone below the interrupted stack
//! pointer is left alone.

use core::fmt;
use core::sync::atomic::{AtomicU32, Ordering};

use radix_hal::{SignalFrame, SyscallRegs, UserFault, UserFaultKind};
use radix_sync::Once;

use crate::cpu::{self, msr};
use crate::fpu;
use crate::interrupts::TrapFrame;
use crate::memory::{self, PAGE};
use crate::paging;
use crate::percpu::{
    self, FULL_R11_OFFSET, FULL_RCX_OFFSET, KERNEL_CS, KERNEL_RSP_OFFSET, SCRATCH_OFFSET,
    USER_BASE, USER_CS, USER_DS,
};
use crate::space::USER_END;
use crate::thread::{self, UserEntry};

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

/// A program thread's registers, in the order of the general registers of
/// Linux's `mcontext_t` on x86-64 (`REG_R8` to `REG_EFL`), then its FS
/// base (the thread pointer).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UserRegisters {
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rsp: u64,
    pub rip: u64,
    pub rflags: u64,
    pub fs_base: u64,
}

/// How many of [`UserRegisters`]' words a context's general registers
/// hold: all but the FS base.
const GREGS: usize = 18;

const _: () = assert!(core::mem::size_of::<UserRegisters>() == (GREGS + 1) * 8);

impl UserRegisters {
    /// The general registers, in `mcontext_t`'s order.
    fn words(&self) -> [u64; GREGS] {
        [
            self.r8,
            self.r9,
            self.r10,
            self.r11,
            self.r12,
            self.r13,
            self.r14,
            self.r15,
            self.rdi,
            self.rsi,
            self.rbp,
            self.rbx,
            self.rdx,
            self.rax,
            self.rcx,
            self.rsp,
            self.rip,
            self.rflags,
        ]
    }

    /// Puts a system call's status and results where the call returns
    /// them.
    pub fn set_results(&mut self, results: &SyscallRegs) {
        let [rdi, rsi, rdx, r10, r8, r9, r12, r13, r14, r15] = results.args;
        self.rax = results.number;
        (self.rdi, self.rsi, self.rdx, self.r10, self.r8, self.r9) = (rdi, rsi, rdx, r10, r8, r9);
        (self.r12, self.r13, self.r14, self.r15) = (r12, r13, r14, r15);
    }
}

/// The parts of a signal frame: its `ucontext_t` and `siginfo_t`, at the
/// offsets glibc's x86-64 headers give them.
mod layout {
    /// `ucontext_t`.
    pub const CONTEXT: u64 = 968;
    /// `siginfo_t`, right after it.
    pub const INFO: u64 = 128;
    /// Both.
    pub const FRAME: usize = (CONTEXT + INFO) as usize;
    /// `uc_stack`: `ss_sp`, `ss_flags` (an `int`) and `ss_size`.
    pub const STACK: usize = 16;
    /// `uc_mcontext.gregs`, 23 words: the 18 general registers, then
    /// `REG_CSGSFS`, `REG_ERR`, `REG_TRAPNO`, `REG_OLDMASK` and `REG_CR2`.
    pub const GREGS: usize = 40;
    pub const CSGSFS: usize = GREGS + 18 * 8;
    pub const ERR: usize = GREGS + 19 * 8;
    pub const TRAPNO: usize = GREGS + 20 * 8;
    pub const OLDMASK: usize = GREGS + 21 * 8;
    pub const CR2: usize = GREGS + 22 * 8;
    /// `uc_mcontext.fpregs`: where the extended state is.
    pub const FPREGS: usize = 224;
    /// `uc_sigmask` (128 bytes, of which the kernel uses the first 8).
    pub const SIGMASK: usize = 296;
    /// What `sigreturn` reads: from the general registers to the mask.
    pub const RESTORED: core::ops::Range<usize> = GREGS..SIGMASK + 8;
}

/// The bytes below a program's stack pointer that a signal frame leaves
/// alone: the System V ABI's red zone, which leaf functions use without
/// moving the stack pointer.
const RED_ZONE: u64 = 128;

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
    test eax, eax
    jnz 1f
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

1:
    /* A full return: an interrupt frame below the saved registers (SS,
       RSP, RFLAGS, CS, RIP), then every register from them but RCX and
       R11, which come from this CPU's data. */
    push {user_ds}
    push qword ptr [rsp + 8 + 15 * 8]
    push qword ptr [rsp + 16 + 14 * 8]
    push {user_cs}
    push qword ptr [rsp + 32 + 13 * 8]
    mov rax, [rsp + 40]
    mov rdi, [rsp + 40 + 8]
    mov rsi, [rsp + 40 + 16]
    mov rdx, [rsp + 40 + 24]
    mov r10, [rsp + 40 + 32]
    mov r8, [rsp + 40 + 40]
    mov r9, [rsp + 40 + 48]
    mov r12, [rsp + 40 + 56]
    mov r13, [rsp + 40 + 64]
    mov r14, [rsp + 40 + 72]
    mov r15, [rsp + 40 + 80]
    mov rbx, [rsp + 40 + 88]
    mov rbp, [rsp + 40 + 96]
    mov rcx, gs:[{full_rcx}]
    mov r11, gs:[{full_r11}]
    swapgs
    iretq

    .global radix_enter_user_registers
radix_enter_user_registers:
    /* RDI points at a UserRegisters. */
    cli
    push {user_ds}
    push qword ptr [rdi + 15 * 8]
    push qword ptr [rdi + 17 * 8]
    push {user_cs}
    push qword ptr [rdi + 16 * 8]
    mov r8, [rdi]
    mov r9, [rdi + 8]
    mov r10, [rdi + 16]
    mov r11, [rdi + 24]
    mov r12, [rdi + 32]
    mov r13, [rdi + 40]
    mov r14, [rdi + 48]
    mov r15, [rdi + 56]
    mov rsi, [rdi + 72]
    mov rbp, [rdi + 80]
    mov rbx, [rdi + 88]
    mov rdx, [rdi + 96]
    mov rax, [rdi + 104]
    mov rcx, [rdi + 112]
    mov rdi, [rdi + 64]
    swapgs
    iretq

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
    full_rcx = const FULL_RCX_OFFSET,
    full_r11 = const FULL_R11_OFFSET,
    user_ds = const USER_DS,
    user_cs = const USER_CS,
    initial_flags = const FLAG_INTERRUPT | FLAG_RESERVED,
    smap = sym paging::SMAP,
);

unsafe extern "C" {
    /// The SYSCALL entry; see the assembly above.
    fn radix_syscall_entry();
    /// Leaves for user mode at `entry` with the stack at `stack` and `arg`
    /// in RDI.
    fn radix_enter_user(entry: u64, stack: u64, arg: u64) -> !;
    /// Leaves for user mode with every register from `regs`.
    fn radix_enter_user_registers(regs: *const UserRegisters) -> !;
    /// Copies `len` bytes from `src` to `dst`, one side of them user
    /// memory; returns how many were left when a fault stopped it.
    fn radix_user_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    static radix_user_copy_insn: u8;
    static radix_user_copy_done: u8;
}

static SYSCALL_HANDLER: Once<fn(&mut SyscallRegs)> = Once::new();
static FAULT_HANDLER: Once<fn(&UserFault)> = Once::new();
static PAGE_HANDLER: Once<fn(u64, bool) -> bool> = Once::new();
static RETURN_HANDLER: Once<fn()> = Once::new();

/// Registers the system call handler.
pub fn set_syscall_handler(handler: fn(&mut SyscallRegs)) {
    let _ = SYSCALL_HANDLER.set(handler);
}

/// Registers the user fault handler.
pub fn set_fault_handler(handler: fn(&UserFault)) {
    let _ = FAULT_HANDLER.set(handler);
}

/// Registers the user page handler.
pub fn set_page_handler(handler: fn(u64, bool) -> bool) {
    let _ = PAGE_HANDLER.set(handler);
}

/// Registers the user return handler.
pub fn set_return_handler(handler: fn()) {
    let _ = RETURN_HANDLER.set(handler);
}

/// Runs the user return handler, if there is one, and leaves interrupts
/// masked: on the way back to user mode.
pub fn on_return() {
    if let Some(handler) = RETURN_HANDLER.get() {
        handler();
    }
    cpu::mask_interrupts();
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

/// The flags a program gets back: its own of those it may hold, with
/// interrupts on.
fn user_flags(rflags: u64) -> u64 {
    (rflags & USER_FLAGS) | FLAG_INTERRUPT | FLAG_RESERVED
}

/// Called by the entry stub with the saved registers. Returns nonzero for
/// a full return, with RCX and R11 in this CPU's data.
///
/// The frame is only ever reached through raw pointers, apart from the
/// handler's borrow of its first eleven words for the length of the call:
/// the return handler reads and writes it too (`saved`), to deliver
/// signals.
#[unsafe(no_mangle)]
extern "C" fn radix_syscall_dispatch(frame: *mut SyscallFrame) -> u64 {
    thread::set_user_entry(UserEntry::Syscall);
    match SYSCALL_HANDLER.get() {
        // SAFETY: the stub passes the frame it has just saved at the top
        // of this thread's kernel stack, which nothing else uses while the
        // call runs; the borrow of its registers ends when the handler
        // returns.
        Some(handler) => handler(unsafe { &mut (*frame).regs }),
        // SAFETY: as above, for one write.
        None => unsafe { (*frame).regs.number = u64::MAX },
    }
    // Back to the program with interrupts masked from here to the return.
    cpu::mask_interrupts();
    on_return();
    // SAFETY: as above; no reference to the frame is left.
    unsafe {
        // The CPU's return address is at most the top of the user half
        // (after a `syscall` in its last two bytes, where the page above is
        // never mapped, so the program faults there), and the kernel only
        // ever writes ones below it, so SYSRET never sees a non-canonical
        // one, which would fault in kernel mode on Intel CPUs. Clamping
        // keeps that so whatever the kernel did.
        (*frame).rip = (*frame).rip.min(USER_TOP);
        (*frame).rflags = user_flags((*frame).rflags);
    }
    match thread::take_full_return() {
        Some((rcx, r11)) => {
            let cpu = percpu::current();
            cpu.full_rcx.store(rcx, Ordering::Relaxed);
            cpu.full_r11.store(r11, Ordering::Relaxed);
            1
        }
        None => 0,
    }
}

/// Where the running thread's user registers are saved: where its last
/// entry from user mode left them, at the top of its kernel stack.
#[derive(Clone, Copy)]
enum Saved {
    Syscall(*mut SyscallFrame),
    Trap(*mut TrapFrame),
}

/// The running thread's saved user registers, if it has been in user mode.
fn saved() -> Option<Saved> {
    let top = percpu::current().kernel_stack_top();
    match thread::user_entry() {
        UserEntry::Syscall => {
            Some(Saved::Syscall((top - core::mem::size_of::<SyscallFrame>() as u64) as *mut _))
        }
        UserEntry::Trap => {
            Some(Saved::Trap((top - core::mem::size_of::<TrapFrame>() as u64) as *mut _))
        }
        UserEntry::None => None,
    }
}

/// The running thread's user registers. For a system call, `results` says
/// whether to read the words that hold its results; while the call runs,
/// its handler borrows them, so only it may.
fn capture(results: bool) -> Option<UserRegisters> {
    // SAFETY: `saved` names the frame at the top of this thread's kernel
    // stack that its entry from user mode wrote, which stays there until
    // the thread returns to user mode; only the words the caller may read
    // are read (see above), through the raw pointer.
    let mut regs = unsafe {
        match saved()? {
            Saved::Syscall(frame) => {
                let mut regs = UserRegisters {
                    rbx: (*frame).rbx,
                    rbp: (*frame).rbp,
                    rip: (*frame).rip,
                    rflags: (*frame).rflags,
                    rsp: (*frame).rsp,
                    ..UserRegisters::default()
                };
                (regs.rcx, regs.r11) = thread::full_return().unwrap_or((regs.rip, regs.rflags));
                if results {
                    let words = (*frame).regs;
                    regs.set_results(&words);
                }
                regs
            }
            Saved::Trap(frame) => {
                let f = *frame;
                UserRegisters {
                    r8: f.r8,
                    r9: f.r9,
                    r10: f.r10,
                    r11: f.r11,
                    r12: f.r12,
                    r13: f.r13,
                    r14: f.r14,
                    r15: f.r15,
                    rdi: f.rdi,
                    rsi: f.rsi,
                    rbp: f.rbp,
                    rbx: f.rbx,
                    rdx: f.rdx,
                    rax: f.rax,
                    rcx: f.rcx,
                    rsp: f.rsp,
                    rip: f.rip,
                    rflags: f.rflags,
                    fs_base: 0,
                }
            }
        }
    };
    // SAFETY: reading FS base changes nothing; it is the running thread's.
    regs.fs_base = unsafe { cpu::rdmsr(msr::FS_BASE) };
    Some(regs)
}

/// The running thread's user registers; see the HAL.
pub fn user_registers() -> Option<UserRegisters> {
    capture(false)
}

/// The running thread's stack pointer in user mode, or 0 before it has
/// been there.
pub fn user_stack_pointer() -> u64 {
    // SAFETY: as in `capture`; the stack pointer is in the frame's tail,
    // which no handler borrows.
    unsafe {
        match saved() {
            Some(Saved::Syscall(frame)) => (*frame).rsp,
            Some(Saved::Trap(frame)) => (*frame).rsp,
            None => 0,
        }
    }
}

/// Leaves the kernel for user mode with every register from `regs`; FS
/// base stays the context's thread pointer.
pub fn enter_user_registers(regs: &UserRegisters) -> ! {
    let cpu = percpu::current();
    assert!(cpu.space.load(Ordering::Relaxed) != 0, "entering user mode without an address space");
    let mut regs = *regs;
    regs.rip = regs.rip.min(USER_TOP);
    regs.rflags = user_flags(regs.rflags);
    // SAFETY: the running context has an address space loaded (checked
    // above), the return address is a canonical user one and the flags
    // are ones a program may hold, so `iretq` cannot fault; the stub reads
    // `regs`, on this stack above where it pushes, before it leaves. The
    // kernel stack is abandoned, as in `enter_user`.
    unsafe { radix_enter_user_registers(&regs) }
}

/// Makes the running thread's system call run again when it returns to
/// user mode: its registers as they were, and its return address back on
/// the `syscall` instruction (two bytes long). Called on the way back.
pub fn restart_syscall(original: &SyscallRegs) {
    if let Some(Saved::Syscall(frame)) = saved() {
        // SAFETY: as in `capture`; the handler has returned, so nothing
        // borrows the frame.
        unsafe {
            (*frame).regs = *original;
            (*frame).rip -= 2;
        }
    }
}

/// Writes a signal handler's frame on the running thread's stack; see the
/// HAL. Called on the way back to user mode, with interrupts on, so that
/// writing the frame may make the stack's pages.
pub fn push_signal_frame(signal: &SignalFrame<'_>) -> bool {
    let Some(regs) = capture(true) else { return false };
    if signal.handler >= USER_TOP || signal.restorer >= USER_TOP {
        return false;
    }
    let size = fpu::image_size() as u64;
    let top = match signal.stack {
        Some((base, len)) => base.checked_add(len),
        None => regs.rsp.checked_sub(RED_ZONE),
    };
    let Some(fpstate) = top.and_then(|top| top.checked_sub(size)).map(|at| at & !63) else {
        return false;
    };
    let Some(context) = fpstate.checked_sub(layout::FRAME as u64).map(|at| at & !15) else {
        return false;
    };
    let Some(base) = context.checked_sub(8) else { return false };

    let mut bytes = [0u8; layout::FRAME];
    let mut put = |at: usize, word: u64| bytes[at..at + 8].copy_from_slice(&word.to_le_bytes());
    let (sp, flags, len) = signal.altstack;
    put(layout::STACK, sp);
    put(layout::STACK + 8, u64::from(flags));
    put(layout::STACK + 16, len);
    for (i, word) in regs.words().into_iter().enumerate() {
        put(layout::GREGS + 8 * i, word);
    }
    put(layout::CSGSFS, u64::from(USER_CS) | (u64::from(USER_DS) << 48));
    if let Some((vector, error, address)) = signal.trap {
        put(layout::ERR, error);
        put(layout::TRAPNO, vector);
        put(layout::CR2, address);
    }
    put(layout::OLDMASK, signal.mask);
    put(layout::FPREGS, fpstate);
    put(layout::SIGMASK, signal.mask);
    bytes[layout::CONTEXT as usize..].copy_from_slice(signal.info);

    let area = thread::current_area();
    let were = cpu::disable_interrupts();
    // SAFETY: the area is the running context's own, and with interrupts
    // masked no switch saves into it meanwhile.
    unsafe { fpu::save(area) };
    if were {
        cpu::enable_interrupts();
    }
    // SAFETY: the area is mapped for as long as the context lives and
    // holds at least `size` bytes; until the thread runs user code again
    // its extended state stays as just saved, so a switch meanwhile writes
    // the same image again.
    let image = unsafe { core::slice::from_raw_parts(area as *const u8, size as usize) };
    if !copy_to_user(base, &signal.restorer.to_le_bytes())
        || !copy_to_user(context, &bytes)
        || !copy_to_user(fpstate, image)
    {
        return false;
    }

    cpu::disable_interrupts();
    // SAFETY: as above, with interrupts masked: the handler starts with
    // the initial extended state, as Linux's do.
    unsafe { fpu::load_initial(area) };
    // Its registers and RCX and R11 are in the frame now.
    let _ = thread::take_full_return();
    let entry =
        (signal.handler, base, u64::from(signal.signal), context + layout::CONTEXT, context);
    // SAFETY: as in `restart_syscall`.
    unsafe {
        match saved() {
            Some(Saved::Syscall(frame)) => {
                let (rip, rsp, rdi, rsi, rdx) = entry;
                (*frame).rip = rip;
                (*frame).rsp = rsp;
                (*frame).rflags &= !(FLAG_DIRECTION | FLAG_TRAP);
                (*frame).regs.number = 0;
                (*frame).regs.args[0] = rdi;
                (*frame).regs.args[1] = rsi;
                (*frame).regs.args[2] = rdx;
            }
            Some(Saved::Trap(frame)) => {
                (*frame).rip = entry.0;
                (*frame).rsp = entry.1;
                (*frame).rflags = user_flags((*frame).rflags) & !(FLAG_DIRECTION | FLAG_TRAP);
                (*frame).rdi = entry.2;
                (*frame).rsi = entry.3;
                (*frame).rdx = entry.4;
                (*frame).rax = 0;
            }
            None => {}
        }
    }
    if were {
        cpu::enable_interrupts();
    }
    true
}

/// Restores what a signal frame saved; see the HAL. Called by the system
/// call that returns from a signal handler.
pub fn pop_signal_frame(context: u64, regs: &mut SyscallRegs) -> Option<u64> {
    let Some(Saved::Syscall(frame)) = saved() else { return None };
    let mut head = [0u8; layout::RESTORED.end - layout::RESTORED.start];
    if !copy_from_user(&mut head, context.checked_add(layout::RESTORED.start as u64)?) {
        return None;
    }
    let word = |at: usize| {
        let at = at - layout::RESTORED.start;
        u64::from_le_bytes(head[at..at + 8].try_into().unwrap())
    };
    let greg = |i: usize| word(layout::GREGS + 8 * i);
    // r8 r9 r10 r11 r12 r13 r14 r15 rdi rsi rbp rbx rdx rax rcx rsp rip eflags
    let (rsp, rip) = (greg(15), greg(16));
    if rip >= USER_TOP || rsp > USER_TOP {
        return None;
    }
    let fpstate = word(layout::FPREGS);
    let size = fpu::image_size();
    if fpstate != 0 && (!fpstate.is_multiple_of(64) || !user_range(fpstate, size)) {
        return None;
    }
    let mask = word(layout::SIGMASK);

    let area = thread::current_area();
    let were = cpu::disable_interrupts();
    // SAFETY: the running context's own area, with interrupts masked so
    // that no switch saves into it meanwhile; the program's image is made
    // safe to load before it is loaded, and if it cannot be read the area
    // gets the thread's live state back.
    let restored = unsafe {
        if fpstate == 0 {
            fpu::load_initial(area);
            true
        } else {
            let image = core::slice::from_raw_parts_mut(area as *mut u8, size);
            // Interrupts are masked, so a page that is not there is not
            // made: the frame's pages were all written when it was pushed.
            if copy_from_user(image, fpstate) {
                fpu::sanitize(image);
                fpu::restore(area);
                true
            } else {
                fpu::save(area);
                false
            }
        }
    };
    if were {
        cpu::enable_interrupts();
    }
    if !restored {
        return None;
    }

    regs.number = greg(13);
    regs.args =
        [greg(8), greg(9), greg(12), greg(2), greg(0), greg(1), greg(4), greg(5), greg(6), greg(7)];
    // SAFETY: as in `capture`: the tail of the frame, which the handler's
    // borrow (`regs`) does not cover.
    unsafe {
        (*frame).rbx = greg(11);
        (*frame).rbp = greg(10);
        (*frame).rsp = rsp;
        (*frame).rip = rip;
        (*frame).rflags = greg(17);
    }
    thread::request_full_return(greg(14), greg(3));
    Some(mask)
}

/// Runs the user fault handler for a fault in user mode.
fn fault(fault: UserFault) {
    match FAULT_HANDLER.get() {
        Some(handler) => handler(&fault),
        None => panic!("User mode: {fault}, and no kernel handler for it"),
    }
}

/// Handles exception `vector` from user mode: the fault handler resolves
/// it or raises a signal, which the return handler then delivers.
pub fn exception(vector: u8, frame: &TrapFrame) {
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
    fault(UserFault { kind, pc: frame.rip, sp: frame.rsp, code: frame.error });
}

/// Where a fault in kernel mode at `rip` resumes, if it is the copy
/// instruction of a user copy: for a page fault on a user address (`page`:
/// the address and whether it was a write) that the copy may sleep for,
/// the copy instruction again once the user page handler has made the
/// page; otherwise the copy's end, so that it reports what was left.
pub fn fixup(rip: u64, page: Option<(u64, bool)>, may_sleep: bool) -> Option<u64> {
    let insn = &raw const radix_user_copy_insn as u64;
    if rip != insn {
        return None;
    }
    if let Some((address, write)) = page {
        if address >= USER_TOP {
            return None;
        }
        if may_sleep && let Some(handler) = PAGE_HANDLER.get() {
            cpu::enable_interrupts();
            let made = handler(address, write);
            cpu::disable_interrupts();
            if made {
                return Some(insn);
            }
        }
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

/// Copies the page at `src` to the one at `dst`, both RAM.
pub fn copy_frame(dst: u64, src: u64) {
    assert!(
        memory::is_ram(src..src + PAGE) && memory::is_ram(dst..dst + PAGE) && src != dst,
        "copying {src:#x} to {dst:#x}: not two pages of RAM"
    );
    // SAFETY: both are distinct pages of RAM (checked above), which the
    // direct map covers; the caller owns `dst`, and nothing writes `src`
    // meanwhile (a shared page is only ever read).
    unsafe {
        core::ptr::copy_nonoverlapping(
            memory::phys_to_virt(src) as *const u8,
            memory::phys_to_virt(dst) as *mut u8,
            PAGE as usize,
        )
    };
}

/// Copies RAM at `phys` into `dst`.
pub fn read_frames(phys: u64, dst: &mut [u8]) {
    let end = phys + dst.len() as u64;
    assert!(memory::is_ram(phys..end), "reading {phys:#x}: not RAM");
    // SAFETY: as in `zero_frames`, reading.
    unsafe {
        core::ptr::copy_nonoverlapping(
            memory::phys_to_virt(phys) as *const u8,
            dst.as_mut_ptr(),
            dst.len(),
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
