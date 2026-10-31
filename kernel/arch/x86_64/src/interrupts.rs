// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The interrupt descriptor table, exception handling and interrupt
//! dispatch.
//!
//! All 256 vectors enter through small assembly stubs that push the vector
//! number (and a zero where the CPU pushes no error code), save every
//! general-purpose register into a [`TrapFrame`] and call [`dispatch`]. The
//! common stub also pushes a fake stack frame record made of the
//! interrupted RIP and RBP, so a frame-pointer backtrace taken inside a
//! handler continues into the interrupted code.
//!
//! Exceptions in the kernel are fatal except breakpoints, which call the
//! registered breakpoint handler and resume, NMIs, which the panic path
//! uses to stop the other CPUs, and machine checks the CPU can go on after
//! (`mce`). A machine check in user mode that the program must not go on
//! after moves to the thread's stack and ends only the program
//! (`crate::user::machine_check`). A fatal exception records a [`Fault`]
//! for the panic report and panics with a description of what went wrong.
//!
//! Interrupts (vectors 32 and up) count the CPU's interrupt depth. On the
//! way out of one that came in from thread context with interrupts on and
//! preemption allowed, or from user mode, the registered preemption handler
//! runs, still with interrupts masked: that is where the scheduler switches
//! threads when a tick or a reschedule IPI asked for it.
//!
//! From user mode (Phase 5) the CPU switches to the running thread's kernel
//! stack (the TSS's RSP0), and the common stub swaps GS base to this CPU's
//! data, and back on the way out, when the saved CS says the interrupted
//! code ran in user mode. NMIs, double faults and machine checks can also
//! arrive in kernel mode before the SYSCALL entry has swapped GS, or after
//! the exit has swapped it back, and an `iretq` to user mode that faults
//! does so in kernel mode after the swap, so for an exception in kernel
//! mode (rare, unlike interrupts) the stub reads GS base itself and swaps
//! only if it is not a kernel address. It then marks the frame, so that a
//! fatal report says so, instead of faulting again on the first access to
//! this CPU's data, and again, until the stack runs out. With SMAP on, the
//! stub clears RFLAGS.AC, so a handler never runs with user pages open (the
//! interrupted code gets its flags back with `iretq`). Exceptions in user
//! mode go to the user fault handler (`crate::user`), and every return to
//! user mode but an NMI's (which runs on its own stack) runs the user
//! return handler, which delivers signals. A page fault on the copy
//! instruction of a user copy lets the user page handler make the page,
//! or moves on to the end of the copy instead.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use radix_hal::Fault;
use radix_klog::TextBuf;
use radix_sync::{Once, TakeOnce};

use crate::cpu;
use crate::percpu::{self, IST_DOUBLE_FAULT, IST_MACHINE_CHECK, IST_NMI, KERNEL_CS};

/// Vector of the local APIC timer.
pub const VECTOR_TIMER: u8 = 0x30;
/// Vector of the ping IPI.
pub const VECTOR_PING: u8 = 0x40;
/// Vector of the TLB flush IPI.
pub const VECTOR_TLB: u8 = 0x41;
/// Vector of the reschedule IPI.
pub const VECTOR_RESCHEDULE: u8 = 0x42;
/// First vector the legacy PICs are remapped to (they stay masked).
pub const VECTOR_PIC_BASE: u8 = 0x20;
/// Vector of the interrupts for corrected machine-check errors: Intel's
/// CMCI and AMD's thresholds (ADR 0169), Linux's vector for both.
pub const VECTOR_CORRECTED_ERROR: u8 = 0xf9;
/// Vector of local APIC errors.
pub const VECTOR_APIC_ERROR: u8 = 0xfe;
/// The local APIC's spurious-interrupt vector.
pub const VECTOR_SPURIOUS: u8 = 0xff;

