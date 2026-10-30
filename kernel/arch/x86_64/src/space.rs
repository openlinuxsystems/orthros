// SPDX-FileCopyrightText: 2026 The Dukovo contributors
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
//! Pages are 4 KiB ones, but for a run of frames that follow one another
//! ([`AddressSpace::map_run`]: a frame object's, mapped whole), which a
//! page directory's entry maps 2 MiB at a time wherever the virtual and
//! physical addresses both allow (ADR 0121): a copy over such memory then
//! needs one TLB entry for 512 pages, and under a hypervisor saves the
//! walks of two sets of tables at every miss. A large page changes only as
//! a whole: whatever changes part of one splits it first
//! ([`AddressSpace::split`]).
//!
//! User programs run in these spaces (Phase 5's `UserSpace` handles); the
//! `tlb` self-test also checks the shootdowns and PCID switches on every
//! CPU, and large pages.

use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};

use radix_hal::{MAX_CPUS, MapPageError, Scanned, UserAccess};
use radix_mm::{AllocError, Owner, Pcids};
use radix_sync::SpinLock;

use crate::memory::{self, PAGE};
use crate::paging::{self, Access, MapError};
use crate::{cpu, percpu};

/// The first address above the lower half.
pub const USER_END: u64 = 0x0000_8000_0000_0000;

const PRESENT: u64 = 1;
const ADDRESS: u64 = 0x000f_ffff_ffff_f000;
/// A software bit of a leaf entry that is not present: the entry still
/// holds its frame, which a program may not reach for now (a page of
/// anonymous memory under `PROT_NONE`). The CPU ignores every other bit
/// of an entry that is not present.
const HELD: u64 = 1 << 9;
/// A software bit of a leaf entry that is not present: the page is
/// swapped out, and the entry's address bits hold its swap slot rather
/// than a frame (ADR 0152).
const SWAPPED: u64 = 1 << 10;
/// The bit the CPU sets in an entry it reads a page through.
const ACCESSED: u64 = 1 << 5;
/// A large page (in a page-directory or page-directory-pointer entry).
const LARGE: u64 = 1 << 7;
/// What a page directory's entry for a large page maps.
const LARGE_PAGE: u64 = 512 * PAGE;
/// The frame of a large page's entry. Its bit 12 is the PAT bit, which
/// user pages leave clear, as they leave the PAT, PCD and PWT bits of every
/// entry: all user memory is write-back.
const LARGE_ADDRESS: u64 = ADDRESS & !(LARGE_PAGE - 1);

/// The most cleared entries [`AddressSpace::edit`] keeps before it flushes
/// the TLB and hands their frames back.
const EDIT_BATCH: usize = 64;

