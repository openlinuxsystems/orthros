// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Thread contexts and the context switch.
//!
//! A context is a stack and a *context block*: one page holding the saved
//! stack pointer, an `on_cpu` word, and the extended-state save area
//! (`fpu.rs`). A new thread's stack and block are one area from the
//! mapping window: an unmapped guard page, the stack, and the block on top,
//! so an overflow faults on the guard instead of running into memory
//! another thread uses.
//!
//! A context may have a program's address space attached (Phase 5): the
//! switch then loads that space and the program's thread pointer (FS
//! base), and otherwise the kernel's own tables, so a space is only ever
//! loaded while a context that holds it runs. It also points this CPU's
//! system call and interrupt stack at the incoming context's kernel stack.
//!
//! The switch saves the callee-saved registers on the outgoing stack,
//! stores the stack pointer in the outgoing block, clears its `on_cpu`
//! word, then waits until it can set the incoming block's `on_cpu` word
//! from 0 to 1, loads that block's stack pointer and pops the incoming
//! thread's registers. Between letting go of the outgoing context and
//! taking the incoming one it touches no stack at all, so a thread that
//! another CPU picks up while this CPU is still switching away from it is
//! never run on two CPUs, and no two CPUs switching towards each other's
//! threads can wait for each other. The scheduler therefore need not hold
//! a lock across the switch (`docs/design/scheduler.md`).

use core::ops::Range;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use radix_sync::Once;

use crate::cpu::{self, msr};
use crate::fpu;
use crate::memory::PAGE;
use crate::paging;
use crate::percpu;
use crate::space::{AddressSpace, UserSpace};

/// Offsets in a context block.
const RSP: usize = 0;
const ON_CPU: usize = 8;
const AREA: usize = 64;

core::arch::global_asm!(
    r#"
    .section .text
    .global radix_context_switch
radix_context_switch:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    /* Let go of the outgoing context: from here on, no stack. */
    mov dword ptr [rdi + 8], 0
2:
    xor eax, eax
    mov ecx, 1
    lock cmpxchg dword ptr [rsi + 8], ecx
    jz 3f
    pause
    jmp 2b
3:
    mov rsp, [rsi]
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

    .global radix_thread_trampoline
radix_thread_trampoline:
    mov rdi, r12
    mov rsi, r13
    mov rdx, r14
    /* A zero return address ends backtraces here. */
    push 0
    xor ebp, ebp
    jmp radix_thread_start
    "#
);

unsafe extern "C" {
    /// Saves the running context in block `from` and continues the one in
    /// block `to`; see the module comment.
    fn radix_context_switch(from: usize, to: usize);
    static radix_thread_trampoline: u8;
}

/// Where a new context starts, from the trampoline: loads its initial
/// extended state and runs its entry point.
// Only our trampoline calls this, passing the Rust function pointer `new`
// stored, so its calling convention never crosses a foreign boundary.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
extern "C" fn radix_thread_start(entry: fn(usize) -> !, arg: usize, block: usize) -> ! {
    // SAFETY: `block` is this context's block (the trampoline got it from
    // the frame `new` built), whose save area `new` initialised; this is
    // the context's own first use of it.
    unsafe { fpu::restore(block + AREA) };
    entry(arg)
}

/// A thread's saved context.
pub struct Context {
    block: usize,
    stack: Range<usize>,
    /// What to unmap when the context is dropped: start and pages.
    mapping: (usize, usize),
    /// The program's address space, for a user thread.
    space: Once<UserSpace>,
    /// The program's thread pointer (FS base).
    thread_pointer: AtomicU64,
}

impl Context {
    fn on_cpu(&self) -> &AtomicU32 {
        // SAFETY: the block is a page mapped for as long as the context
        // exists (it is unmapped only in `drop`), `ON_CPU` is 4-aligned in
        // it, and the word is only ever accessed atomically (the switch
        // uses a locked compare-and-swap to take it and a plain aligned
        // store, which is atomic on x86-64, to let go of it).
        unsafe { &*((self.block + ON_CPU) as *const AtomicU32) }
    }

    /// A context of the code running now on this CPU, on its current
    /// kernel stack.
    pub fn adopt_current() -> Option<Context> {
        let block = paging::map_area(1).ok()?;
        // SAFETY: `block` is a fresh zeroed page that nothing else uses.
        unsafe { fpu::init_area(block + AREA) };
        let context = Context {
            block,
            stack: percpu::current().stacks.get().map_or(0..0, |s| s.kernel.clone()),
            mapping: (block, 1),
            space: Once::new(),
            thread_pointer: AtomicU64::new(0),
        };
        context.on_cpu().store(1, Ordering::Relaxed);
        let cpu = percpu::current();
        cpu.context.store(block, Ordering::Relaxed);
        cpu.thread_stack[0].store(context.stack.start, Ordering::Relaxed);
        cpu.thread_stack[1].store(context.stack.end, Ordering::Relaxed);
        Some(context)
    }

