// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! User mode (Phase 5): address spaces for programs, entering user mode,
//! system calls, faults in user mode, and copying between the kernel and
//! user memory.
//!
//! The kernel decides what a program may do; this layer only provides the
//! mechanism. A program's thread is a kernel thread whose context has a
//! user address space attached ([`UserMode::attach_space`]): switching to
//! it switches to that space, and the thread enters user mode once, with
//! [`UserMode::enter_user`]. From then on it comes back into the kernel
//! only through a system call, which runs the registered handler on the
//! thread's kernel stack with the call's registers, or through an
//! interrupt or a fault. A fault in user mode runs the user fault handler
//! instead of stopping the machine.

use core::fmt;

/// The registers of a system call: its number and its ten argument words
/// as the program passed them. The handler overwrites them with the
/// call's status and its ten result words, which the program gets back.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyscallRegs {
    /// The call's number; on the way back, its status.
    pub number: u64,
    /// The arguments; on the way back, the results.
    pub args: [u64; 10],
}

/// What a program may do with a page mapped into its address space.
/// Nothing is writable and executable at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserAccess {
    /// Read only.
    Read,
    /// Read and write.
    ReadWrite,
    /// Read and execute.
    ReadExecute,
}

/// Why a page could not be mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapPageError {
    /// Something is mapped there already.
    AlreadyMapped,
    /// No memory for a page table.
    NoMemory,
}

/// What went wrong in user mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserFaultKind {
    /// An access to `address` that its page does not allow, or to an
    /// unmapped page.
    Page {
        /// The address accessed.
        address: u64,
        /// Whether it was a write.
        write: bool,
        /// Whether it was an instruction fetch.
        execute: bool,
        /// Whether the page was mapped (so the access was not allowed).
        present: bool,
    },
    /// A privileged instruction, a bad segment, or a non-canonical address.
    Protection,
    /// An instruction the CPU does not know.
    InvalidOpcode,
    /// A breakpoint instruction.
    Breakpoint,
    /// A division by zero or an overflowing division.
    Divide,
    /// A floating-point or vector exception.
    FloatingPoint,
    /// An alignment check.
    Alignment,
    /// Any other exception, by its vector.
    Other(u8),
}

/// A fault in user mode, for the handler that decides the thread's fate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserFault {
    /// What happened.
    pub kind: UserFaultKind,
    /// The program counter of the faulting instruction.
    pub pc: u64,
    /// The stack pointer at the time.
    pub sp: u64,
    /// The CPU's error code, if it gave one (0 otherwise).
    pub code: u64,
}

impl fmt::Display for UserFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            UserFaultKind::Page { address, write, execute, present } => {
                let access = if execute {
                    "executing"
                } else if write {
                    "writing"
                } else {
                    "reading"
                };
                let why = if present { "not allowed" } else { "not mapped" };
                write!(f, "page fault {access} {address:#x} ({why})")?;
            }
            UserFaultKind::Protection => f.write_str("protection fault")?,
            UserFaultKind::InvalidOpcode => f.write_str("invalid instruction")?,
            UserFaultKind::Breakpoint => f.write_str("breakpoint")?,
            UserFaultKind::Divide => f.write_str("division error")?,
            UserFaultKind::FloatingPoint => f.write_str("floating-point exception")?,
            UserFaultKind::Alignment => f.write_str("alignment check")?,
            UserFaultKind::Other(vector) => write!(f, "exception {vector}")?,
        }
        write!(f, " at pc {:#x}, sp {:#x}", self.pc, self.sp)
    }
}

/// User mode, implemented by the architecture alongside
/// [`crate::Threads`], whose contexts user threads run in.
pub trait UserMode: crate::Threads {
    /// A program's address space: page tables for the user half. A handle
    /// that can be shared; the tables are freed when the last handle goes.
    /// The frames its pages map belong to whoever mapped them.
    type Space: Send + Sync;

    /// Kernel memory that can also be mapped into address spaces, such as
    /// a ring's shared pages: zeroed when made, freed when dropped. Only
    /// drop it once no address space maps it any more.
    type Shared: Send + Sync;

    /// The first address above the user half; user addresses are below.
    const USER_TOP: u64;

    /// A new address space with nothing mapped. `None` when memory runs
    /// out.
    fn new_space() -> Option<Self::Space>;