/// What [`AddressSpace::edit`] does with an entry that holds a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Leaves it as it is.
    Keep,
    /// Gives it this access, or none (keeping the frame) for `None`.
    Set(Option<Access>),
    /// Removes it; the frame is handed back once no CPU can reach it.
    Clear,
}

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
    /// [`UserSpace`] handles to it.
    handles: AtomicUsize,
    /// Whether a large page was ever mapped in it: until one is,
    /// [`AddressSpace::split`] has nothing to look for.
    large: AtomicBool,
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
        for (mine, theirs) in paging::table(pml4)[256..].iter().zip(&kernel[256..]) {
            mine.store(theirs.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        if let Some(record) = paging::table_record(pml4) {
            record.add_table_lines(u64::MAX << 32);
        }
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
                handles: AtomicUsize::new(0),
                large: AtomicBool::new(false),
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
        let old = slot.load(Ordering::Relaxed);
        slot.store(phys | paging::user_flags(access), Ordering::Relaxed);
        drop(guard);
        if old & PRESENT != 0 {
            self.flush(virt);
        }
        Ok(())
    }

    /// Maps the page at `virt` (in the lower half) to frame `phys`, for
    /// user mode, unless something is mapped there already.
    pub fn map_new(&self, virt: u64, phys: u64, access: Access) -> Result<(), MapPageError> {
        assert!(virt < USER_END && virt.is_multiple_of(PAGE), "{virt:#x} is not a user page");
        let _guard = self.lock.lock();
        self.map_locked(virt, phys, access)
    }

    /// [`AddressSpace::map_new`], with the lock held.
    fn map_locked(&self, virt: u64, phys: u64, access: Access) -> Result<(), MapPageError> {
        let slot = paging::entry(self.pml4, virt, 1, true).map_err(|err| match err {
            MapError::LargePage => MapPageError::AlreadyMapped,
            MapError::NoMemory => MapPageError::NoMemory,
        })?;
        if slot.load(Ordering::Relaxed) & PRESENT != 0 {
            return Err(MapPageError::AlreadyMapped);
        }
        // Not present before, so no CPU has it cached: no flush.
        slot.store(phys | paging::user_flags(access), Ordering::Relaxed);
        Ok(())
    }

    /// Maps the `pages` pages from `virt` (in the lower half) to the frames
    /// from `phys` on, which follow one another, for user mode, unless one
    /// of them is mapped already: all of them, or none. Each 2 MiB that
    /// starts at a large page's boundary in both, where no page table is
    /// yet, goes in one entry of the page directory.
    pub fn map_run(
        &self,
        virt: u64,
        phys: u64,
        pages: u64,
        access: Access,
    ) -> Result<(), MapPageError> {
        assert!(
            virt.is_multiple_of(PAGE)
                && phys.is_multiple_of(PAGE)
                && pages <= USER_END.saturating_sub(virt) / PAGE,
            "{pages} pages at {virt:#x} are not user pages"
        );
        let end = virt + pages * PAGE;
        let guard = self.lock.lock();
        let mut at = virt;
        let mut failed = None;
        while at < end {
            let frame = phys + (at - virt);
            let large = at.is_multiple_of(LARGE_PAGE)
                && frame.is_multiple_of(LARGE_PAGE)
                && end - at >= LARGE_PAGE;
            let mapped = if large { self.map_large(at, frame, access) } else { Ok(false) };
            let step = match mapped {
                Ok(true) => Ok(LARGE_PAGE),
                Ok(false) => self.map_locked(at, frame, access).map(|()| PAGE),
                Err(err) => Err(err),
            };
            match step {
                Ok(step) => at += step,
                Err(err) => {
                    failed = Some(err);
                    break;
                }
            }
        }
        drop(guard);
        let Some(err) = failed else { return Ok(()) };
        // A CPU running the space may have reached the pages mapped so far,
        // so they go as any others do.
        self.edit(virt, at, &mut |_, _, _| Edit::Clear, &mut |_| {});
        Err(err)
    }

    /// Maps the large page at `virt` to the frames from `phys` on, unless
    /// a page table that maps something is there (`false`), with the lock
    /// held.
    fn map_large(&self, virt: u64, phys: u64, access: Access) -> Result<bool, MapPageError> {
        let slot = paging::entry(self.pml4, virt, 2, true).map_err(|_| MapPageError::NoMemory)?;
        match slot.load(Ordering::Relaxed) {
            // Not present before, so no CPU has it cached: no flush.
            0 => {}
            entry if entry & LARGE != 0 => return Err(MapPageError::AlreadyMapped),
            entry if is_empty(entry & ADDRESS) => {
                // A page table left empty by an unmap goes once no CPU's
                // paging-structure caches can hold it, as the frees of
                // `free_empty_tables` do.
                slot.store(0, Ordering::Relaxed);
                self.flush_range(virt..virt + LARGE_PAGE);
                memory::free_pages(entry & ADDRESS, 1, Owner::PageTable);
            }
            _ => return Ok(false),
        }
        slot.store(large_entry(phys, access), Ordering::Relaxed);
        self.large.store(true, Ordering::Relaxed);
        Ok(true)
    }

    /// Makes sure no large page maps pages on both sides of `at`, so that
    /// those on either side can change alone: one there becomes a page
    /// table of the same pages. On the way, the large page is unmapped and
    /// shot down before the table takes its place, as Linux splits one: a
    /// CPU must never cache a page's translation as a large page and a
    /// 4 KiB one at once, which some take for a machine check (Intel's
    /// erratum of a page-size change, AMD's erratum 383). A program that
    /// touches it meanwhile faults, and finds it mapped again once the
    /// split is done, under the lock of its regions its fault waits for.
    pub fn split(&self, at: u64) -> Result<(), MapPageError> {
        // The caller orders its maps and splits of a space (the regions'
        // lock), so a large page it mapped is seen here.
        if at.is_multiple_of(LARGE_PAGE) || at >= USER_END || !self.large.load(Ordering::Relaxed) {
            return Ok(());
        }
        let _guard = self.lock.lock();
        let Some((entries, 2)) = mapping(self.pml4, at) else { return Ok(()) };
        let slot = &entries[index(at, 2)];
        let entry = slot.load(Ordering::Relaxed);
        if entry & PRESENT == 0 {
            return Ok(());
        }
        let table = paging::new_table(false).map_err(|_| MapPageError::NoMemory)?;
        let (frame, flags) = (entry & LARGE_ADDRESS, entry & !(ADDRESS | LARGE));
        for (page, leaf) in (0..).zip(paging::table(table)) {
            leaf.store((frame + page * PAGE) | flags, Ordering::Relaxed);
        }
        if let Some(record) = paging::table_record(table) {
            record.set_table_lines(u64::MAX);
        }
        let base = at & !(LARGE_PAGE - 1);
        slot.store(0, Ordering::Relaxed);
        self.flush_range(base..base + LARGE_PAGE);
        slot.store(paging::user_table(table), Ordering::Relaxed);
        Ok(())
    }

    /// The frame the entry for `virt` holds, and the access it gives
    /// (`None` for an entry that holds its frame without giving access).
    pub fn entry(&self, virt: u64) -> Option<(u64, Option<Access>)> {
        let _guard = self.lock.lock();
        let entry = page_entry(self.pml4, virt)?;
        if entry & PRESENT != 0 {
            Some((entry & ADDRESS, Some(paging::access_of(entry))))
        } else if entry & HELD != 0 {
            Some((entry & ADDRESS, None))
        } else {
            None
        }
    }

    /// Calls `each` with every page from `start` to `end` that maps a
    /// frame of its own (not a large page's) or is swapped out, in order,
    /// until it returns `false`, with the tables' lock held. If `forget`,
    /// a frame's entry has its accessed bit cleared as it is read, with no
    /// flush, as Linux's `ptep_clear_flush_young` leaves x86's: a CPU that
    /// still caches it only makes the page look older than it is.
    pub fn scan(
        &self,
        start: u64,
        end: u64,
        forget: bool,
        each: &mut dyn FnMut(u64, Scanned) -> bool,
    ) {
        let _guard = self.lock.lock();
        let end = end.min(USER_END);
        if start < end {
            leaves_of(self.pml4, 4, 0, start, end, &mut |virt, slot| {
                let entry = slot.load(Ordering::Relaxed);
                if entry & PRESENT != 0 {
                    let used = entry & ACCESSED != 0;
                    if used && forget {
                        slot.fetch_and(!ACCESSED, Ordering::Relaxed);
                    }
                    each(virt, Scanned::Frame { phys: entry & ADDRESS, used })
                } else {
                    each(virt, Scanned::Swapped((entry & ADDRESS) >> 12))
                }
            });
        }
    }

    /// Swaps the page at `virt` out to `slot`: if its entry still maps
    /// `phys` and, if `unused`, no CPU read it through the entry since
    /// [`scan`] cleared its accessed bit, the entry holds the slot instead,
    /// and `true` says so once no CPU can reach the frame.
    ///
    /// [`scan`]: AddressSpace::scan
    pub fn swap_out(&self, virt: u64, phys: u64, slot: u64, unused: bool) -> bool {
        let guard = self.lock.lock();
        let Some((entries, 1)) = mapping(self.pml4, virt) else { return false };
        let entry_slot = &entries[index(virt, 1)];
        let entry = entry_slot.load(Ordering::Relaxed);
        if entry & PRESENT == 0 || entry & ADDRESS != phys || unused && entry & ACCESSED != 0 {
            return false;
        }
        let swapped = swap_entry(slot);
        if entry_slot
            .compare_exchange(entry, swapped, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        drop(guard);
        self.flush(virt);
        true
    }

    /// Gives the page at `virt` the swap slot `slot`, in a space no CPU
    /// runs (a fork's copy), where nothing is mapped.
    pub fn set_swapped(&self, virt: u64, slot: u64) -> Result<(), MapPageError> {
        assert!(virt < USER_END && virt.is_multiple_of(PAGE), "{virt:#x} is not a user page");
        let _guard = self.lock.lock();
        let entry = paging::entry(self.pml4, virt, 1, true).map_err(|_| MapPageError::NoMemory)?;
        entry.store(swap_entry(slot), Ordering::Relaxed);
        Ok(())
    }

    /// The swap slot of the page at `virt`, if it is swapped out.
    pub fn swapped(&self, virt: u64) -> Option<u64> {
        let _guard = self.lock.lock();
        let entry = page_entry(self.pml4, virt)?;
        (entry & (PRESENT | SWAPPED) == SWAPPED).then_some((entry & ADDRESS) >> 12)
    }

    /// Takes every swapped-out page from `start` to `end` out of the
    /// tables, giving `each` its slot, with the tables' lock held.
    pub fn clear_swapped(&self, start: u64, end: u64, each: &mut dyn FnMut(u64)) {
        let _guard = self.lock.lock();
        let end = end.min(USER_END);
        if start < end {
            leaves_of(self.pml4, 4, 0, start, end, &mut |_, slot| {
                let entry = slot.load(Ordering::Relaxed);
                if entry & PRESENT == 0 {
                    slot.store(0, Ordering::Relaxed);
                    each((entry & ADDRESS) >> 12);
                }
                true
            });
        }
    }

    /// Calls `each` with the frame of every page from `start` to `end`,
    /// in order, while the page is mapped (writable too if `write`) and
    /// `each` returns `true`: one walk of the tables, with their lock held.
    pub fn frames(&self, start: u64, end: u64, write: bool, each: &mut dyn FnMut(u64) -> bool) {
        let _guard = self.lock.lock();
        let end = end.min(USER_END);
        let mut virt = start & !(PAGE - 1);
        while virt < end {
            let Some((entries, level)) = mapping(self.pml4, virt) else { return };
            let first = ((virt / PAGE) % 512) as usize;
            let count = (end - virt).div_ceil(PAGE).min(512 - first as u64);
            let usable = |entry: u64| {
                entry & PRESENT != 0 && (!write || paging::access_of(entry) == Access::ReadWrite)
            };
            if level == 2 {
                // A large page's frames, from `virt` on.
                let entry = entries[index(virt, 2)].load(Ordering::Relaxed);
                let frame = (entry & LARGE_ADDRESS) + first as u64 * PAGE;
                if !usable(entry) || !(0..count).all(|page| each(frame + page * PAGE)) {
                    return;
                }
            } else {
                for slot in &entries[first..first + count as usize] {
                    let entry = slot.load(Ordering::Relaxed);
                    if !usable(entry) || !each(entry & ADDRESS) {
                        return;
                    }
                }
            }
            virt += count * PAGE;
        }
    }

    /// Points the entry for `virt` at `phys` with `access`, or holds the
    /// frame without access for `None`, replacing whatever was there. A
    /// CPU that cached the old entry is shot down, unless `upgrade` says
    /// the change only gives more access to the same frame: a stale entry
    /// then costs at most a fault that finds the new one.
    pub fn set(
        &self,
        virt: u64,
        phys: u64,
        access: Option<Access>,
        upgrade: bool,
    ) -> Result<(), MapPageError> {
        assert!(virt < USER_END && virt.is_multiple_of(PAGE), "{virt:#x} is not a user page");
        let guard = self.lock.lock();
        let slot = paging::entry(self.pml4, virt, 1, true).map_err(|_| MapPageError::NoMemory)?;
        let old = slot.load(Ordering::Relaxed);
        slot.store(leaf_entry(phys, access), Ordering::Relaxed);
        drop(guard);
        if old & PRESENT != 0 {
            if upgrade && old & ADDRESS == phys {
                paging::flush_local(virt..virt + PAGE);
            } else {
                self.flush(virt);
            }
        }
        Ok(())
    }

    /// Visits every entry from `start` to `end` that holds a frame, in
    /// order, and does what `decide` says with it, given its address, its
    /// frame and its access. The TLB is flushed once for every
    /// [`EDIT_BATCH`] entries cleared, and at the end if any present entry
    /// changed; `released` gets each cleared entry's frame after the
    /// flush, when no CPU can reach it any more. Both run with the
    /// space's page-table lock held, so they must not sleep; `decide` may
    /// map pages into another space. A large page goes to `decide` once,
    /// as its first page, and what it says goes for the whole of it (which
    /// is never held without access); `released` gets each of its frames.
    /// The range must not cut through one ([`AddressSpace::split`]).
    pub fn edit(
        &self,
        start: u64,
        end: u64,
        decide: &mut dyn FnMut(u64, u64, Option<Access>) -> Edit,
        released: &mut dyn FnMut(u64),
    ) {
        assert!(start <= end && end <= USER_END, "editing {start:#x}-{end:#x}");
        if start == end {
            return;
        }
        let mut decide = |virt, frame, access| (decide(virt, frame, access), None);
        let _guard = self.lock.lock();
        let mut walk = Walk::new(start, end, &mut decide, released, None);
        walk.table(self, self.pml4, 4, 0);
        walk.flush(self, end);
    }

    /// Like [`AddressSpace::edit`], and gives `to` an entry for the same
    /// frame wherever `decide` says, with that access (`None`: holding the
    /// frame without access): how a forked program's pages are shared.
    /// `to` must be a space no CPU runs, which gets no flush. If `to`
    /// cannot get a page table for an entry, the walk stops there and
    /// returns that entry's address: `to` has the entries before it.
    pub fn copy_into(
        &self,
        to: &AddressSpace,
        start: u64,
        end: u64,
        decide: &mut dyn FnMut(u64, u64, Option<Access>) -> (Edit, Option<Option<Access>>),
    ) -> Result<(), u64> {
        assert!(start <= end && end <= USER_END, "copying {start:#x}-{end:#x}");
        assert!(!core::ptr::eq(self, to), "copying an address space into itself");
        if start == end {
            return Ok(());
        }
        let mut released = |_| {};
        let _guard = self.lock.lock();
        // Two spaces' locks, always the source's first.
        let _target = to.lock.lock_nested(1);
        let mut walk = Walk::new(start, end, decide, &mut released, Some(to));
        walk.table(self, self.pml4, 4, 0);
        walk.flush(self, end);
        walk.failed.map_or(Ok(()), Err)
    }

    /// The frame `virt` maps and how, if it is mapped.
    pub fn query(&self, virt: u64) -> Option<(u64, Access)> {
        let _guard = self.lock.lock();
        let entry = page_entry(self.pml4, virt)?;
        if entry & PRESENT == 0 {
            return None;
        }
        Some((entry & ADDRESS, paging::access_of(entry)))
    }

    /// Removes the mapping of `virt` from every CPU and returns the frame
    /// it mapped, which the caller may then free. A page of a large page
    /// is not unmapped alone: [`AddressSpace::edit`] takes the large page,
    /// once [`AddressSpace::split`] has made sure of its edges.
    pub fn unmap(&self, virt: u64) -> Option<u64> {
        let guard = self.lock.lock();
        let slot = paging::leaf(self.pml4, virt)?;
        let entry = slot.load(Ordering::Relaxed);
        if entry & PRESENT == 0 {
            return None;
        }
        slot.store(0, Ordering::Relaxed);
        let phys = entry & ADDRESS;
        drop(guard);
        self.flush(virt);
        Some(phys)
    }

    fn flush(&self, virt: u64) {
        self.flush_range(virt..virt + PAGE);
    }

    fn flush_range(&self, range: Range<u64>) {
        self.tlb_generation.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        paging::shootdown_on(range, |cpu| self.is_active(cpu.index()));
    }

    /// Switches this CPU to the space.
    pub fn switch_to(&'static self) {
        let were_enabled = cpu::disable_interrupts();
        let me = percpu::current_index();
        let this = core::ptr::from_ref::<AddressSpace>(self) as usize;
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

    /// Frees the page tables of the lower half that map nothing: all of
    /// them once every page is cleared, as when its program has ended. A
    /// CPU that runs the space is flushed first. Their frames go back
    /// where the pages went, rather than where the last handle goes.
    pub fn free_empty_tables(&self) {
        let _guard = self.lock.lock();
        let mut unlinked = Unlinked { tables: [0; EDIT_BATCH], count: 0 };
        prune(self, self.pml4, 4, 0..256, &mut unlinked);
        unlinked.free(self);
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

/// What [`Walk`]'s callback decides for an entry: its edit, and the entry
/// the copy's target gets for the same frame, if any.
type Decision = (Edit, Option<Option<Access>>);

/// The state of one [`AddressSpace::edit`] or [`AddressSpace::copy_into`].
struct Walk<'a> {
    start: u64,
    end: u64,
    decide: &'a mut dyn FnMut(u64, u64, Option<Access>) -> Decision,
    released: &'a mut dyn FnMut(u64),
    /// The space a copy gives entries to, whose lock the walk holds.
    to: Option<&'a AddressSpace>,
    /// Where a copy stopped, for want of a page table in `to`.
    failed: Option<u64>,
    /// Frames of cleared entries, handed back after the next flush.
    batch: [u64; EDIT_BATCH],
    batched: usize,
    /// Whether a present entry changed since the last flush.
    changed: bool,
    /// Where the range not yet flushed starts.
    flushed_to: u64,
}

/// The entry that holds `frame` with `access`, or without access.
fn leaf_entry(frame: u64, access: Option<Access>) -> u64 {
    match access {
        Some(access) => frame | paging::user_flags(access),
        None => frame | HELD,
    }
}

/// The entry of a page swapped out to `slot`.
fn swap_entry(slot: u64) -> u64 {
    assert!(slot < 1 << 40, "swap slot {slot:#x} does not fit an entry");
    slot << 12 | SWAPPED
}

/// Calls `each` with the address and entry of every leaf of the table at
/// `phys`, of `level` (4 for the PML4), mapping from `base`, from `start`
/// to `end`, that is present (a 4 KiB page's) or swapped out, in order,
/// until it returns `false`, which this returns then too. The caller
/// holds the space's lock.
fn leaves_of(
    phys: u64,
    level: u32,
    base: u64,
    start: u64,
    end: u64,
    each: &mut dyn FnMut(u64, &AtomicU64) -> bool,
) -> bool {
    let span = 1u64 << (12 + 9 * (level - 1));
    let first = ((start.max(base) - base) / span) as usize;
    let last = ((end - 1 - base) / span).min(511) as usize;
    let entries = paging::table(phys);
    for indices in in_use(paging::lines(phys).0, first, last) {
        for index in indices {
            let entry = entries[index].load(Ordering::Relaxed);
            let at = base + index as u64 * span;
            let go_on = if level > 1 {
                entry & (PRESENT | LARGE) != PRESENT
                    || leaves_of(entry & ADDRESS, level - 1, at, start, end, each)
            } else if entry & PRESENT != 0 || entry & (SWAPPED | HELD) == SWAPPED {
                each(at, &entries[index])
            } else {
                true
            };
            if !go_on {
                return false;
            }
        }
    }
    true
}

/// The entry of a large page that maps the frames from `frame` on.
fn large_entry(frame: u64, access: Access) -> u64 {
    frame | paging::user_flags(access) | LARGE
}

/// Whether the page table at `phys` holds no entry, read in its lines in
/// use.
fn is_empty(phys: u64) -> bool {
    let entries = paging::table(phys);
    in_use(paging::lines(phys).0, 0, 511)
        .all(|line| entries[line].iter().all(|entry| entry.load(Ordering::Relaxed) == 0))
}

/// The index of `virt`'s entry in its table at `level`.
fn index(virt: u64, level: u32) -> usize {
    ((virt >> (12 + 9 * (level - 1))) & 0x1ff) as usize
}

/// The table under the PML4 at `root` whose entry maps `virt`, and its
/// level: a page table (1), or a page directory whose entry is a large
/// page's (2). `None` where the tables stop before either.
fn mapping(root: u64, virt: u64) -> Option<(&'static [AtomicU64; 512], u32)> {
    let mut table = root;
    for level in (2..=4).rev() {
        let entries = paging::table(table);
        let entry = entries[index(virt, level)].load(Ordering::Relaxed);
        if entry & PRESENT == 0 {
            return None;
        }
        if entry & LARGE != 0 {
            return (level == 2).then_some((entries, 2));
        }
        table = entry & ADDRESS;
    }
    Some((paging::table(table), 1))
}

/// The entry for the page at `virt` as a page table holds it: a large
/// page's for the page within it, with bit 7 (PAT in a page table's
/// entry) clear.
fn page_entry(root: u64, virt: u64) -> Option<u64> {
    let (entries, level) = mapping(root, virt)?;
    let entry = entries[index(virt, level)].load(Ordering::Relaxed);
    if level == 1 {
        return Some(entry);
    }
    let frame = (entry & LARGE_ADDRESS) + (virt & (LARGE_PAGE - 1) & !(PAGE - 1));
    Some(frame | (entry & !(ADDRESS | LARGE)))
}

impl<'a> Walk<'a> {
    fn new(
        start: u64,
        end: u64,
        decide: &'a mut dyn FnMut(u64, u64, Option<Access>) -> Decision,
        released: &'a mut dyn FnMut(u64),
        to: Option<&'a AddressSpace>,
    ) -> Walk<'a> {
        Walk {
            start,
            end,
            decide,
            released,
            to,
            failed: None,
            batch: [0; EDIT_BATCH],
            batched: 0,
            changed: false,
            flushed_to: start,
        }
    }

    /// Walks the entries of the table at `phys`, of `level` (4 for the
    /// PML4), that cover the range; the table maps from `base`.
    fn table(&mut self, space: &AddressSpace, phys: u64, level: u32, base: u64) {
        if level == 1 {
            self.leaves(space, phys, base);
            return;
        }
        let span = 1u64 << (12 + 9 * (level - 1));
        let first = ((self.start.max(base) - base) / span) as usize;
        let last = ((self.end - 1 - base) / span).min(511) as usize;
        let entries = paging::table(phys);
        // A range within one line is read whatever the lines in use say,
        // without looking up the table's record.
        let lines = if first / paging::LINE == last / paging::LINE {
            u64::MAX
        } else {
            paging::lines(phys).0
        };
        for indices in in_use(lines, first, last) {
            for index in indices {
                if self.failed.is_some() {
                    return;
                }
                let entry = entries[index].load(Ordering::Relaxed);
                let at = base + index as u64 * span;
                // Programs' large pages are all 2 MiB ones.
                if entry & PRESENT != 0 && entry & LARGE == 0 {
                    self.table(space, entry & ADDRESS, level - 1, at);
                } else if entry & PRESENT != 0 && level == 2 {
                    self.large(space, &entries[index], entry, at);
                }
            }
        }
    }

    /// Does what `decide` says with the large page whose entry, `entry`,
    /// is at `slot` and maps from `base`; the range covers it whole.
    fn large(&mut self, space: &AddressSpace, slot: &AtomicU64, entry: u64, base: u64) {
        assert!(
            self.start <= base && base + LARGE_PAGE <= self.end,
            "a walk from {:#x} to {:#x} cuts through the large page at {base:#x}",
            self.start,
            self.end
        );
        let frame = entry & LARGE_ADDRESS;
        let held = || panic!("the large page at {base:#x} would be held without access");
        let (edit, copy) = (self.decide)(base, frame, Some(paging::access_of(entry)));
        if let (Some(to), Some(access)) = (self.to, copy) {
            let access = access.unwrap_or_else(held);
            // As for a page table's entries: the target's lock is held,
            // and no CPU runs it.
            match paging::entry(to.pml4, base, 2, true) {
                Ok(target) => {
                    target.store(large_entry(frame, access), Ordering::Relaxed);
                    to.large.store(true, Ordering::Relaxed);
                }
                Err(_) => {
                    self.failed = Some(base);
                    return;
                }
            }
        }
        match edit {
            Edit::Keep => {}
            Edit::Set(access) => {
                let new = large_entry(frame, access.unwrap_or_else(held));
                slot.store(new, Ordering::Relaxed);
                self.changed |= new != entry;
            }
            Edit::Clear => {
                slot.store(0, Ordering::Relaxed);
                self.changed = true;
                // Its 512 frames are handed back at once, after a flush.
                self.flush(space, base + LARGE_PAGE);
                for page in 0..LARGE_PAGE / PAGE {
                    (self.released)(frame + page * PAGE);
                }
            }
        }
    }

    /// Walks the entries of the page table at `phys`, which maps from
    /// `base`, that cover the range: each one that holds a frame goes to
    /// `decide`.
    fn leaves(&mut self, space: &AddressSpace, phys: u64, base: u64) {
        let first = ((self.start.max(base) - base) / PAGE) as usize;
        let last = ((self.end - 1 - base) / PAGE).min(511) as usize;
        let entries = paging::table(phys);
        // Nor does a range within part of a line need the record: it
        // empties no whole line.
        let (lines, record) =
            if first / paging::LINE == last / paging::LINE && last - first + 1 < paging::LINE {
                (u64::MAX, None)
            } else {
                paging::lines(phys)
            };
        // Lines the walk saw whole and left empty, which no longer count
        // as in use.
        let mut emptied = 0;
        // The copy's page table for the same 2 MiB, found (or made) when
        // the first entry goes to it, and the lines given entries there.
        let mut target: Option<u64> = None;
        let mut filled = 0;
        'lines: for indices in in_use(lines, first, last) {
            let (bit, whole) = (paging::line_bit(indices.start), indices.len() == paging::LINE);
            // What the line's entries hold once the walk is done with them.
            let mut left = 0;
            for index in indices {
                let slot = &entries[index];
                let entry = slot.load(Ordering::Relaxed);
                let present = entry & PRESENT != 0;
                if !present && entry & HELD == 0 {
                    left |= entry;
                    continue;
                }
                let virt = base + index as u64 * PAGE;
                let frame = entry & ADDRESS;
                let access = present.then(|| paging::access_of(entry));
                let (edit, copy) = (self.decide)(virt, frame, access);
                if let (Some(to), Some(access)) = (self.to, copy) {
                    // The target's lock is held (`copy_into`), and no CPU
                    // runs it, so its new entry needs no flush.
                    let table = match target {
                        Some(table) => table,
                        None => match paging::table_of(to.pml4, virt, 1, true) {
                            Ok(table) => *target.insert(table),
                            Err(_) => {
                                self.failed = Some(virt);
                                break 'lines;
                            }
                        },
                    };
                    paging::table(table)[index].store(leaf_entry(frame, access), Ordering::Relaxed);
                    filled |= paging::line_bit(index);
                }
                match edit {
                    Edit::Keep => left |= entry,
                    Edit::Set(access) => {
                        let new = leaf_entry(frame, access);
                        slot.store(new, Ordering::Relaxed);
                        self.changed |= present && new != entry;
                        left |= new;
                    }
                    Edit::Clear => {
                        slot.store(0, Ordering::Relaxed);
                        self.changed |= present;
                        self.batch[self.batched] = frame;
                        self.batched += 1;
                        if self.batched == EDIT_BATCH {
                            self.flush(space, virt + PAGE);
                        }
                    }
                }
            }
            if whole && left == 0 {
                emptied |= bit;
            }
        }
        if let Some(record) = record.filter(|_| emptied != 0) {
            record.set_table_lines(record.table_lines() & !emptied);
        }
        if let Some(record) = target.and_then(paging::table_record) {
            record.add_table_lines(filled);
        }
    }

    /// Flushes what changed below `to` and hands back the cleared frames.
    fn flush(&mut self, space: &AddressSpace, to: u64) {
        if self.changed {
            space.tlb_generation.fetch_add(1, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            paging::shootdown_on(self.flushed_to..to, |cpu| space.is_active(cpu.index()));
            self.changed = false;
        }
        self.flushed_to = to;
        for &frame in &self.batch[..self.batched] {
            (self.released)(frame);
        }
        self.batched = 0;
    }
}

/// A shared handle to a program's address space; the space is destroyed
/// when the last handle goes. Only threads' contexts switch to it, and
/// each keeps a handle, so no CPU runs a space whose last handle is gone.
pub struct UserSpace(&'static AddressSpace);

impl UserSpace {
    /// A new, empty address space.
    pub fn new() -> Option<UserSpace> {
        let space = AddressSpace::new().ok()?;
        space.handles.store(1, Ordering::Relaxed);
        Some(UserSpace(space))
    }

    /// The space.
    pub fn space(&self) -> &'static AddressSpace {
        self.0
    }
}

impl Clone for UserSpace {
    fn clone(&self) -> UserSpace {
        self.0.handles.fetch_add(1, Ordering::Relaxed);
        UserSpace(self.0)
    }
}

impl Drop for UserSpace {
    fn drop(&mut self) {
        if self.0.handles.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire);
            // SAFETY: this was the last handle, and only holders of a
            // handle reach the space: nothing uses it afterwards. A context
            // keeps a handle while it may run, so no CPU runs the space
            // (which `destroy` checks).
            unsafe { self.0.destroy() };
        }
    }
}