/// Registers saved by the entry stubs, lowest address first.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// The stubs. Each is 16 bytes long, so the stub for vector v is at
// `isr_stubs + 16 * v`. Vectors 8, 10-14, 17, 21, 29 and 30 come with an
// error code from the CPU; the others push a zero in its place.
core::arch::global_asm!(
    r#"
    .section .text.isr, "ax"
    .balign 16
    .global radix_isr_stubs
radix_isr_stubs:
    .set vector, 0
    .rept 256
    .balign 16
    .if (vector == 8) || (vector >= 10 && vector <= 14) || (vector == 17) || (vector == 21) || (vector == 29) || (vector == 30)
    .else
    pushq $0
    .endif
    pushq $vector
    jmp radix_isr_common
    .set vector, vector + 1
    .endr

radix_isr_common:
    cld
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rbp
    pushq %rsi
    pushq %rdi
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    /* RBX (saved above, callee-saved across the call) says whether GS
       base was swapped on the way in. */
    xorl %ebx, %ebx
    testb $3, 18*8(%rsp)
    jz 1f
    swapgs
    movl $1, %ebx
    jmp 3f
1:
    /* Kernel mode: an interrupt finds the kernel's GS base, but an
       exception may not, so it reads GS base, which is the kernel's when
       its top bit is set. */
    cmpq $32, 15*8(%rsp)
    jae 3f
    movl $0xc0000101, %ecx
    rdmsr
    testl %edx, %edx
    js 3f
    swapgs
    movl $1, %ebx
    orq ${user_gs}, 15*8(%rsp)
3:
    cmpb $0, {smap}(%rip)
    je 4f
    clac
4:
    /* Fake frame record: interrupted RIP above interrupted RBP. */
    pushq 17*8(%rsp)
    pushq %rbp
    movq %rsp, %rbp
    leaq 16(%rsp), %rdi
    call radix_trap_dispatch
radix_isr_return:
    addq $16, %rsp
    testl %ebx, %ebx
    jz 5f
    swapgs
5:
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rdi
    popq %rsi
    popq %rbp
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax
    addq $16, %rsp
    iretq

    /* A machine check in user mode that ends the program
       (`user::machine_check`): RDI is its frame, moved from the
       machine-check stack to the top of the thread's kernel stack, where
       an exception from user mode leaves its frame, and RSI the bad page.
       It goes on there as the stub above does, GS base swapped. */
    .global radix_isr_user_machine_check
radix_isr_user_machine_check:
    movq %rdi, %rsp
    movl $1, %ebx
    pushq 17*8(%rsp)
    pushq 11*8(%rsp)
    movq %rsp, %rbp
    leaq 16(%rsp), %rdi
    call radix_trap_user_machine_check
    jmp radix_isr_return
    "#,
    smap = sym crate::paging::SMAP,
    user_gs = const USER_GS_IN_KERNEL,
    options(att_syntax)
);

/// Set in a frame's vector word by the entry stub: the exception came in
/// kernel mode while GS base was the program's.
const USER_GS_IN_KERNEL: u64 = 1 << 8;

unsafe extern "C" {
    /// The first entry stub; see the assembly above.
    static radix_isr_stubs: u8;
}

/// One IDT gate.
#[repr(C)]
#[derive(Clone, Copy)]
struct Gate {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_middle: u16,
    offset_high: u32,
    _reserved: u32,
}

static IDT: TakeOnce<[Gate; 256]> = TakeOnce::new(
    [Gate {
        offset_low: 0,
        selector: 0,
        ist: 0,
        attributes: 0,
        offset_middle: 0,
        offset_high: 0,
        _reserved: 0,
    }; 256],
);

/// Address of the IDT once built.
static IDT_ADDRESS: Once<usize> = Once::new();

/// Builds the IDT. Called once, by the boot CPU.
pub fn init() {
    let Some(idt) = IDT.take() else { return };
    let stubs = &raw const radix_isr_stubs as usize;
    for (vector, gate) in idt.iter_mut().enumerate() {
        let handler = (stubs + 16 * vector) as u64;
        let ist = match vector {
            2 => IST_NMI,
            8 => IST_DOUBLE_FAULT,
            18 => IST_MACHINE_CHECK,
            _ => 0,
        };
        // Present, 64-bit interrupt gate (interrupts masked on entry),
        // which only ring 0 may raise with `int`, except the breakpoint:
        // a program's `int3` is a breakpoint, not a protection fault.
        let attributes = if vector == 3 { 0xee } else { 0x8e };
        *gate = Gate {
            offset_low: handler as u16,
            selector: KERNEL_CS,
            ist,
            attributes,
            offset_middle: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            _reserved: 0,
        };
    }
    let _ = IDT_ADDRESS.set(idt.as_ptr() as usize);
}

