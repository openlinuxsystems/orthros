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
//!
//! Since Phase 8 the kernel also reaches the registers a program entered
//! with: to fork it ([`UserMode::user_registers`]), to run a signal handler
//! ([`UserMode::push_signal_frame`]) and to return from one
//! ([`UserMode::pop_signal_frame`]). Pages of anonymous memory are made as
//! a program touches them, and a fault on one while the kernel copies to
//! or from user memory runs the user page handler, which may make the
//! page, before the copy gives up.

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

/// What [`UserMode::edit_pages`] does with a page that holds a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageEdit {
    /// Leaves it.
    Keep,
    /// Gives it this access, or none (keeping the frame) for `None`.
    Set(Option<UserAccess>),
    /// Removes it: the frame goes to the `released` callback once no CPU
    /// can reach it.
    Clear,
}

/// What [`UserMode::scan`] finds at a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scanned {
    /// The page maps a frame.
    Frame {
        /// Where the frame is.
        phys: u64,
        /// Whether a CPU used it through the entry since the last scan
        /// that forgot its use.
        used: bool,
    },
    /// The page is swapped out, to this slot.
    Swapped(u64),
}

/// A signal handler's frame: what [`UserMode::push_signal_frame`] writes on
/// a program's stack, so that its thread runs `handler(signal, &info,
/// &context)` and returns through `restorer`, which calls the kernel to
/// restore what was saved (radix-signal(2) describes the layout).
pub struct SignalFrame<'a> {
    /// The signal's number.
    pub signal: u32,
    /// Its `siginfo_t`, as the C library declares it.
    pub info: &'a [u8; 128],
    /// The handler to run.
    pub handler: u64,
    /// Where the handler returns to.
    pub restorer: u64,
    /// The signal mask to restore when it returns.
    pub mask: u64,
    /// The alternate stack to run the handler on (its lowest address and
    /// size), if the handler asked for one and the thread is not on it.
    pub stack: Option<(u64, u64)>,
    /// The thread's alternate stack as the context records it: its
    /// address, flags and size.
    pub altstack: (u64, u32, u64),
    /// For a signal a fault raised: the vector, the error code and the
    /// address it faulted on.
    pub trap: Option<(u64, u64, u64)>,
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

    /// A program thread's registers, as it came into the kernel: what a
    /// forked thread starts with.
    type UserRegisters: Clone + Copy + Send + Sync;

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

    /// Maps the `pages` pages from `virt` (page-aligned, below
    /// [`UserMode::USER_TOP`]) to the frames that follow one another from
    /// `phys` on, with `access`, unless one of them is mapped already: all
    /// of them, or none. Where both addresses allow, one large page may
    /// map many of them, which then change only as a whole: whatever
    /// changes part of one calls [`UserMode::split_pages`] first, and
    /// [`UserMode::unmap_page`] leaves it be.
    fn map_pages(
        space: &Self::Space,
        virt: u64,
        phys: u64,
        pages: u64,
        access: UserAccess,
    ) -> Result<(), MapPageError>;

    /// Makes sure no large page ([`UserMode::map_pages`]) maps pages on
    /// both sides of `at` (page-aligned), so that a range that starts or
    /// ends there can change alone.
    fn split_pages(space: &Self::Space, at: u64) -> Result<(), MapPageError>;

    /// Removes the mapping of the page at `virt` from every CPU's view and
    /// returns the frame it mapped.
    fn unmap_page(space: &Self::Space, virt: u64) -> Option<u64>;

    /// The frame and access of the page at `virt`, if it is mapped.
    fn query_page(space: &Self::Space, virt: u64) -> Option<(u64, UserAccess)>;

    /// The frame the page at `virt` holds and the access it gives, `None`
    /// for one that keeps its frame but gives no access.
    fn query_entry(space: &Self::Space, virt: u64) -> Option<(u64, Option<UserAccess>)>;

    /// Calls `each` with the frame of every page from `start` to `end`, in
    /// order, while the page is mapped (writable too if `write`) and `each`
    /// returns `true`: one walk of the page tables, held locked meanwhile,
    /// so `each` must not sleep. A page that keeps its frame without
    /// access stops it, as one not there does.
    fn frames(
        space: &Self::Space,
        start: u64,
        end: u64,
        write: bool,
        each: &mut dyn FnMut(u64) -> bool,
    );

    /// Points the page at `virt` at the frame at `phys` with `access`
    /// (`None`: keeping the frame without access), replacing whatever was
    /// there. `upgrade` says only access to the same frame is added, which
    /// needs no shootdown.
    fn set_entry(
        space: &Self::Space,
        virt: u64,
        phys: u64,
        access: Option<UserAccess>,
        upgrade: bool,
    ) -> Result<(), MapPageError>;

    /// Visits every page from `start` to `end` that holds a frame, in
    /// order, and does what `decide` says, given its address, frame and
    /// access; `released` gets the frame of every page cleared once no
    /// CPU can reach it. Both run with the space's page tables locked,
    /// and must not sleep. A large page ([`UserMode::map_pages`]) goes to
    /// `decide` once, as its first page, and what it says goes for all of
    /// it, which is never held without access; the range must not cut
    /// through one.
    fn edit_pages(
        space: &Self::Space,
        start: u64,
        end: u64,
        decide: &mut dyn FnMut(u64, u64, Option<UserAccess>) -> PageEdit,
        released: &mut dyn FnMut(u64),
    );

    /// Like [`UserMode::edit_pages`] over `from`, and gives `to` an entry
    /// for the same frame wherever `decide` says (its second half), with
    /// that access: how a forked program's pages are shared. `to` must be
    /// a space no thread runs yet. If `to` cannot get a page table for an
    /// entry, the walk stops at that entry and returns its address, with
    /// `to` holding the entries before it.
    fn copy_pages(
        from: &Self::Space,
        to: &Self::Space,
        start: u64,
        end: u64,
        decide: &mut dyn FnMut(
            u64,
            u64,
            Option<UserAccess>,
        ) -> (PageEdit, Option<Option<UserAccess>>),
    ) -> Result<(), u64>;

    /// Calls `each` with every page from `start` to `end` that maps a
    /// frame of its own (not part of a large page) or is swapped out, in
    /// order, until it returns `false`: one walk with the page tables
    /// locked, so `each` must not sleep. If `forget`, whether a frame was
    /// used is forgotten as it is read, so the next scan says whether it
    /// was used since this one (ADR 0152).
    fn scan(
        space: &Self::Space,
        start: u64,
        end: u64,
        forget: bool,
        each: &mut dyn FnMut(u64, Scanned) -> bool,
    );

    /// Swaps the page at `virt` out to swap slot `slot` (below 2^40): if it
    /// still maps the frame at `phys`, unused since [`UserMode::scan`] last
    /// looked, the page holds the slot instead, and `true` says so once no
    /// CPU can reach the frame.
    fn swap_out(space: &Self::Space, virt: u64, phys: u64, slot: u64) -> bool;

    /// Gives the page at `virt`, where nothing is mapped, of `space`, which
    /// no thread runs yet (a fork's copy), the swap slot `slot`.
    fn set_swapped(space: &Self::Space, virt: u64, slot: u64) -> Result<(), MapPageError>;

    /// The swap slot of the page at `virt`, if it is swapped out.
    fn swapped(space: &Self::Space, virt: u64) -> Option<u64>;

    /// Takes every swapped-out page from `start` to `end` out of `space`,
    /// giving `each` its slot; `each` must not sleep.
    fn clear_swapped(space: &Self::Space, start: u64, end: u64, each: &mut dyn FnMut(u64));

    /// Frees the page tables of `space` that map nothing: all of them
    /// once every page is cleared. Their memory goes back from the CPU
    /// that calls this, rather than from wherever the last handle goes.
    fn free_page_tables(space: &Self::Space);

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
    /// thread's kernel stack with interrupts masked; when it returns, the
    /// return handler runs and the thread goes back to user mode (to run
    /// the faulting instruction again, or a signal handler), unless the
    /// thread has exited meanwhile.
    fn set_user_fault_handler(handler: fn(&UserFault));

    /// Registers the handler of page faults on user memory that the
    /// kernel's own copies take: given the address and whether it was a
    /// write, it returns whether it made the page reachable, so that the
    /// copy goes on. It runs with interrupts on and may sleep.
    fn set_user_page_handler(handler: fn(u64, bool) -> bool);

    /// Registers the function called, with interrupts masked, just before
    /// a system call, an interrupt or a fault returns to user mode: the
    /// kernel stops threads and delivers signals there. It may turn
    /// interrupts on, but returns with them masked.
    fn set_user_return_handler(handler: fn());

    /// The running thread's user registers as it came into the kernel. For
    /// a system call only the registers the call keeps are meaningful: the
    /// others hold its results, which [`UserMode::set_results`] fills.
    fn user_registers() -> Option<Self::UserRegisters>;

    /// Puts a system call's status and results in saved registers, as the
    /// call itself returns them.
    fn set_results(regs: &mut Self::UserRegisters, results: &SyscallRegs);

    /// The thread pointer (x86-64: the FS base) saved registers hold, which
    /// [`UserMode::enter_user_registers`] leaves to the context: a forked
    /// thread's context gets it with [`UserMode::set_thread_pointer`].
    fn thread_pointer(regs: &Self::UserRegisters) -> u64;

    /// Leaves the kernel for user mode in the running context with every
    /// register as `regs` has it.
    fn enter_user_registers(regs: &Self::UserRegisters) -> !;

    /// Gives `context`, which has not run yet, the running thread's
    /// extended state (x86-64: x87, SSE, AVX).
    fn copy_extended_state(context: &Self::Context);

    /// The running thread's stack pointer in user mode.
    fn user_stack_pointer() -> u64;

    /// Writes a signal handler's frame on the running thread's stack (or
    /// the alternate stack it names), saving its registers and extended
    /// state there, and makes it return to user mode in the handler.
    /// Returns `false`, changing nothing, if the frame cannot be written.
    fn push_signal_frame(frame: &SignalFrame<'_>) -> bool;

    /// Restores what a signal frame saved, given the address of its
    /// context: the registers a system call keeps go into the running
    /// thread's saved registers (which it then returns with in full), the
    /// rest into `regs`, which the call writes back in place of results.
    /// Returns the signal mask the frame holds, or `None`, changing
    /// nothing, if the frame cannot be read or holds an address outside
    /// the user half.
    fn pop_signal_frame(context: u64, regs: &mut SyscallRegs) -> Option<u64>;

    /// Makes the running thread's system call run again when it returns to
    /// user mode, with the registers it had when it was made.
    fn restart_syscall(original: &SyscallRegs);

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

    /// Copies the page of RAM at `src` to the one at `dst`: a page of
    /// anonymous memory that two address spaces shared, for the one that
    /// writes to it.
    fn copy_frame(dst: u64, src: u64);

    /// Copies RAM at `phys` into `dst`.
    fn read_frames(phys: u64, dst: &mut [u8]);

    /// The frequency of the cycle counter programs can read (x86-64: the
    /// TSC), in hertz, or 0 if unknown.
    fn cycle_counter_hz() -> u64;

    /// Describes how user mode is entered and protected, for the boot
    /// report.
    fn describe_user_mode(line: &mut dyn FnMut(fmt::Arguments<'_>));
}
