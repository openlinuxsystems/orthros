// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Address spaces: page tables for the lower half of the address space,
//! tagged with PCIDs.
//!
//! An [`AddressSpace`] has its own PML4 whose upper half is the kernel's
//! (copied once: the kernel's half of the PML4 never changes after boot)
//! and whose lower half is its own. Switching to it loads its PML4 with
//! its PCID, so on CPUs with PCIDs the TLB keeps every space's entries and
//! the switch flushes nothing, unless the space changed since this CPU last
//! ran it.
//!
//! That is what the space's *TLB generation* tracks: every unmapping or
//! remapping bumps it and shoots the change down on the CPUs running the
//! space now ([`paging::shootdown_on`]); each CPU records the generation it
//! loaded, and one that finds a newer generation when it switches back
//! flushes the space's PCID then. A CPU marks itself as running a space
//! before it reads the generation, and a change bumps the generation
//! before it reads which CPUs run the space, so every CPU either gets the
//! IPI or sees the new generation. PCIDs come from [`radix_mm::Pcids`].
//!
//! Phase 2 has no user processes yet; the `tlb` self-test exercises this.

use core::sync::atomic::{AtomicU64, Ordering, fence};

use radix_hal::MAX_CPUS;
use radix_mm::{AllocError, Owner, Pcids};
use radix_sync::SpinLock;

use crate::memory::{self, PAGE};
use crate::paging::{self, Access, MapError};
use crate::{cpu, percpu};

/// The first address above the lower half.
pub const USER_END: u64 = 0x0000_8000_0000_0000;

const PRESENT: u64 = 1;
const ADDRESS: u64 = 0x000f_ffff_ffff_f000;

static PCIDS: Pcids = Pcids::new();

/// An address space. It lives in a frame of its own, so references to it
/// are `'static` until [`AddressSpace::destroy`].
pub struct AddressSpace {
    /// Physical address of the PML4.
    pml4: u64,
    /// Physical address of the frame holding this structure.
    frame: u64,
    /// The PCID allocator's context value.
    context: AtomicU64,
    /// Bumped by every change that needs a flush.
    tlb_generation: AtomicU64,
    /// CPUs running the space, one bit each.
    active: [AtomicU64; MAX_CPUS / 64],
    /// The generation each CPU last loaded.
    seen: [AtomicU64; MAX_CPUS],
    /// Held while the lower half's tables change.
    lock: SpinLock<()>,
}

const _: () = assert!(core::mem::size_of::<AddressSpace>() <= PAGE as usize);