/// Loads the IDT on this CPU.
pub fn load() {
    let Some(&base) = IDT_ADDRESS.get() else { return };
    #[repr(C, packed)]
    struct Pointer {
        limit: u16,
        base: u64,
    }
    let pointer = Pointer { limit: (256 * 16 - 1) as u16, base: base as u64 };
    // SAFETY: the IDT is a fully initialised static that is never written
    // again, and every gate points at an entry stub.
    unsafe {
        core::arch::asm!("lidt [{}]", in(reg) &raw const pointer, options(readonly, nostack, preserves_flags));
    };
}

/// Names of the exceptions, by vector.
const EXCEPTIONS: [(&str, &str); 32] = [
    ("Divide error", "#DE"),
    ("Debug exception", "#DB"),
    ("Non-maskable interrupt", "NMI"),
    ("Breakpoint", "#BP"),
    ("Overflow", "#OF"),
    ("BOUND range exceeded", "#BR"),
    ("Invalid opcode", "#UD"),
    ("Device not available", "#NM"),
    ("Double fault", "#DF"),
    ("Coprocessor segment overrun", "#9"),
    ("Invalid TSS", "#TS"),
    ("Segment not present", "#NP"),
    ("Stack-segment fault", "#SS"),
    ("General protection fault", "#GP"),
    ("Page fault", "#PF"),
    ("Reserved exception 15", "#15"),
    ("x87 floating-point error", "#MF"),
    ("Alignment check", "#AC"),
    ("Machine check", "#MC"),
    ("SIMD floating-point exception", "#XM"),
    ("Virtualization exception", "#VE"),
    ("Control protection exception", "#CP"),
    ("Reserved exception 22", "#22"),
    ("Reserved exception 23", "#23"),
    ("Reserved exception 24", "#24"),
    ("Reserved exception 25", "#25"),
    ("Reserved exception 26", "#26"),
    ("Reserved exception 27", "#27"),
    ("Hypervisor injection exception", "#HV"),
    ("VMM communication exception", "#VC"),
    ("Security exception", "#SX"),
    ("Reserved exception 31", "#31"),
];

/// The function breakpoints call.
static BREAKPOINT_HANDLER: Once<fn(&Fault)> = Once::new();

/// Registers the breakpoint handler.
pub fn set_breakpoint_handler(handler: fn(&Fault)) {
    let _ = BREAKPOINT_HANDLER.set(handler);
}

/// The function called on the way back to a preemptible thread.
static PREEMPT_HANDLER: Once<fn()> = Once::new();

/// Registers the preemption handler.
pub fn set_preempt_handler(handler: fn()) {
    let _ = PREEMPT_HANDLER.set(handler);
}

/// RFLAGS.IF.
const INTERRUPT_FLAG: u64 = 1 << 9;

/// Set while the panic path stops the other CPUs; NMIs then halt.
pub static STOPPING: AtomicBool = AtomicBool::new(false);
/// CPUs that have stopped.
pub static STOPPED: AtomicUsize = AtomicUsize::new(0);

/// Converts saved registers into the HAL's fault record.
pub fn fault_from(frame: &TrapFrame) -> Fault {
    let regs = [
        ("rax", frame.rax),
        ("rbx", frame.rbx),
        ("rcx", frame.rcx),
        ("rdx", frame.rdx),
        ("rsi", frame.rsi),
        ("rdi", frame.rdi),
        ("rbp", frame.rbp),
        ("rsp", frame.rsp),
        ("r8", frame.r8),
        ("r9", frame.r9),
        ("r10", frame.r10),
        ("r11", frame.r11),
        ("r12", frame.r12),
        ("r13", frame.r13),
        ("r14", frame.r14),
        ("r15", frame.r15),
        ("rip", frame.rip),
        ("rflags", frame.rflags),
        ("cs", frame.cs),
        ("ss", frame.ss),
        ("cr2", cpu::cr2()),
        ("cr3", cpu::cr3()),
        ("cr4", cpu::cr4()),
        ("error", frame.error),
    ];
    Fault {
        pc: frame.rip as usize,
        frame_pointer: frame.rbp as usize,
        stack_pointer: frame.rsp as usize,
        registers: regs,
        register_count: regs.len(),
    }
}