/// The HAL's access rights as a mapping's.
pub fn access(access: UserAccess) -> Access {
    match access {
        UserAccess::Read => Access::ReadOnly,
        UserAccess::ReadWrite => Access::ReadWrite,
        UserAccess::ReadExecute => Access::Code,
    }
}

/// A mapping's access rights as the HAL's.
pub fn user_access(access: Access) -> UserAccess {
    match access {
        Access::ReadOnly => UserAccess::Read,
        Access::ReadWrite => UserAccess::ReadWrite,
        Access::Code => UserAccess::ReadExecute,
    }
}

/// Frees the page tables below entries `range` of the table at `phys`.
fn free_tables(phys: u64, level: u32, range: Range<usize>) {
    if level == 1 || range.is_empty() {
        return;
    }
    let entries = paging::table(phys);
    for indices in in_use(paging::lines(phys).0, range.start, range.end - 1) {
        for entry in &entries[indices] {
            let entry = entry.load(Ordering::Relaxed);
            if entry & PRESENT != 0 && entry & LARGE == 0 {
                free_tables(entry & ADDRESS, level - 1, 0..512);
                memory::free_pages(entry & ADDRESS, 1, Owner::PageTable);
            }
        }
    }
}

/// Page tables taken out of a space, freed once no CPU can reach them.
struct Unlinked {
    tables: [u64; EDIT_BATCH],
    count: usize,
}