    /// Maps the page at `virt` (page-aligned, below [`UserMode::USER_TOP`])
    /// to the frame at `phys`, with `access`.
    fn map_page(
        space: &Self::Space,
        virt: u64,
        phys: u64,
        access: UserAccess,
    ) -> Result<(), MapPageError>;

    /// Removes the mapping of the page at `virt` from every CPU's view and
    /// returns the frame it mapped.
    fn unmap_page(space: &Self::Space, virt: u64) -> Option<u64>;

    /// The frame and access of the page at `virt`, if it is mapped.
    fn query_page(space: &Self::Space, virt: u64) -> Option<(u64, UserAccess)>;

    /// Makes `context` run in `space` from its next switch on (it keeps a
    /// handle). A context's space is set once; returns `false`, changing
    /// nothing, if it has one already.
    fn attach_space(context: &Self::Context, space: &Self::Space) -> bool;

    /// Sets the thread pointer (x86-64: the FS base) that `context` has in
    /// user mode, at once if it is the running context.
    fn set_thread_pointer(context: &Self::Context, base: u64);

    /// Leaves the kernel for user mode in the running context, at `entry`
    /// with the stack pointer at `stack_top` and `arg` in the first
    /// argument register; every other register is zero. The context must
    /// have a space attached and be running in it.
    fn enter_user(entry: u64, stack_top: u64, arg: u64) -> !;

    /// Registers the system call handler. It runs on the thread's kernel
    /// stack with interrupts masked and a preemption count of 0; it may
    /// turn interrupts on, sleep, and switch threads.
    fn set_syscall_handler(handler: fn(&mut SyscallRegs));

    /// Registers the handler of faults in user mode. It runs on the
    /// thread's kernel stack with interrupts masked, and never returns:
    /// the thread exits.
    fn set_user_fault_handler(handler: fn(&UserFault) -> !);

    /// Registers the function called, with interrupts masked, just before
    /// an interrupt returns to user mode: the kernel stops threads there.
    fn set_user_return_handler(handler: fn());

    /// Copies `dst.len()` bytes of the running thread's user memory from
    /// `src`. Returns `false` if the range is not wholly below
    /// [`UserMode::USER_TOP`] or a page is unmapped or unreadable; `dst`
    /// then holds what was read before it.
    fn copy_from_user(dst: &mut [u8], src: u64) -> bool;

    /// Copies `src` into the running thread's user memory at `dst`, like
    /// [`UserMode::copy_from_user`]; a page must be writable.
    fn copy_to_user(dst: u64, src: &[u8]) -> bool;

    /// `pages` pages of zeroed kernel memory that can be mapped into
    /// address spaces. `None` when memory runs out.
    fn new_shared(pages: usize) -> Option<Self::Shared>;

    /// The physical address of page `page` of `shared`, to map it.
    fn shared_phys(shared: &Self::Shared, page: usize) -> u64;

    /// Reads the 32-bit word at byte `offset` (4-aligned) of `shared`,
    /// with acquire ordering: a program writes it.
    fn shared_load(shared: &Self::Shared, offset: usize) -> u32;

    /// Writes the 32-bit word at byte `offset` (4-aligned) of `shared`,
    /// with release ordering: what was written before is visible to a
    /// program that sees it.
    fn shared_store(shared: &Self::Shared, offset: usize, value: u32);

    /// Copies bytes out of `shared` from `offset`.
    fn shared_read(shared: &Self::Shared, offset: usize, dst: &mut [u8]);

    /// Copies bytes into `shared` at `offset`.
    fn shared_write(shared: &Self::Shared, offset: usize, src: &[u8]);

    /// Fills `pages` pages of RAM from `phys` with zeroes. For frames the
    /// kernel hands to programs, which never see what was there before.
    fn zero_frames(phys: u64, pages: u64);

    /// Copies `src` into RAM at `phys`, for loading programs.
    fn write_frames(phys: u64, src: &[u8]);

    /// The frequency of the cycle counter programs can read (x86-64: the
    /// TSC), in hertz, or 0 if unknown.
    fn cycle_counter_hz() -> u64;

    /// Describes how user mode is entered and protected, for the boot
    /// report.
    fn describe_user_mode(line: &mut dyn FnMut(fmt::Arguments<'_>));
}