impl AddressSpace {
    /// A new address space with nothing mapped in its lower half.
    pub fn new() -> Result<&'static AddressSpace, AllocError> {
        let pml4 = paging::new_table(true).map_err(|_| AllocError)?;
        let Some(frame) = memory::alloc_pages(1, false, Owner::Kernel) else {
            memory::free_pages(pml4, 1, Owner::PageTable);
            return Err(AllocError);
        };
        let kernel = paging::table(paging::kernel_pml4());
        paging::table(pml4)[256..].copy_from_slice(&kernel[256..]);
        let space = memory::phys_to_virt(frame) as *mut AddressSpace;
        // SAFETY: `frame` is a fresh page in the direct map, large enough
        // (checked above) and aligned for the structure; it stays allocated
        // until `destroy`, so the reference is valid until then.
        unsafe {
            space.write(AddressSpace {
                pml4,
                frame,
                context: AtomicU64::new(0),
                // Every CPU's `seen` starts at 0, so its first switch here
                // flushes whatever an earlier holder of the PCID left.
                tlb_generation: AtomicU64::new(1),
                active: [const { AtomicU64::new(0) }; MAX_CPUS / 64],
                seen: [const { AtomicU64::new(0) }; MAX_CPUS],
                lock: SpinLock::new(()),
            });
            Ok(&*space)
        }
    }

    fn set_active(&self, cpu: usize, on: bool) {
        let bit = 1 << (cpu % 64);
        if on {
            self.active[cpu / 64].fetch_or(bit, Ordering::SeqCst);
        } else {
            self.active[cpu / 64].fetch_and(!bit, Ordering::SeqCst);
        }
    }

    fn is_active(&self, cpu: usize) -> bool {
        self.active[cpu / 64].load(Ordering::SeqCst) & (1 << (cpu % 64)) != 0
    }

    /// Maps the page at `virt` (in the lower half) to frame `phys`, for
    /// user mode, replacing any earlier mapping.
    pub fn map(&self, virt: u64, phys: u64, access: Access) -> Result<(), MapError> {
        assert!(virt < USER_END && virt.is_multiple_of(PAGE), "{virt:#x} is not a user page");
        let guard = self.lock.lock();
        let slot = paging::entry(self.pml4, virt, 1, true)?;
        let old = core::mem::replace(slot, phys | paging::user_flags(access));
        drop(guard);
        if old & PRESENT != 0 {
            self.flush(virt);
        }
        Ok(())
    }

    /// Removes the mapping of `virt` from every CPU and returns the frame
    /// it mapped, which the caller may then free.
    pub fn unmap(&self, virt: u64) -> Option<u64> {
        let guard = self.lock.lock();
        let slot = paging::leaf(self.pml4, virt).filter(|slot| **slot & PRESENT != 0)?;
        let phys = core::mem::replace(slot, 0) & ADDRESS;
        drop(guard);
        self.flush(virt);
        Some(phys)
    }

    fn flush(&self, virt: u64) {
        self.tlb_generation.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        paging::shootdown_on(virt..virt + PAGE, |cpu| self.is_active(cpu.index()));
    }

    /// Switches this CPU to the space.
    pub fn switch_to(&'static self) {
        let were_enabled = cpu::disable_interrupts();
        let me = percpu::current_index();
        let this = self as *const AddressSpace as usize;
        let previous = percpu::current().space.swap(this, Ordering::Relaxed);
        if previous != this {
            if previous != 0 {
                // SAFETY: a space stays valid while a CPU runs it (see
                // `destroy`), and this CPU ran `previous` until now.
                unsafe { (*(previous as *const AddressSpace)).set_active(me, false) };
            }
            self.set_active(me, true);
            fence(Ordering::SeqCst);
            let generation = self.tlb_generation.load(Ordering::SeqCst);
            let stale = self.seen[me].swap(generation, Ordering::Relaxed) != generation;
            let cr3 = if paging::PCID.load(Ordering::Relaxed) {
                let (pcid, flush_all) = PCIDS.switch(me, &self.context);
                if flush_all {
                    cpu::flush_tlb_all();
                }
                // Bit 63 keeps the PCID's TLB entries.
                let keep = if stale || flush_all { 0 } else { 1 << 63 };
                self.pml4 | u64::from(pcid) | keep
            } else {
                self.pml4
            };
            // SAFETY: the PML4's upper half is the kernel's, so the running
            // code, its stack and everything the kernel uses stay mapped.
            unsafe { cpu::write_cr3(cr3) };
        }
        if were_enabled {
            cpu::enable_interrupts();
        }
    }

    /// Switches this CPU back to the kernel's own tables.
    pub fn switch_to_kernel() {
        let were_enabled = cpu::disable_interrupts();
        let me = percpu::current_index();
        let previous = percpu::current().space.swap(0, Ordering::Relaxed);
        if previous != 0 {
            // SAFETY: as in `switch_to`.
            unsafe { (*(previous as *const AddressSpace)).set_active(me, false) };
            if paging::PCID.load(Ordering::Relaxed) {
                PCIDS.leave(me);
            }
            // PCID 0, flushed: the kernel's lower half changes (the AP
            // trampoline's identity page) are shot down while CPUs may run
            // other PCIDs, so its entries are never kept.
            // SAFETY: the kernel's tables map everything the kernel uses.
            unsafe { cpu::write_cr3(paging::kernel_pml4()) };
        }
        if were_enabled {
            cpu::enable_interrupts();
        }
    }

    /// Frees the space's page tables. The frames its pages map belong to
    /// whoever mapped them and are not freed.
    ///
    /// # Safety
    ///
    /// Nothing may use the reference (or copies of it) afterwards. No CPU
    /// may run the space, which this checks.
    pub unsafe fn destroy(&'static self) {
        assert!(
            self.active.iter().all(|word| word.load(Ordering::SeqCst) == 0),
            "destroying an address space that a CPU runs"
        );
        free_tables(self.pml4, 4, 0..256);
        memory::free_pages(self.pml4, 1, Owner::PageTable);
        // Its PCID stays taken until the next rollover, so no CPU uses the
        // PCID's stale entries before they are flushed.
        memory::free_pages(self.frame, 1, Owner::Kernel);
    }
}

/// Frees the page tables below entries `range` of the table at `phys`.
fn free_tables(phys: u64, level: u32, range: core::ops::Range<usize>) {
    if level == 1 {
        return;
    }
    for index in range {
        let entry = paging::table(phys)[index];
        if entry & PRESENT != 0 && entry & (1 << 7) == 0 {
            free_tables(entry & ADDRESS, level - 1, 0..512);
            memory::free_pages(entry & ADDRESS, 1, Owner::PageTable);
        }
    }
}

/// What the other CPUs check during the `tlb` self-test.
struct Test {
    /// The address to read; it holds `expect`.
    address: AtomicU64,
    expect: AtomicU64,
    /// The spaces of the PCID part, and what each maps at `USER_TEST`.
    spaces: [AtomicU64; 2],
    expect_space: [AtomicU64; 2],
    failures: AtomicU64,
}

static TEST: Test = Test {
    address: AtomicU64::new(0),
    expect: AtomicU64::new(0),
    spaces: [AtomicU64::new(0), AtomicU64::new(0)],
    expect_space: [AtomicU64::new(0), AtomicU64::new(0)],
    failures: AtomicU64::new(0),
};

/// Where the PCID part of the self-test maps its pages.
const USER_TEST: u64 = 0x10_0000_0000;