impl Unlinked {
    /// Adds `table`, which no entry of `space` points to any more,
    /// freeing those before it first if there is no room.
    fn add(&mut self, space: &AddressSpace, table: u64) {
        if self.count == EDIT_BATCH {
            self.free(space);
        }
        self.tables[self.count] = table;
        self.count += 1;
    }

    /// Flushes the lower half from the CPUs that run `space`, whose
    /// paging-structure caches may still hold the tables, then frees them.
    fn free(&mut self, space: &AddressSpace) {
        if self.count == 0 {
            return;
        }
        space.tlb_generation.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        paging::shootdown_on(0..USER_END, |cpu| space.is_active(cpu.index()));
        for &table in &self.tables[..self.count] {
            memory::free_pages(table, 1, Owner::PageTable);
        }
        self.count = 0;
    }
}

/// Takes the tables below entries `range` of the table at `phys` (of
/// `level`) that map nothing out of `space`, into `unlinked`; whether no
/// entry in `range` is left.
fn prune(
    space: &AddressSpace,
    phys: u64,
    level: u32,
    range: Range<usize>,
    unlinked: &mut Unlinked,
) -> bool {
    let entries = paging::table(phys);
    let (lines, record) = paging::lines(phys);
    let (mut emptied, mut empty) = (0, true);
    for indices in in_use(lines, range.start, range.end - 1) {
        let (bit, whole) = (paging::line_bit(indices.start), indices.len() == paging::LINE);
        let mut left = 0;
        for slot in &entries[indices] {
            let entry = slot.load(Ordering::Relaxed);
            let table = entry & PRESENT != 0 && entry & LARGE == 0 && level > 1;
            if table && prune(space, entry & ADDRESS, level - 1, 0..512, unlinked) {
                slot.store(0, Ordering::Relaxed);
                unlinked.add(space, entry & ADDRESS);
                continue;
            }
            left |= entry;
        }
        if whole && left == 0 {
            emptied |= bit;
        }
        empty &= left == 0;
    }
    if let Some(record) = record.filter(|_| emptied != 0) {
        record.set_table_lines(record.table_lines() & !emptied);
    }
    empty
}