    /// A context that runs `entry(arg)` on a new stack of `pages` pages.
    pub fn new(entry: fn(usize) -> !, arg: usize, pages: usize) -> Option<Context> {
        let pages = pages.max(1);
        let area = paging::map_area(pages + 1).ok()?;
        let stack = area..area + pages * PAGE as usize;
        let block = stack.end;
        // SAFETY: the area was just mapped, zeroed and writable, and
        // nothing else knows of it: the block is its last page, and the
        // initial frame is the top 72 bytes of the stack below it.
        unsafe {
            fpu::init_area(block + AREA);
            // The frame the switch pops: r15, r14 (block), r13 (argument),
            // r12 (entry), rbx, rbp, then the return address. After the
            // `ret` the stack pointer is 16-byte aligned, and the
            // trampoline's push makes it look like a call to `entry`.
            let frame = (block - 9 * 8) as *mut usize;
            let values = [
                0,
                block,
                arg,
                entry as usize,
                0,
                0,
                &raw const radix_thread_trampoline as usize,
                0,
                0,
            ];
            for (slot, value) in values.into_iter().enumerate() {
                frame.add(slot).write(value);
            }
            ((block + RSP) as *mut usize).write(frame as usize);
        }
        Some(Context {
            block,
            stack,
            mapping: (area, pages + 1),
            space: Once::new(),
            thread_pointer: AtomicU64::new(0),
        })
    }

    /// Makes the context run in `space` from its next switch on.
    pub fn attach_space(&self, space: &UserSpace) -> bool {
        self.space.set(space.clone()).is_ok()
    }

    /// Sets the context's thread pointer, at once if it runs here.
    pub fn set_thread_pointer(&self, base: u64) {
        if base >= crate::user::USER_TOP {
            return;
        }
        let were = cpu::disable_interrupts();
        self.thread_pointer.store(base, Ordering::Relaxed);
        if percpu::current().context.load(Ordering::Relaxed) == self.block {
            // SAFETY: FS base is the program's; the kernel never uses FS,
            // and the value is a user address, so canonical.
            unsafe { cpu::wrmsr(msr::FS_BASE, base) };
        }
        if were {
            cpu::enable_interrupts();
        }
    }

    /// Switches from `from`, which must be running on this CPU, to `to`.
    pub fn switch(from: &Context, to: &Context) {
        assert!(!cpu::interrupts_enabled(), "context switch with interrupts on");
        let cpu = percpu::current();
        assert_eq!(
            cpu.context.load(Ordering::Relaxed),
            from.block,
            "switching away from a context that is not running on this CPU"
        );
        if from.block == to.block {
            return;
        }
        // SAFETY: `from` runs on this CPU (checked above), so its save area
        // is this thread's own.
        unsafe { fpu::save(from.block + AREA) };
        cpu.context.store(to.block, Ordering::Relaxed);
        cpu.thread_stack[0].store(to.stack.start, Ordering::Relaxed);
        cpu.thread_stack[1].store(to.stack.end, Ordering::Relaxed);
        cpu.set_kernel_stack(to.stack.end);
        match to.space.get() {
            Some(space) => {
                space.space().switch_to();
                // SAFETY: as in `set_thread_pointer`.
                unsafe {
                    cpu::wrmsr(msr::FS_BASE, to.thread_pointer.load(Ordering::Relaxed));
                }
            }
            None => AddressSpace::switch_to_kernel(),
        }
        // SAFETY: both blocks are mapped while their contexts exist, which
        // the references guarantee for the whole switch. `from` is the
        // context this CPU runs, so saving into its block is saving this
        // very thread. `to` holds a stack pointer saved by an earlier
        // switch or written by `new`, and the switch takes its `on_cpu`
        // word before using it, so no other CPU runs it meanwhile.
        // Interrupts are off, and NMIs and machine checks run on their own
        // stacks, so nothing uses the stack while no context owns it.
        unsafe { radix_context_switch(from.block, to.block) };
        // Back in `from`, possibly on another CPU.
        // SAFETY: `from` runs here again; its area holds what `save` wrote.
        unsafe { fpu::restore(from.block + AREA) };
    }

    /// Whether the context is on a CPU.
    pub fn is_running(&self) -> bool {
        self.on_cpu().load(Ordering::Acquire) != 0
    }

    /// The context's stack.
    pub fn stack(&self) -> Range<usize> {
        self.stack.clone()
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        if self.is_running() {
            radix_klog::warn!(
                "Threads: a context dropped while on a CPU; leaking its {} pages",
                self.mapping.1
            );
            return;
        }
        paging::unmap_area(self.mapping.0, self.mapping.1);
    }
}