fn read_user(address: u64) -> u64 {
    let smap = paging::SMAP.load(Ordering::Relaxed);
    if smap {
        cpu::stac();
    }
    // SAFETY: the self-test mapped `address` in the current space before
    // asking for this read, and keeps it mapped until every CPU is done.
    let value = unsafe { core::ptr::read_volatile(address as *const u64) };
    if smap {
        cpu::clac();
    }
    value
}

/// Reads the kernel test page (run on every CPU).
fn check_kernel_page() {
    let address = TEST.address.load(Ordering::Acquire);
    // SAFETY: the self-test keeps this page mapped until every CPU is done.
    let value = unsafe { core::ptr::read_volatile(address as *const u64) };
    if value != TEST.expect.load(Ordering::Relaxed) {
        TEST.failures.fetch_add(1, Ordering::Relaxed);
    }
}

/// Switches between the two test spaces and reads the test page in each
/// (run on every CPU).
fn check_spaces() {
    for round in [0, 1, 0, 1] {
        let space = TEST.spaces[round].load(Ordering::Acquire) as *const AddressSpace;
        // SAFETY: the self-test keeps both spaces alive until every CPU is
        // done, and they live in frames of their own.
        let space: &'static AddressSpace = unsafe { &*space };
        space.switch_to();
        if read_user(USER_TEST) != TEST.expect_space[round].load(Ordering::Relaxed) {
            TEST.failures.fetch_add(1, Ordering::Relaxed);
        }
    }
    AddressSpace::switch_to_kernel();
}

/// Fills a fresh frame with `pattern`.
fn pattern_frame(pattern: u64) -> Result<u64, &'static str> {
    let phys = memory::alloc_pages(1, false, Owner::Kernel).ok_or("out of memory")?;
    // SAFETY: the frame was just allocated and is RAM in the direct map.
    unsafe { core::ptr::write_volatile(memory::phys_to_virt(phys) as *mut u64, pattern) };
    Ok(phys)
}

/// The `tlb` self-test: checks that TLB shootdowns reach every CPU and
/// that switching between address spaces (with PCIDs, when on) never uses
/// stale entries. `on_others` runs a function on every other online CPU
/// and waits for all of them.
pub fn self_test(on_others: &dyn Fn(fn())) -> Result<(), &'static str> {
    const A: u64 = 0x5a5a_0000_0000_000a;
    const B: u64 = 0x5a5a_0000_0000_000b;
    let (a, b) = (pattern_frame(A)?, pattern_frame(B)?);
    let fail = |what: &'static str| {
        if TEST.failures.swap(0, Ordering::Relaxed) == 0 { Ok(()) } else { Err(what) }
    };

    // A kernel page, read everywhere, then pointed at another frame.
    let area = paging::map_area(1).map_err(|_| "out of memory")? as u64;
    let own = paging::remap(area, a);
    TEST.address.store(area, Ordering::Relaxed);
    TEST.expect.store(A, Ordering::Release);
    on_others(check_kernel_page);
    check_kernel_page();
    fail("a CPU read the wrong frame through a fresh mapping")?;
    paging::remap(area, b);
    TEST.expect.store(B, Ordering::Release);
    on_others(check_kernel_page);
    check_kernel_page();
    let shootdown = fail("a CPU kept a stale TLB entry after a shootdown");
    paging::remap(area, own);
    paging::unmap_area(area as usize, 1);
    shootdown?;

    // Two spaces mapping the same address to different frames.
    let spaces = [
        AddressSpace::new().map_err(|_| "out of memory")?,
        AddressSpace::new().map_err(|_| "out of memory")?,
    ];
    let mapped = spaces[0]
        .map(USER_TEST, a, Access::ReadOnly)
        .and_then(|()| spaces[1].map(USER_TEST, b, Access::ReadOnly));
    let result = mapped.map_err(|_| "out of memory").and_then(|()| {
        for (slot, space) in TEST.spaces.iter().zip(&spaces) {
            slot.store(*space as *const AddressSpace as u64, Ordering::Relaxed);
        }
        TEST.expect_space[0].store(A, Ordering::Relaxed);
        TEST.expect_space[1].store(B, Ordering::Release);
        check_spaces();
        on_others(check_spaces);
        fail("a CPU read another address space's page after switching")?;
        // Change the first space while no CPU runs it: every CPU that ran
        // it must flush its PCID when it switches back.
        spaces[0].map(USER_TEST, b, Access::ReadOnly).map_err(|_| "out of memory")?;
        TEST.expect_space[0].store(B, Ordering::Release);
        check_spaces();
        on_others(check_spaces);
        fail("a CPU used a stale entry of an address space that changed while it ran another")
    });
    for space in spaces {
        space.unmap(USER_TEST);
        // SAFETY: every CPU switched back to the kernel's tables in
        // `check_spaces`, and nothing keeps the references.
        unsafe { space.destroy() };
    }
    memory::free_pages(a, 1, Owner::Kernel);
    memory::free_pages(b, 1, Owner::Kernel);
    result
}