/// The entries from `first` to `last` of a table in its lines in use
/// (`lines`, from [`paging::lines`]), a line at a time: most of a
/// program's page tables is empty, and a walk never reads those lines.
fn in_use(lines: u64, first: usize, last: usize) -> impl Iterator<Item = Range<usize>> {
    let (low, high) = (first / paging::LINE, last / paging::LINE);
    let mut bits = lines & (u64::MAX << low) & (u64::MAX >> (63 - high));
    core::iter::from_fn(move || {
        if bits == 0 {
            return None;
        }
        let line = bits.trailing_zeros() as usize;
        bits &= bits - 1;
        Some((line * paging::LINE).max(first)..(line * paging::LINE + paging::LINE).min(last + 1))
    })
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
/// (run on every CPU). With interrupts masked: a context switch loads the
/// incoming context's own tables, which a kernel thread's are not.
fn check_spaces() {
    let were_enabled = cpu::disable_interrupts();
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
    if were_enabled {
        cpu::enable_interrupts();
    }
}

/// Reads `TEST.address` in the first test space (run on every CPU).
fn check_large_page() {
    let were_enabled = cpu::disable_interrupts();
    let space = TEST.spaces[0].load(Ordering::Acquire) as *const AddressSpace;
    // SAFETY: as in `check_spaces`.
    let space: &'static AddressSpace = unsafe { &*space };
    space.switch_to();
    if read_user(TEST.address.load(Ordering::Relaxed)) != TEST.expect.load(Ordering::Relaxed) {
        TEST.failures.fetch_add(1, Ordering::Relaxed);
    }
    AddressSpace::switch_to_kernel();
    if were_enabled {
        cpu::enable_interrupts();
    }
}

/// Where the large-page part of the self-test maps its pages, and what
/// the first word of each holds, with the page's index.
const LARGE_TEST: u64 = 0x20_0000_0000;
const LARGE_PATTERN: u64 = 0x5a5a_0000_1a00_0000;

/// The large-page part of the `tlb` self-test, with `b` a frame holding
/// `pattern`: a run of 512 frames mapped by one large page, read, looked
/// up, copied into another space, split, changed and unmapped.
fn large_pages(on_others: &dyn Fn(fn()), b: u64, pattern: u64) -> Result<(), &'static str> {
    // A power of two of pages comes aligned to its size.
    let run = memory::alloc_pages(512, false, Owner::Kernel).ok_or("out of memory")?;
    for page in 0..512 {
        // SAFETY: the run was just allocated and is RAM in the direct map.
        unsafe {
            let word = memory::phys_to_virt(run + page * PAGE) as *mut u64;
            core::ptr::write_volatile(word, LARGE_PATTERN + page);
        }
    }
    let spaces = [AddressSpace::new(), AddressSpace::new()];
    let result = match &spaces {
        [Ok(space), Ok(copy)] => large_pages_in(on_others, space, copy, run, (b, pattern)),
        _ => Err("out of memory"),
    };
    for space in spaces.into_iter().flatten() {
        let end = LARGE_TEST + 2 * LARGE_PAGE;
        space.edit(LARGE_TEST, end, &mut |_, _, _| Edit::Clear, &mut |_| {});
        // SAFETY: every CPU switched back to the kernel's tables in
        // `check_large_page`, and nothing keeps the references.
        unsafe { space.destroy() };
    }
    memory::free_pages(run, 512, Owner::Kernel);
    result
}