/// Describes a page fault's error code.
fn page_fault_detail(out: &mut TextBuf<256>, error: u64, address: u64) {
    let access = if error & (1 << 4) != 0 {
        "executing"
    } else if error & (1 << 1) != 0 {
        "writing"
    } else {
        "reading"
    };
    let cause = if error & 1 != 0 {
        "the page does not allow this access"
    } else {
        "the page is not mapped"
    };
    let _ = write!(out, " while {access} {address:#x}: {cause}");
    if error & (1 << 3) != 0 {
        let _ = write!(out, " (reserved bit set in a page-table entry)");
    }
    // A protection fault on a lower-half (user) page in kernel mode.
    if error & 1 != 0 && error & (1 << 2) == 0 && address < crate::space::USER_END {
        if error & (1 << 4) != 0 && crate::paging::SMEP.load(Ordering::Relaxed) {
            let _ = write!(out, "; SMEP stopped the kernel from running code in a user page");
        } else if crate::paging::SMAP.load(Ordering::Relaxed) {
            let _ = write!(out, "; SMAP stopped the kernel from touching a user page");
        }
    }
    if percpu::current().in_stack_guard(address as usize) {
        let _ = write!(out, "; this is a stack guard page, so the kernel stack overflowed");
    }
}

/// Called by the entry stubs with the saved registers.
///
/// From user mode the frame is at the top of the thread's kernel stack,
/// where the return handler reads and writes it too, to deliver signals,
/// so it is only borrowed for as long as each step needs it.
#[unsafe(no_mangle)]
extern "C" fn radix_trap_dispatch(frame: *mut TrapFrame) {
    // SAFETY: the stub passes the frame it has just saved on this stack,
    // which nothing else uses until this returns but the steps below.
    let (vector, cs, rflags) = unsafe { ((*frame).vector as u8, (*frame).cs, (*frame).rflags) };
    let interrupt = vector >= 32;
    // NMIs, double faults and machine checks run on stacks of their own.
    let own_stack = matches!(vector, 2 | 8 | 18);
    let from_user = cs & 3 != 0 && !own_stack;
    if from_user {
        crate::thread::set_user_entry(crate::thread::UserEntry::Trap);
        if !interrupt {
            // SAFETY: as above, borrowed for the call.
            crate::user::exception(vector, unsafe { &*frame });
            crate::user::on_return();
            return;
        }
    }
    if interrupt {
        // For the timer's sample of where the CPU's time goes. Handlers
        // run with interrupts masked, so no other interrupt comes between.
        percpu::current().interrupted_user.store(from_user, Ordering::Relaxed);
        percpu::interrupt_enter();
    }
    // SAFETY: as above.
    handle(vector, unsafe { &mut *frame });
    if interrupt
        && percpu::interrupt_exit() == 0
        && (from_user || (rflags & INTERRUPT_FLAG != 0 && percpu::preempt_count() == 0))
        && let Some(handler) = PREEMPT_HANDLER.get()
    {
        handler();
    }
    if from_user {
        crate::user::on_return();
    }
}

fn handle(vector: u8, frame: &mut TrapFrame) {
    match vector {
        VECTOR_TIMER => crate::time::on_tick(),
        VECTOR_RESCHEDULE => {
            percpu::current().reschedules.fetch_add(1, Ordering::Relaxed);
            crate::apic::end_of_interrupt();
        }
        VECTOR_PING => {
            percpu::current().pings.fetch_add(1, Ordering::Relaxed);
            radix_klog::tracepoint!("ipi.ping", percpu::current_index());
            crate::apic::end_of_interrupt();
        }
        VECTOR_TLB => {
            // End the interrupt first, as Linux's IPI handlers do: once
            // the flush is answered, the initiator may send the next
            // request, and with it pending at the EOI the hypervisor
            // cannot offer paravirtual EOI, so the EOI would leave the
            // virtual machine.
            crate::apic::end_of_interrupt();
            crate::paging::serve_shootdown();
        }
        VECTOR_CORRECTED_ERROR => {
            crate::mce::on_corrected_interrupt();
            crate::apic::end_of_interrupt();
        }
        VECTOR_APIC_ERROR => {
            let status = crate::apic::read_error_status();
            radix_klog::tracepoint!("apic.error", status);
            crate::apic::end_of_interrupt();
        }
        // Spurious interrupts from the local APIC or the masked PICs need
        // no end-of-interrupt.
        VECTOR_SPURIOUS => {}
        v if (crate::devices::FIRST..=crate::devices::LAST).contains(&v) => {
            crate::devices::dispatch(v);
        }
        v if (VECTOR_PIC_BASE..VECTOR_PIC_BASE + 16).contains(&v) => {
            radix_klog::tracepoint!("pic.spurious", v);
        }
        2 => nmi(frame),
        3 => breakpoint(frame),
        13 | 14 => match msr_fixup(vector, frame).or_else(|| copy_fixup(vector, frame)) {
            Some(resume) => frame.rip = resume,
            None => fatal_exception(frame),
        },
        18 => match crate::mce::triage(frame.cs & 3 != 0) {
            radix_mca::Verdict::GoOn => {}
            radix_mca::Verdict::EndProgram(page) => crate::user::machine_check(frame, page),
            radix_mca::Verdict::Fatal => fatal_exception(frame),
        },
        0..32 => fatal_exception(frame),
        v => {
            radix_klog::tracepoint!("irq.unexpected", v);
            crate::apic::end_of_interrupt();
        }
    }
}

/// Where a protection fault resumes, if it was a read of an MSR that may
/// not exist (`cpu::rdmsr_safe`).
fn msr_fixup(vector: u8, frame: &TrapFrame) -> Option<u64> {
    if vector == 13 { cpu::rdmsr_fixup(frame.rip) } else { None }
}

/// Where a kernel-mode page or protection fault resumes, if it hit a user
/// copy's copy instruction (on a user address, for a page fault): the
/// instruction again if the page was made, which the copy may sleep for
/// when it ran with interrupts on and preemption allowed.
fn copy_fixup(vector: u8, frame: &TrapFrame) -> Option<u64> {
    let page = (vector == 14).then(|| (cpu::cr2(), frame.error & (1 << 1) != 0));
    let may_sleep = frame.rflags & INTERRUPT_FLAG != 0 && percpu::preempt_count() == 0;
    crate::user::fixup(frame.rip, page, may_sleep)
}

fn nmi(frame: &TrapFrame) {
    if STOPPING.load(Ordering::Acquire) {
        let cpu = percpu::current();
        // Where it was, for the panic report: a CPU that did not answer a
        // request may be stuck there.
        let mut trace = [0; percpu::STOPPED_FRAMES];
        trace[0] = frame.rip as usize;
        let len =
            1 + <crate::Arch as radix_hal::Cpu>::backtrace(frame.rbp as usize, &mut trace[1..]);
        for (slot, &address) in cpu.stopped_at.iter().zip(&trace[..len]) {
            slot.store(address, Ordering::Relaxed);
        }
        cpu.online.store(false, Ordering::Relaxed);
        cpu.stopped.store(true, Ordering::Release);
        STOPPED.fetch_add(1, Ordering::AcqRel);
        cpu::halt_forever();
    }
    // Logging could deadlock on a lock this CPU holds, so only record it.
    radix_klog::tracepoint!("nmi.unexpected", frame.rip);
}

fn breakpoint(frame: &mut TrapFrame) {
    // RIP already points after the `int3`.
    let fault = fault_from(frame);
    match BREAKPOINT_HANDLER.get() {
        Some(handler) => handler(&fault),
        None => radix_klog::info!("Breakpoint at {:#x} (no debugger; continuing)", frame.rip - 1),
    }
}

fn fatal_exception(frame: &TrapFrame) -> ! {
    let cpu = percpu::current();
    let vector = (frame.vector & 0xff) as usize;
    let (name, mnemonic) = EXCEPTIONS[vector];
    if cpu.exception_depth.fetch_add(1, Ordering::AcqRel) > 2 {
        // Exceptions inside the handling of exceptions: give up quietly
        // rather than recurse until the stack is gone.
        cpu::halt_forever();
    }
    *cpu.fault.lock() = Some(fault_from(frame));
    let mut detail = TextBuf::<256>::new();
    match vector {
        14 => page_fault_detail(&mut detail, frame.error, cpu::cr2()),
        10..=13 if frame.error != 0 => {
            let _ = write!(detail, " (selector error code {:#x})", frame.error);
        }
        8 if cpu.in_stack_guard(cpu::cr2() as usize) => {
            let _ = write!(
                detail,
                " after touching {:#x}, a stack guard page: the kernel stack overflowed",
                cpu::cr2()
            );
        }
        8 => {
            let _ = write!(
                detail,
                ", probably after a kernel stack overflow or a fault in a fault handler"
            );
        }
        18 => crate::mce::describe(&mut detail),
        _ => {}
    }
    if frame.cs & 3 != 0 {
        let _ = write!(detail, " [user mode]");
    }
    if frame.vector & USER_GS_IN_KERNEL != 0 {
        let _ = write!(detail, " [kernel mode with the program's GS base]");
    }
    panic!("{name} ({mnemonic}) at {:#x}{detail}", frame.rip);
}