fn large_pages_in(
    on_others: &dyn Fn(fn()),
    space: &'static AddressSpace,
    copy: &'static AddressSpace,
    run: u64,
    (b, pattern): (u64, u64),
) -> Result<(), &'static str> {
    let page = |n: u64| LARGE_TEST + n * PAGE;
    let read_everywhere = |at: u64, expect: u64| {
        TEST.spaces[0].store(core::ptr::from_ref(space) as u64, Ordering::Relaxed);
        TEST.address.store(at, Ordering::Relaxed);
        TEST.expect.store(expect, Ordering::Release);
        check_large_page();
        on_others(check_large_page);
        TEST.failures.swap(0, Ordering::Relaxed) == 0
    };
    let check = |ok: bool, what: &'static str| if ok { Ok(()) } else { Err(what) };
    let mut cleared = |_: u64, _: u64, _: Option<Access>| Edit::Clear;

    // A page table an unmap left empty gives way to a large page.
    space.map_new(page(517), b, Access::ReadOnly).map_err(|_| "out of memory")?;
    space.edit(page(517), page(518), &mut cleared, &mut |_| {});
    space.map_run(page(512), run, 512, Access::ReadOnly).map_err(|_| "out of memory")?;
    check(
        matches!(mapping(space.pml4, page(517)), Some((_, 2))),
        "an empty page table kept a large page out",
    )?;
    space.edit(page(512), page(1024), &mut cleared, &mut |_| {});

    space.map_run(page(0), run, 512, Access::ReadOnly).map_err(|_| "out of memory")?;
    check(
        matches!(mapping(space.pml4, page(5)), Some((_, 2))),
        "a run of 512 frames got no large page",
    )?;
    check(
        space.query(page(5) + 8) == Some((run + 5 * PAGE, Access::ReadOnly))
            && space.entry(page(511)) == Some((run + 511 * PAGE, Some(Access::ReadOnly))),
        "a page of a large page looked up wrong",
    )?;
    check(
        space.map_new(page(7), b, Access::ReadOnly) == Err(MapPageError::AlreadyMapped)
            && space.map_run(page(0), run, 512, Access::ReadOnly)
                == Err(MapPageError::AlreadyMapped),
        "a page of a large page was mapped again",
    )?;
    let (mut next, mut seen) = (run + 510 * PAGE, 0);
    space.frames(page(510), page(513), false, &mut |frame| {
        seen += u64::from(frame == next);
        next += PAGE;
        true
    });
    check(seen == 2 && next == run + 512 * PAGE, "a large page's frames came wrong")?;
    space.frames(page(0), page(1), true, &mut |_| {
        seen = 0;
        true
    });
    check(seen == 2, "a read-only large page's frames came for writing")?;
    check(
        read_everywhere(page(300), LARGE_PATTERN + 300),
        "a CPU read the wrong frame through a large page",
    )?;

    // A fork's copy gets the large page, asked about once.
    let mut asked = 0;
    let copied = space.copy_into(copy, page(0), page(512), &mut |_, _, access| {
        asked += 1;
        (Edit::Keep, Some(access))
    });
    check(
        copied.is_ok()
            && asked == 1
            && matches!(mapping(copy.pml4, page(3)), Some((_, 2)))
            && copy.query(page(3)) == Some((run + 3 * PAGE, Access::ReadOnly)),
        "a copied large page came wrong",
    )?;

    // Split, its pages stay; a page changed is read changed everywhere.
    space.split(page(100)).map_err(|_| "out of memory")?;
    check(
        matches!(mapping(space.pml4, page(5)), Some((_, 1)))
            && space.query(page(5)) == Some((run + 5 * PAGE, Access::ReadOnly))
            && space.query(page(511)) == Some((run + 511 * PAGE, Access::ReadOnly)),
        "a split large page's pages came wrong",
    )?;
    space.set(page(300), b, Some(Access::ReadOnly), false).map_err(|_| "out of memory")?;
    check(read_everywhere(page(300), pattern), "a CPU read a split large page's old page")?;
    check(
        read_everywhere(page(301), LARGE_PATTERN + 301),
        "a CPU read the wrong page of a split large page",
    )?;

    // Unmapped whole: every frame comes back once.
    let mut released = 0;
    copy.edit(page(0), page(512), &mut cleared, &mut |_| released += 1);
    check(released == 512 && copy.query(page(3)).is_none(), "an unmapped large page stayed")
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
            slot.store(core::ptr::from_ref::<AddressSpace>(*space) as u64, Ordering::Relaxed);
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
    let result = result.and_then(|()| large_pages(on_others, b, B));
    memory::free_pages(a, 1, Owner::Kernel);
    memory::free_pages(b, 1, Owner::Kernel);
    result
}
