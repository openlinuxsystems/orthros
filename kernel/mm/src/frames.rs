// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The frame allocator: buddy zones per NUMA node.
//!
//! Physical memory is split into zones by node and by what devices can
//! reach: each node has a `Dma32` zone (RAM below 4 GiB, for 32-bit DMA)
//! and a `Normal` zone (the rest). Each zone is a [`Buddy`] allocator
//! behind its own lock. An allocation walks the zones in fallback order:
//! the preferred node's `Normal` zone, then its `Dma32` zone, then the
//! other nodes nearest first (ACPI SLIT distances), unless the flags ask
//! for one node only or for `Dma32` memory.
//!
//! Two floors protect zones from running dry:
//!
//! - every zone keeps a *reserve*, between 64 KiB and 4 MiB, that only
//!   `Atomic` allocations may take, so interrupt handlers can still
//!   allocate when sleeping callers have used everything else;
//! - when a machine has memory above 4 GiB, allocations that could use it
//!   leave 1/16 of each `Dma32` zone (at most 64 MiB) for the devices that
//!   need it.
//!
//! Before giving up, a `MaySleep` allocation runs the reclaim hook (the
//! heap's [`crate::Heap::drain`]) and tries once more. If that fails too
//! and the block is small (at most [`OOM_MAX_ORDER`]), it runs the kernel's
//! out-of-memory hook, which may free memory by stopping a resource domain
//! and asks for another try, at most four times for one allocation (Phase
//! 4; `docs/design/scheduler.md`). Larger
//! blocks fail instead: running out of contiguous memory is not running out
//! of memory.
//!
//! Each CPU keeps a cache of free single frames from its node's first zone
//! with memory, as Linux keeps per-CPU page lists. An order-0 allocation
//! that may come from that zone takes the frame the CPU freed last, and
//! freeing such a frame puts it back, without the zone's lock and without
//! the buddy splits and merges, which cost some thousand instructions a
//! frame. An empty cache takes [`CACHE_BATCH`] frames from above the zone's
//! floors; one holding more than its limit (1/512 of the zone, between 64
//! frames and 2 MiB) gives its oldest batch back. Cached frames count as
//! free, and an allocation that finds nothing empties every CPU's cache
//! into the zones before it tries again.
//!
//! Memory filled in order, as a program first writes a buffer from its
//! start to its end, takes its frames from a run instead
//! ([`Frames::alloc_sequential`]): a block of [`RUN_FRAMES`] contiguous
//! frames each CPU takes from its zone at once and hands out from the
//! lowest up, so that the buffer lies in long stretches of physical
//! memory, as Linux's mostly does, rather than in whatever frames were
//! freed last. A device takes such a buffer in fewer, longer pieces, and a
//! hypervisor copies it faster. The frames of a run not yet handed out
//! count as cached.
//!
//! A frame whose memory went bad, which a machine check reports, is
//! retired ([`Frames::retire`]): taken out of its zone at once if it is
//! free, or else when its block is freed, and never handed out again.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use radix_sync::{Once, RawSpinLock, SpinLock};

use crate::buddy::{Buddy, FreeError, MAX_ORDER, ORDERS, RegionError};
use crate::context::{AllocError, Context, Flags, NodePolicy, ZoneLimit};
use crate::page::{NIL, Owner, PAGE_SHIFT, PAGE_SIZE, PageInfo, Region, State};
use crate::platform::{MAX_CPUS, NoInterrupts, Platform};

/// Most NUMA nodes.
pub const MAX_NODES: usize = 8;
/// Most regions of RAM across all zones.
pub const MAX_REGIONS: usize = 64;
/// Frames per section of physical memory, 2^15 (128 MiB, Linux's section
/// size on x86_64): `Frames::entry` finds a frame's region through the
/// section it is in.
const SECTION_SHIFT: u32 = 15;
/// Sections the map covers: the first 1 TiB of physical addresses. A
/// frame above is found by a binary search.
const SECTIONS: usize = 8192;
const _: () = assert!(MAX_REGIONS < u8::MAX as usize, "a section names its region in a byte");
/// First frame of the `Normal` zone: 4 GiB.
const DMA32_END_PFN: u64 = 1 << (32 - PAGE_SHIFT);
/// Zones per node.
const KINDS: usize = 2;
/// The largest order whose failure runs the out-of-memory hook: 32 KiB,
/// Linux's "costly" boundary.
pub const OOM_MAX_ORDER: u32 = 3;
/// How many times one allocation asks the out-of-memory hook for memory
/// before it fails.
const OOM_ATTEMPTS: u32 = 4;
/// Frames a CPU's cache takes from its zone, or gives back, at a time.
pub const CACHE_BATCH: u32 = 32;
/// Most frames a CPU's cache holds: 2 MiB.
const CACHE_MOST: u32 = 512;
/// The order of the blocks a CPU's run takes from its zone.
const RUN_ORDER: u32 = 5;
/// Frames in a run: 128 KiB.
pub const RUN_FRAMES: u32 = 1 << RUN_ORDER;
/// Most frames [`Frames::retire`] takes out of use: 2 MiB of bad memory,
/// well past where a machine's memory wants replacing.
pub const MAX_RETIRED: usize = 512;
/// Times [`Frames::retire`] empties the CPUs' caches to find a free frame
/// in them before it leaves the frame to be withheld when freed.
const RETIRE_ATTEMPTS: u32 = 4;

/// What [`Frames::retire`] did with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retired {
    /// It is out of use for good: it was free and left its zone, or the
    /// allocator never handed it out.
    Now,
    /// It is in use. It leaves its zone when its block is freed.
    WhenFreed,
    /// The allocator does not manage it.
    NotManaged,
    /// [`MAX_RETIRED`] frames are retired already.
    TooMany,
}

/// What memory a zone holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoneKind {
    /// RAM below 4 GiB.
    Dma32,
    /// RAM at and above 4 GiB.
    Normal,
}

impl ZoneKind {
    /// The kind of zone frame `pfn` belongs in.
    pub fn of(pfn: u64) -> ZoneKind {
        if pfn < DMA32_END_PFN { ZoneKind::Dma32 } else { ZoneKind::Normal }
    }

    /// Name for reports.
    pub fn name(self) -> &'static str {
        match self {
            ZoneKind::Dma32 => "DMA32",
            ZoneKind::Normal => "Normal",
        }
    }

    fn index(self) -> usize {
        match self {
            ZoneKind::Dma32 => 0,
            ZoneKind::Normal => 1,
        }
    }
}

/// Why memory could not be handed to the frame allocator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupError {
    /// More than [`MAX_REGIONS`] regions, or more than the zone can hold.
    TooManyRegions,
    /// A region overlaps another, crosses 4 GiB, lies above 16 TiB, or names
    /// a node above [`MAX_NODES`].
    InvalidRegion,
    /// The allocator is already running.
    AlreadyRunning,
}

impl From<RegionError> for SetupError {
    fn from(error: RegionError) -> SetupError {
        match error {
            RegionError::TooMany => SetupError::TooManyRegions,
            RegionError::Invalid => SetupError::InvalidRegion,
        }
    }
}

/// A snapshot of one zone, for reports.
#[derive(Clone, Copy, Debug)]
pub struct ZoneStats {
    /// Its node.
    pub node: usize,
    /// Its kind.
    pub kind: ZoneKind,
    /// Frames it manages.
    pub managed: u64,
    /// Frames free, those CPUs' caches hold included.
    pub free: u64,
    /// Frames kept for atomic allocations.
    pub reserve: u64,
    /// Free blocks per order.
    pub blocks: [u32; ORDERS],
    /// Lowest and one past the highest physical address it spans.
    pub span: (u64, u64),
}

struct Zone {
    buddy: SpinLock<Buddy<'static>>,
    /// Frames only atomic allocations may take.
    reserve: AtomicU64,
    /// Frames kept for `Dma32` allocations (in `Dma32` zones only).
    protect: AtomicU64,
    /// Free frames of the zone that CPUs' caches hold.
    cached: AtomicU64,
}

impl Zone {
    const fn new() -> Zone {
        Zone {
            buddy: SpinLock::new(Buddy::new()),
            reserve: AtomicU64::new(0),
            protect: AtomicU64::new(0),
            cached: AtomicU64::new(0),
        }
    }
}

/// One CPU's cache of free frames, on a cache line of its own. Its lock is
/// taken with interrupts masked, by its CPU but for the rare emptying of
/// every cache, and only the zone's lock is taken inside it: a raw lock
/// needs neither the preemption hooks nor a lock site, whose pointer would
/// cost the KASLR table an entry per CPU.
#[repr(align(64))]
struct CpuFrames {
    cache: RawSpinLock<Cached>,
}

impl CpuFrames {
    const fn new() -> CpuFrames {
        CpuFrames { cache: RawSpinLock::new(Cached::new()) }
    }
}

/// Free frames of one zone, linked through their records' `next` and
/// `prev` like a free list, the one freed last at the head. Their records
/// say [`State::Cached`]: allocated, as far as the zone knows.
struct Cached {
    /// The zone, once the CPU has first used its cache; `None` in it if
    /// the CPU's node has no memory.
    zone: Option<Option<usize>>,
    head: u32,
    tail: u32,
    len: u32,
    /// Frames it holds before it gives a batch back.
    limit: u32,
    /// The frames from `run` to `run_end` of the block it hands out in
    /// order ([`Frames::alloc_sequential`]), cached too.
    run: u32,
    run_end: u32,
}

impl Cached {
    const fn new() -> Cached {
        Cached { zone: None, head: NIL, tail: NIL, len: 0, limit: 0, run: 0, run_end: 0 }
    }
}

#[derive(Clone, Copy)]
struct RegionEntry {
    region: Region<'static>,
    zone: usize,
}

struct RegionTable {
    entries: [Option<RegionEntry>; MAX_REGIONS],
    len: usize,
    /// The entry with the most frames, which most lookups land in: looked
    /// at first.
    largest: usize,
}

impl RegionTable {
    const fn new() -> RegionTable {
        RegionTable { entries: [None; MAX_REGIONS], len: 0, largest: 0 }
    }

    fn iter(&self) -> impl Iterator<Item = &RegionEntry> {
        self.entries[..self.len].iter().flatten()
    }

    /// The entry containing frame `pfn`, from entry `first` on: the
    /// entries are sorted by base, so the first that starts above `pfn`
    /// ends the search.
    #[inline]
    fn find_from(&self, first: usize, pfn: u64) -> Option<&RegionEntry> {
        let entries = self.entries.get(first..self.len)?.iter().flatten();
        entries.take_while(|e| e.region.base <= pfn).find(|e| e.region.contains(pfn))
    }

    /// The entry containing frame `pfn`, by a binary search.
    fn search(&self, pfn: u64) -> Option<&RegionEntry> {
        let entries = &self.entries[..self.len];
        let index = entries.partition_point(|e| e.is_some_and(|e| e.region.end() <= pfn));
        entries.get(index)?.as_ref().filter(|e| e.region.contains(pfn))
    }
}

/// The frame allocator.
pub struct Frames {
    zones: [Zone; MAX_NODES * KINDS],
    platform: Once<&'static dyn Platform>,
    building: SpinLock<RegionTable>,
    regions: Once<RegionTable>,
    /// For each section of physical memory, 1 + the index of the first
    /// entry of `regions` with frames in it, or 0 for none: set before
    /// `regions` is, and never changed after.
    sections: [AtomicU8; SECTIONS],
    nodes: AtomicUsize,
    /// Nodes in fallback order, per node: nearest first.
    fallback: [[AtomicU8; MAX_NODES]; MAX_NODES],
    distance: [[AtomicU8; MAX_NODES]; MAX_NODES],
    owner_pages: [AtomicU64; Owner::ALL.len()],
    failures: AtomicU64,
    reclaim: Once<fn()>,
    oom: Once<fn(u32) -> bool>,
    oom_calls: AtomicU64,
    /// Whether any `Normal` zone has memory.
    has_normal: AtomicBool,
    cpus: [CpuFrames; MAX_CPUS],
    /// The frames retired, in order: the first `retired_len` are set, and
    /// none is ever removed. Read without a lock: a frame is listed before
    /// its zone's lock or a CPU's cache lock is taken to look for it.
    retired: [AtomicU64; MAX_RETIRED],
    retired_len: AtomicUsize,
    /// Held to list a frame, with how many are listed.
    retiring: RawSpinLock<usize>,
}

impl Frames {
    /// An allocator without memory. It hands out nothing until the
    /// architecture adds regions, releases free memory and calls
    /// [`Frames::start`].
    pub const fn new() -> Frames {
        Frames {
            zones: [const { Zone::new() }; MAX_NODES * KINDS],
            platform: Once::new(),
            building: SpinLock::new(RegionTable::new()),
            regions: Once::new(),
            sections: [const { AtomicU8::new(0) }; SECTIONS],
            nodes: AtomicUsize::new(1),
            fallback: [const { [const { AtomicU8::new(0) }; MAX_NODES] }; MAX_NODES],
            distance: [const { [const { AtomicU8::new(10) }; MAX_NODES] }; MAX_NODES],
            owner_pages: [const { AtomicU64::new(0) }; Owner::ALL.len()],
            failures: AtomicU64::new(0),
            reclaim: Once::new(),
            oom: Once::new(),
            oom_calls: AtomicU64::new(0),
            has_normal: AtomicBool::new(false),
            cpus: [const { CpuFrames::new() }; MAX_CPUS],
            retired: [const { AtomicU64::new(0) }; MAX_RETIRED],
            retired_len: AtomicUsize::new(0),
            retiring: RawSpinLock::new(0),
        }
    }

    /// Registers the machine. Call before [`Frames::start`].
    pub fn set_platform(&self, platform: &'static dyn Platform) {
        let _ = self.platform.set(platform);
    }

    /// The machine, once registered.
    pub fn platform(&self) -> Option<&'static dyn Platform> {
        self.platform.get().copied()
    }

    /// Sets the number of nodes and their distances (ACPI SLIT values: 10
    /// for a node to itself, larger for farther nodes). Without a call,
    /// the machine has one node.
    pub fn set_topology(&self, nodes: usize, distance: impl Fn(usize, usize) -> u8) {
        let nodes = nodes.clamp(1, MAX_NODES);
        self.nodes.store(nodes, Ordering::Relaxed);
        for (from, row) in self.distance.iter().enumerate().take(nodes) {
            for (to, slot) in row.iter().enumerate().take(nodes) {
                slot.store(distance(from, to), Ordering::Relaxed);
            }
            let mut order = [0u8; MAX_NODES];
            for (slot, node) in order.iter_mut().zip(0u8..) {
                *slot = node;
            }
            // Nearest first; the node itself first among equals.
            order[..nodes].sort_unstable_by_key(|&to| {
                (distance(from, usize::from(to)), usize::from(to) != from, to)
            });
            for (slot, &node) in self.fallback[from].iter().zip(&order) {
                slot.store(node, Ordering::Relaxed);
            }
        }
    }

    /// Number of NUMA nodes.
    pub fn nodes(&self) -> usize {
        self.nodes.load(Ordering::Relaxed)
    }

    /// Distance from node `from` to node `to`, as ACPI reports it.
    pub fn distance(&self, from: usize, to: usize) -> u8 {
        self.distance
            .get(from)
            .and_then(|row| row.get(to))
            .map_or(255, |d| d.load(Ordering::Relaxed))
    }

    /// Adds a region of RAM on `node`, described by one record per frame.
    /// It must not cross 4 GiB; its frames stay reserved until
    /// [`Frames::release`].
    pub fn add_region(&self, node: usize, region: Region<'static>) -> Result<(), SetupError> {
        if self.regions.get().is_some() {
            return Err(SetupError::AlreadyRunning);
        }
        let kind = ZoneKind::of(region.base);
        if node >= MAX_NODES || region.pages.is_empty() || ZoneKind::of(region.end() - 1) != kind {
            return Err(SetupError::InvalidRegion);
        }
        let zone = node * KINDS + kind.index();
        let mut table = self.building.lock();
        if table.iter().any(|e| e.region.base < region.end() && region.base < e.region.end()) {
            return Err(SetupError::InvalidRegion);
        }
        if table.len == MAX_REGIONS {
            return Err(SetupError::TooManyRegions);
        }
        self.zones[zone].buddy.lock().add_region(region)?;
        let at = table.iter().take_while(|e| e.region.base < region.base).count();
        let len = table.len;
        table.entries.copy_within(at..len, at + 1);
        table.entries[at] = Some(RegionEntry { region, zone });
        table.len += 1;
        Ok(())
    }

    /// Gives the free RAM in `start..end` (physical addresses) to the
    /// zones. Returns how many frames they took.
    pub fn release(&self, start: u64, end: u64) -> u64 {
        let (first, last) = (start.div_ceil(PAGE_SIZE as u64), end >> PAGE_SHIFT);
        let table = self.building.lock();
        let mut released = 0;
        for entry in table.iter() {
            let (from, to) = (first.max(entry.region.base), last.min(entry.region.end()));
            if from < to {
                released += self.zones[entry.zone].buddy.lock().release(from, to);
            }
        }
        released
    }

    /// Finishes setting up: fixes the region table and the zones'
    /// reserves. Allocations work from here on.
    pub fn start(&self) {
        let table = self.building.lock();
        let mut copy = RegionTable::new();
        copy.entries = table.entries;
        copy.len = table.len;
        drop(table);
        let frames = |index: usize| copy.entries[index].map_or(0, |e| e.region.pages.len());
        copy.largest = (0..copy.len).max_by_key(|&index| frames(index)).unwrap_or(0);
        // The lowest entry with frames in a section is written last.
        for (index, entry) in copy.entries[..copy.len].iter().enumerate().rev() {
            let Some(entry) = entry else { continue };
            let first = usize::try_from(entry.region.base >> SECTION_SHIFT).unwrap_or(usize::MAX);
            let last = usize::try_from((entry.region.end() - 1) >> SECTION_SHIFT);
            let number = (index + 1) as u8;
            for section in
                self.sections.iter().take(last.map_or(SECTIONS, |last| last + 1)).skip(first)
            {
                section.store(number, Ordering::Relaxed);
            }
        }
        let normal = (0..MAX_NODES).any(|node| {
            self.zones[node * KINDS + ZoneKind::Normal.index()].buddy.lock().managed_pages() > 0
        });
        self.has_normal.store(normal, Ordering::Relaxed);
        for (index, zone) in self.zones.iter().enumerate() {
            let managed = zone.buddy.lock().managed_pages();
            zone.reserve.store((managed / 128).clamp(16, 1024).min(managed), Ordering::Relaxed);
            let dma32 = index % KINDS == ZoneKind::Dma32.index();
            let protect = if dma32 && normal { (managed / 16).min(16384) } else { 0 };
            zone.protect.store(protect, Ordering::Relaxed);
        }
        let _ = self.regions.set(copy);
    }

    /// Whether [`Frames::start`] has run.
    pub fn is_running(&self) -> bool {
        self.regions.get().is_some()
    }

    /// Registers the function a `MaySleep` allocation calls to free cached
    /// memory before it fails.
    pub fn set_reclaim(&self, reclaim: fn()) {
        let _ = self.reclaim.set(reclaim);
    }

    /// Registers the function a `MaySleep` allocation of at most
    /// [`OOM_MAX_ORDER`] calls, with the order, once reclaim has not helped:
    /// it returns whether memory was freed and the allocation should try
    /// again. It must work out for itself whether its caller can wait.
    pub fn set_oom(&self, oom: fn(u32) -> bool) {
        let _ = self.oom.set(oom);
    }

    /// How many times the out-of-memory hook has run.
    pub fn oom_calls(&self) -> u64 {
        self.oom_calls.load(Ordering::Relaxed)
    }

    /// Allocates a block of 2^`order` frames for `owner` and returns its
    /// physical address.
    pub fn alloc(&self, order: u32, flags: Flags, owner: Owner) -> Result<u64, AllocError> {
        self.alloc_tagged(order, flags, owner, 0)
    }

    /// Like [`Frames::alloc`], storing `tag` in the block's first record
    /// for the owner (the heap keeps a slab's size class there).
    pub fn alloc_tagged(
        &self,
        order: u32,
        flags: Flags,
        owner: Owner,
        tag: u8,
    ) -> Result<u64, AllocError> {
        self.alloc_inner(order, flags, owner, tag, true, false)
    }

    /// Like an order-0 [`Frames::alloc`], for memory filled in order: the
    /// next frame of this CPU's run, a block of [`RUN_FRAMES`] contiguous
    /// frames it takes from its zone when it has handed out the last, so
    /// that frames allocated one after another this way follow each other
    /// in memory. Any frame does if no such block is free.
    pub fn alloc_sequential(&self, flags: Flags, owner: Owner) -> Result<u64, AllocError> {
        self.alloc_inner(0, flags, owner, 0, true, true)
    }

    /// Like an order-0 [`Frames::alloc`] (or [`Frames::alloc_sequential`]
    /// if `sequential`) that fails, without running the reclaim or
    /// out-of-memory hooks, when no frame is free above the reserves: for
    /// a caller holding a lock that reclaiming may need, which lets go of
    /// it and has memory reclaimed before it tries again.
    pub fn alloc_unreclaimed(
        &self,
        flags: Flags,
        owner: Owner,
        sequential: bool,
    ) -> Result<u64, AllocError> {
        self.alloc_inner(0, flags, owner, 0, false, sequential)
    }

    /// The allocation itself. Without `reclaim`, a `MaySleep` allocation
    /// keeps out of the reserves but does not run the reclaim hook: the
    /// heap allocates its own bookkeeping this way, so reclaim never runs
    /// inside the heap. With `sequential`, a single frame comes from the
    /// CPU's run if it can.
    pub(crate) fn alloc_inner(
        &self,
        order: u32,
        flags: Flags,
        owner: Owner,
        tag: u8,
        reclaim: bool,
        sequential: bool,
    ) -> Result<u64, AllocError> {
        if order > MAX_ORDER || self.regions.get().is_none() {
            return Err(AllocError);
        }
        let platform = self.platform();
        if flags.zero && platform.is_none() {
            return Err(AllocError);
        }
        let origin = match flags.node {
            NodePolicy::Local => platform.map_or(0, |p| p.cpu_node(p.current_cpu())),
            NodePolicy::Prefer(node) | NodePolicy::Only(node) => usize::from(node),
        }
        .min(self.nodes() - 1);
        // Empty the CPUs' caches once, reclaim once, then the out-of-memory
        // hook up to OOM_ATTEMPTS times, each followed by another try: a
        // hook that freed memory is never asked again before that memory
        // has been tried.
        let (mut drained, mut reclaimed, mut oom_calls) = (false, false, 0);
        loop {
            let cached = match platform {
                Some(platform) if order == 0 && flags.zone == ZoneLimit::Any => sequential
                    .then(|| self.take_run(platform, origin, owner, tag))
                    .flatten()
                    .or_else(|| self.take_cached(platform, origin, owner, tag)),
                _ => None,
            };
            if let Some(pfn) = cached.or_else(|| self.try_zones(origin, order, flags, owner, tag)) {
                self.owner_pages[owner as usize].fetch_add(1 << order, Ordering::Relaxed);
                let phys = pfn << PAGE_SHIFT;
                if let (true, Some(platform)) = (flags.zero, platform) {
                    // SAFETY: the block was just taken from a free list, so
                    // nothing else uses it, and the platform's direct map
                    // maps every managed frame writable (`Platform`).
                    #[allow(unsafe_code)]
                    unsafe {
                        crate::raw::zero(platform.direct_map() + phys as usize, PAGE_SIZE << order);
                    };
                }
                return Ok(phys);
            }
            if !drained && self.zones.iter().any(|z| z.cached.load(Ordering::Relaxed) > 0) {
                drained = true;
                self.drain_caches();
                continue;
            }
            if flags.context != Context::MaySleep || !reclaim {
                break;
            }
            if !reclaimed && let Some(hook) = self.reclaim.get() {
                reclaimed = true;
                hook();
                continue;
            }
            match self.oom.get() {
                Some(oom) if order <= OOM_MAX_ORDER && oom_calls < OOM_ATTEMPTS => {
                    oom_calls += 1;
                    self.oom_calls.fetch_add(1, Ordering::Relaxed);
                    if !oom(order) {
                        break;
                    }
                }
                _ => break,
            }
        }
        self.failures.fetch_add(1, Ordering::Relaxed);
        Err(AllocError)
    }

    /// Tries each zone in fallback order.
    fn try_zones(
        &self,
        origin: usize,
        order: u32,
        flags: Flags,
        owner: Owner,
        tag: u8,
    ) -> Option<u64> {
        let nodes = if let NodePolicy::Only(_) = flags.node { 1 } else { self.nodes() };
        let kinds: &[ZoneKind] = match flags.zone {
            ZoneLimit::Any => &[ZoneKind::Normal, ZoneKind::Dma32],
            ZoneLimit::Dma32 => &[ZoneKind::Dma32],
        };
        for rank in 0..nodes {
            let node = usize::from(self.fallback[origin][rank].load(Ordering::Relaxed));
            for &kind in kinds {
                let zone = &self.zones[node * KINDS + kind.index()];
                let mut floor = match flags.context {
                    Context::Atomic => 0,
                    Context::MaySleep => zone.reserve.load(Ordering::Relaxed),
                };
                if flags.zone == ZoneLimit::Any {
                    floor += zone.protect.load(Ordering::Relaxed);
                }
                let _irqs = NoInterrupts::new(self.platform());
                let mut buddy = zone.buddy.lock();
                if buddy.free_pages() < floor + (1 << order) {
                    continue;
                }
                if let Some(pfn) = self.alloc_sound(&mut buddy, order, owner, tag) {
                    return Some(pfn);
                }
            }
        }
        None
    }

    /// A frame from this CPU's cache, which takes a batch from its zone
    /// first if it is empty, for an allocation from node `origin`.
    fn take_cached(
        &self,
        platform: &dyn Platform,
        origin: usize,
        owner: Owner,
        tag: u8,
    ) -> Option<u64> {
        let _irqs = NoInterrupts::new(Some(platform));
        let mut cache = self.cpus.get(platform.current_cpu())?.cache.lock();
        let zone = self.cache_zone(&mut cache, platform)?;
        if zone / KINDS != origin {
            return None;
        }
        if cache.len == 0 {
            self.refill(&mut cache, zone);
        }
        let pfn = u64::from(cache.head);
        let info = self.record(pfn).filter(|_| cache.head != NIL)?;
        cache.head = info.next();
        match self.record(u64::from(cache.head)).filter(|_| cache.head != NIL) {
            Some(next) => next.set_prev(NIL),
            None => cache.tail = NIL,
        }
        cache.len -= 1;
        self.zones[zone].cached.fetch_sub(1, Ordering::Relaxed);
        info.set(State::Allocated, 0, owner, tag);
        Some(pfn)
    }

    /// The next frame of this CPU's run, which takes a free block of
    /// [`RUN_ORDER`] from its zone first if it is used up, for an
    /// allocation from node `origin`; `None` if no such block is free
    /// above the zone's floors.
    fn take_run(
        &self,
        platform: &dyn Platform,
        origin: usize,
        owner: Owner,
        tag: u8,
    ) -> Option<u64> {
        let _irqs = NoInterrupts::new(Some(platform));
        let mut cache = self.cpus.get(platform.current_cpu())?.cache.lock();
        let zone = self.cache_zone(&mut cache, platform)?;
        if zone / KINDS != origin {
            return None;
        }
        if cache.run == cache.run_end {
            let block = {
                let zone = &self.zones[zone];
                let floor =
                    zone.reserve.load(Ordering::Relaxed) + zone.protect.load(Ordering::Relaxed);
                let mut buddy = zone.buddy.lock();
                if buddy.free_pages() < floor + u64::from(RUN_FRAMES) {
                    return None;
                }
                self.alloc_sound(&mut buddy, RUN_ORDER, Owner::PerCpu, 0)?
            };
            // The block's frames become single cached frames.
            for pfn in block..block + u64::from(RUN_FRAMES) {
                if let Some(info) = self.record(pfn) {
                    info.set(State::Cached, 0, Owner::PerCpu, 0);
                }
            }
            (cache.run, cache.run_end) = (block as u32, (block + u64::from(RUN_FRAMES)) as u32);
            self.zones[zone].cached.fetch_add(u64::from(RUN_FRAMES), Ordering::Relaxed);
        }
        let pfn = u64::from(cache.run);
        let info = self.record(pfn)?;
        cache.run += 1;
        self.zones[zone].cached.fetch_sub(1, Ordering::Relaxed);
        info.set(State::Allocated, 0, owner, tag);
        Some(pfn)
    }

    /// The zone of a CPU's cache, chosen when the CPU first uses it: its
    /// node's `Normal` zone, or its `Dma32` zone if that is all the node
    /// has, as a local allocation tries first.
    fn cache_zone(&self, cache: &mut Cached, platform: &dyn Platform) -> Option<usize> {
        if let Some(zone) = cache.zone {
            return zone;
        }
        let node = platform.cpu_node(platform.current_cpu()).min(self.nodes() - 1);
        let zone = [ZoneKind::Normal, ZoneKind::Dma32]
            .into_iter()
            .map(|kind| node * KINDS + kind.index())
            .find(|&zone| self.zones[zone].buddy.lock().managed_pages() > 0);
        if let Some(zone) = zone {
            let managed = self.zones[zone].buddy.lock().managed_pages();
            let limit = (managed / 512).clamp(2 * u64::from(CACHE_BATCH), u64::from(CACHE_MOST));
            cache.limit = limit as u32;
        }
        cache.zone = Some(zone);
        zone
    }

    /// Takes up to [`CACHE_BATCH`] frames from `zone` into an empty cache,
    /// none below the zone's floors.
    fn refill(&self, cache: &mut Cached, zone: usize) {
        let zone = &self.zones[zone];
        let floor = zone.reserve.load(Ordering::Relaxed) + zone.protect.load(Ordering::Relaxed);
        let mut buddy = zone.buddy.lock();
        let room = buddy.free_pages().saturating_sub(floor).min(u64::from(CACHE_BATCH));
        for _ in 0..room {
            let Some(pfn) = self.alloc_sound(&mut buddy, 0, Owner::PerCpu, 0) else { break };
            let Some(info) = self.record(pfn) else { break };
            info.set_state(State::Cached);
            self.push(cache, pfn, info);
            zone.cached.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Puts a cached frame at the head of a cache's list.
    fn push(&self, cache: &mut Cached, pfn: u64, info: &PageInfo) {
        info.set_prev(NIL);
        info.set_next(cache.head);
        match self.record(u64::from(cache.head)).filter(|_| cache.head != NIL) {
            Some(head) => head.set_prev(pfn as u32),
            None => cache.tail = pfn as u32,
        }
        cache.head = pfn as u32;
        cache.len += 1;
    }

    /// Gives up to `count` frames from the tail of a cache, the ones freed
    /// longest ago, back to its zone.
    fn give_back(&self, cache: &mut Cached, count: u32) {
        let Some(Some(zone)) = cache.zone else { return };
        let zone = &self.zones[zone];
        let mut buddy = zone.buddy.lock();
        for _ in 0..count {
            let pfn = u64::from(cache.tail);
            let Some(info) = self.record(pfn).filter(|_| cache.tail != NIL) else { break };
            cache.tail = info.prev();
            match self.record(u64::from(cache.tail)).filter(|_| cache.tail != NIL) {
                Some(tail) => tail.set_next(NIL),
                None => cache.head = NIL,
            }
            cache.len -= 1;
            zone.cached.fetch_sub(1, Ordering::Relaxed);
            info.set(State::Allocated, 0, Owner::PerCpu, 0);
            // Cannot fail: the frame was just marked allocated.
            let _ = buddy.free_failed(pfn, 0, Owner::PerCpu, |f| self.is_retired(f));
        }
    }

    /// Puts the frame `pfn` of `zone`, which `owner` allocated alone, in
    /// this CPU's cache if it caches that zone; false if it does not, or if
    /// the frame is not such an allocated frame (which the zone then
    /// reports).
    fn free_cached(&self, platform: &dyn Platform, zone: usize, pfn: u64, owner: Owner) -> bool {
        let _irqs = NoInterrupts::new(Some(platform));
        let Some(cpu) = self.cpus.get(platform.current_cpu()) else { return false };
        let mut cache = cpu.cache.lock();
        if self.cache_zone(&mut cache, platform) != Some(zone) {
            return false;
        }
        let Some(info) = self.record(pfn) else { return false };
        // A retired frame goes to its zone, which withholds it.
        if self.is_retired(pfn) || !info.cache(owner) {
            return false;
        }
        self.push(&mut cache, pfn, info);
        self.zones[zone].cached.fetch_add(1, Ordering::Relaxed);
        if cache.len > cache.limit {
            self.give_back(&mut cache, CACHE_BATCH);
        }
        true
    }

    /// Gives every frame the CPUs' caches hold back to the zones, their
    /// runs' too.
    pub fn drain_caches(&self) {
        for cpu in &self.cpus {
            let _irqs = NoInterrupts::new(self.platform());
            let mut cache = cpu.cache.lock();
            let len = cache.len;
            self.give_back(&mut cache, len);
            let Some(Some(zone)) = cache.zone else { continue };
            let zone = &self.zones[zone];
            let mut buddy = zone.buddy.lock();
            for pfn in u64::from(cache.run)..u64::from(cache.run_end) {
                let Some(info) = self.record(pfn) else { continue };
                info.set(State::Allocated, 0, Owner::PerCpu, 0);
                // Cannot fail: the frame was just marked allocated.
                let _ = buddy.free_failed(pfn, 0, Owner::PerCpu, |f| self.is_retired(f));
                zone.cached.fetch_sub(1, Ordering::Relaxed);
            }
            cache.run = cache.run_end;
        }
    }

    /// The entry of the region containing frame `pfn`: the largest
    /// region's, which holds most frames, or else one found through the
    /// frame's section, as Linux finds a frame's record. Neither look
    /// writes anything, so the CPUs share the lines they read.
    #[inline]
    fn entry(&self, pfn: u64) -> Option<&RegionEntry> {
        let table = self.regions.get()?;
        if let Some(Some(entry)) = table.entries.get(table.largest)
            && entry.region.contains(pfn)
        {
            return Some(entry);
        }
        match self.sections.get(usize::try_from(pfn >> SECTION_SHIFT).ok()?) {
            Some(first) => {
                let first = usize::from(first.load(Ordering::Relaxed)).checked_sub(1)?;
                table.find_from(first, pfn)
            }
            None => table.search(pfn),
        }
    }

    /// The record of frame `pfn`.
    #[inline]
    fn record(&self, pfn: u64) -> Option<&'static PageInfo> {
        self.entry(pfn)?.region.info(pfn)
    }

    /// Frees the block of 2^`order` frames at physical address `phys`,
    /// which `owner` allocated.
    pub fn free(&self, phys: u64, order: u32, owner: Owner) -> Result<(), FreeError> {
        let pfn = phys >> PAGE_SHIFT;
        if !phys.is_multiple_of(PAGE_SIZE as u64) {
            return Err(FreeError::Misaligned);
        }
        let entry = self.entry(pfn).ok_or(FreeError::NotManaged)?;
        if order == 0
            && let Some(platform) = self.platform()
            && self.free_cached(platform, entry.zone, pfn, owner)
        {
            self.owner_pages[owner as usize].fetch_sub(1, Ordering::Relaxed);
            return Ok(());
        }
        let _irqs = NoInterrupts::new(self.platform());
        let mut buddy = self.zones[entry.zone].buddy.lock();
        if self.any_retired(pfn, order) {
            buddy.free_failed(pfn, order, owner, |f| self.is_retired(f))?;
        } else {
            buddy.free(pfn, order, owner)?;
        }
        drop(buddy);
        self.owner_pages[owner as usize].fetch_sub(1 << order, Ordering::Relaxed);
        Ok(())
    }

    /// Takes the frame at physical address `phys`, whose memory went bad,
    /// out of use for good: out of its zone now if it is free, a CPU's
    /// cache included, or else when its block is freed. Takes the zones'
    /// and the caches' locks, so not under either.
    pub fn retire(&self, phys: u64) -> Retired {
        let pfn = phys >> PAGE_SHIFT;
        let Some(entry) = self.entry(pfn) else { return Retired::NotManaged };
        if !self.list(pfn) {
            return Retired::TooMany;
        }
        // A frame in a CPU's cache goes back to its zone, which withholds
        // it; one taken into a cache again meanwhile is looked for again.
        for _ in 0..RETIRE_ATTEMPTS {
            self.drain_caches();
            let _irqs = NoInterrupts::new(self.platform());
            let mut buddy = self.zones[entry.zone].buddy.lock();
            if buddy.take(pfn) {
                return Retired::Now;
            }
            match self.record(pfn).map(PageInfo::state) {
                Some(State::Reserved) => return Retired::Now,
                Some(State::Cached) => {}
                _ => return Retired::WhenFreed,
            }
        }
        Retired::WhenFreed
    }

    /// Lists frame `pfn` as retired, if it is not yet; false if the list is
    /// full.
    fn list(&self, pfn: u64) -> bool {
        let _irqs = NoInterrupts::new(self.platform());
        let mut len = self.retiring.lock();
        if self.retired[..*len].iter().any(|f| f.load(Ordering::Relaxed) == pfn) {
            return true;
        }
        let Some(slot) = self.retired.get(*len) else { return false };
        slot.store(pfn, Ordering::Relaxed);
        *len += 1;
        self.retired_len.store(*len, Ordering::Release);
        true
    }

    /// Frames retired, in use or not: Linux's `HardwareCorrupted`.
    pub fn retired_pages(&self) -> u64 {
        self.retired_len.load(Ordering::Acquire) as u64
    }

    /// Whether frame `pfn` is retired.
    #[inline]
    fn is_retired(&self, pfn: u64) -> bool {
        self.any_retired(pfn, 0)
    }

    /// Whether any frame of the block of `order` at `pfn` is retired: one
    /// load when none is, as on a machine whose memory is sound.
    #[inline]
    fn any_retired(&self, pfn: u64, order: u32) -> bool {
        let len = self.retired_len.load(Ordering::Acquire);
        len > 0
            && self.retired[..len]
                .iter()
                .any(|f| (pfn..pfn + (1 << order)).contains(&f.load(Ordering::Relaxed)))
    }

    /// Takes a block from `buddy` as [`Buddy::alloc`] does, but never one
    /// holding a retired frame: such a block goes back without it, for
    /// good, and another is taken.
    fn alloc_sound(
        &self,
        buddy: &mut Buddy<'static>,
        order: u32,
        owner: Owner,
        tag: u8,
    ) -> Option<u64> {
        loop {
            let pfn = buddy.alloc(order, owner, tag)?;
            if !self.any_retired(pfn, order) {
                return Some(pfn);
            }
            // Cannot fail: the block was just allocated. It loses a frame
            // for good, so the loop ends.
            let _ = buddy.free_failed(pfn, order, owner, |f| self.is_retired(f));
        }
    }

    /// The record of the frame at physical address `phys`.
    pub fn info(&self, phys: u64) -> Option<&'static PageInfo> {
        self.record(phys >> PAGE_SHIFT)
    }

    /// The node whose memory holds physical address `phys`.
    pub fn node_of(&self, phys: u64) -> Option<usize> {
        Some(self.entry(phys >> PAGE_SHIFT)?.zone / KINDS)
    }

    /// Free frames in all zones, those CPUs' caches hold included.
    pub fn free_pages(&self) -> u64 {
        self.zones
            .iter()
            .map(|zone| zone.buddy.lock().free_pages() + zone.cached.load(Ordering::Relaxed))
            .sum()
    }

    /// Frames all zones manage.
    pub fn managed_pages(&self) -> u64 {
        self.zones.iter().map(|zone| zone.buddy.lock().managed_pages()).sum()
    }

    /// Frames allocated to `owner`.
    pub fn owner_pages(&self, owner: Owner) -> u64 {
        self.owner_pages[owner as usize].load(Ordering::Relaxed)
    }

    /// Allocations that failed.
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// Calls `f` with a snapshot of every zone that manages memory.
    pub fn zone_stats(&self, mut f: impl FnMut(&ZoneStats)) {
        for (index, zone) in self.zones.iter().enumerate() {
            let stats = {
                let _irqs = NoInterrupts::new(self.platform());
                let buddy = zone.buddy.lock();
                if buddy.managed_pages() == 0 {
                    continue;
                }
                let mut blocks = [0; ORDERS];
                for (order, count) in blocks.iter_mut().enumerate() {
                    *count = buddy.free_blocks(order as u32);
                }
                let span = buddy.regions().fold((u64::MAX, 0), |(low, high), r| {
                    (low.min(r.base << PAGE_SHIFT), high.max(r.end() << PAGE_SHIFT))
                });
                ZoneStats {
                    node: index / KINDS,
                    kind: if index % KINDS == 0 { ZoneKind::Dma32 } else { ZoneKind::Normal },
                    managed: buddy.managed_pages(),
                    free: buddy.free_pages() + zone.cached.load(Ordering::Relaxed),
                    reserve: zone.reserve.load(Ordering::Relaxed),
                    blocks,
                    span,
                }
            };
            f(&stats);
        }
    }

    /// Checks every zone's free lists ([`Buddy::check`]) and every CPU's
    /// cache: it holds as many frames as it counts, linked both ways, each
    /// a cached frame of its zone, as is each frame left in its run.
    pub fn check(&self) -> Result<(), &'static str> {
        for zone in &self.zones {
            let _irqs = NoInterrupts::new(self.platform());
            zone.buddy.lock().check()?;
        }
        for cpu in &self.cpus {
            let _irqs = NoInterrupts::new(self.platform());
            let cache = cpu.cache.lock();
            let (mut pfn, mut prev, mut len) = (cache.head, NIL, 0);
            while pfn != NIL {
                if len == cache.len {
                    return Err("a CPU's cache holds more frames than it counts (a cycle?)");
                }
                let entry = self.entry(u64::from(pfn));
                let info = self.record(u64::from(pfn)).ok_or("a CPU's cache names no frame")?;
                if info.state() != State::Cached || info.order() != 0 {
                    return Err("a frame in a CPU's cache is not a cached frame");
                }
                if info.prev() != prev {
                    return Err("a CPU's cache's back link is wrong");
                }
                if entry.map(|e| e.zone) != cache.zone.flatten() {
                    return Err("a CPU's cache holds a frame of another zone");
                }
                (prev, pfn, len) = (pfn, info.next(), len + 1);
            }
            if len != cache.len || cache.tail != prev {
                return Err("a CPU's cache's count or tail is wrong");
            }
            if cache.run > cache.run_end || cache.run_end - cache.run > RUN_FRAMES {
                return Err("a CPU's run is out of bounds");
            }
            for pfn in u64::from(cache.run)..u64::from(cache.run_end) {
                let entry = self.entry(pfn);
                let info = self.record(pfn).ok_or("a CPU's run names no frame")?;
                if info.state() != State::Cached || info.order() != 0 {
                    return Err("a frame of a CPU's run is not a cached frame");
                }
                if entry.map(|e| e.zone) != cache.zone.flatten() {
                    return Err("a CPU's run holds a frame of another zone");
                }
            }
        }
        Ok(())
    }
}

impl Default for Frames {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{HostPlatform, records};

    fn frames_with(nodes: &[(usize, u64, usize)]) -> &'static Frames {
        let frames: &'static Frames = Box::leak(Box::new(Frames::new()));
        frames.set_platform(HostPlatform::leak(2, 0));
        let count = nodes.iter().map(|n| n.0).max().unwrap_or(0) + 1;
        frames.set_topology(count, |a, b| if a == b { 10 } else { 20 });
        for &(node, base, pages) in nodes {
            frames.add_region(node, Region { base, pages: records(pages) }).unwrap();
            frames.release(base << PAGE_SHIFT, (base + pages as u64) << PAGE_SHIFT);
        }
        frames.start();
        frames
    }

    #[test]
    fn frames_are_found_in_every_region() {
        let frames = frames_with(&[(0, 64, 32), (0, 256, 4096), (0, 8192, 16)]);
        for (base, pages) in [(64u64, 32u64), (256, 4096), (8192, 16)] {
            for pfn in [base, base + pages / 2, base + pages - 1] {
                let info = frames.info(pfn << PAGE_SHIFT).expect("a frame of a region");
                assert!(core::ptr::eq(info, frames.info(pfn << PAGE_SHIFT).unwrap()));
                assert_eq!(frames.node_of(pfn << PAGE_SHIFT), Some(0));
            }
        }
        // Below, between and above them.
        for pfn in [0, 63, 96, 255, 4352, 8191, 8208, u64::MAX >> PAGE_SHIFT] {
            assert!(frames.info(pfn << PAGE_SHIFT).is_none(), "frame {pfn}");
        }
    }

    #[test]
    fn frames_are_found_through_their_sections() {
        // Three regions in the first section, the last of them reaching
        // into the second, one above 4 GiB and one above the sections the
        // map covers.
        let above = (SECTIONS as u64) << SECTION_SHIFT;
        let regions =
            [(64u64, 32u64), (256, 4096), (32760, 16), (DMA32_END_PFN + 8, 64), (above + 8, 16)];
        let frames = frames_with(&regions.map(|(base, pages)| (0, base, pages as usize)));
        for (base, pages) in regions {
            for pfn in [base, base + pages / 2, base + pages - 1] {
                let entry = frames.entry(pfn).expect("a frame of a region");
                assert_eq!(entry.region.base, base, "frame {pfn}");
                let info = frames.info(pfn << PAGE_SHIFT).unwrap();
                assert!(core::ptr::eq(info, &raw const entry.region.pages[(pfn - base) as usize]));
            }
            for pfn in [base - 1, base + pages] {
                assert!(frames.entry(pfn).is_none(), "frame {pfn}");
            }
        }
    }

    #[test]
    fn prefers_the_local_node_and_falls_back() {
        // Node 0 below 4 GiB, node 1 above it.
        let frames = frames_with(&[(0, 256, 2048), (1, DMA32_END_PFN, 2048)]);
        let local = frames.alloc(0, Flags::ATOMIC, Owner::Kernel).unwrap();
        assert_eq!(frames.node_of(local), Some(0));
        let remote = frames.alloc(0, Flags::ATOMIC.prefer_node(1), Owner::Kernel).unwrap();
        assert_eq!(frames.node_of(remote), Some(1));
        assert!(remote >= 1 << 32);
        let dma = frames.alloc(0, Flags::ATOMIC.prefer_node(1).dma32(), Owner::Kernel).unwrap();
        assert!(dma < 1 << 32);
        assert!(frames.alloc(0, Flags::ATOMIC.only_node(1).dma32(), Owner::Kernel).is_err());
        assert_eq!(frames.owner_pages(Owner::Kernel), 3);
        for phys in [local, remote, dma] {
            frames.free(phys, 0, Owner::Kernel).unwrap();
        }
        assert_eq!(frames.owner_pages(Owner::Kernel), 0);
        frames.check().unwrap();
    }

    #[test]
    fn a_cpu_hands_out_the_frame_it_freed_last() {
        let frames = frames_with(&[(0, 256, 2048)]);
        let first = frames.alloc(0, Flags::MAY_SLEEP, Owner::Kernel).unwrap();
        let second = frames.alloc(0, Flags::MAY_SLEEP, Owner::Anon).unwrap();
        frames.free(first, 0, Owner::Kernel).unwrap();
        assert_eq!(frames.free_pages(), 2047);
        assert_eq!(frames.owner_pages(Owner::Kernel), 0);
        assert_eq!(frames.alloc(0, Flags::MAY_SLEEP, Owner::Kernel), Ok(first));
        // Frees the cache would take are checked as any other.
        frames.free(first, 0, Owner::Kernel).unwrap();
        assert_eq!(frames.free(first, 0, Owner::Kernel), Err(FreeError::AlreadyFree));
        assert_eq!(
            frames.free(second, 0, Owner::Kernel),
            Err(FreeError::WrongOwner { owner: Owner::Anon })
        );
        frames.free(second, 0, Owner::Anon).unwrap();
        frames.check().unwrap();
        // Blocks of more than one frame skip it.
        let block = frames.alloc(1, Flags::MAY_SLEEP, Owner::Kernel).unwrap();
        assert!(block != first && block != second);
        frames.free(block, 1, Owner::Kernel).unwrap();
        assert_eq!(frames.free_pages(), 2048);
        frames.check().unwrap();
    }

    #[test]
    fn frames_allocated_in_order_follow_each_other() {
        let frames = frames_with(&[(0, 256, 2048)]);
        // Frames freed in a jumble fill the cache, which ordinary
        // allocations take from first.
        let jumble: Vec<u64> =
            (0..64).map(|_| frames.alloc(0, Flags::MAY_SLEEP, Owner::Kernel).unwrap()).collect();
        for &phys in jumble.iter().step_by(2).chain(jumble.iter().skip(1).step_by(2)) {
            frames.free(phys, 0, Owner::Kernel).unwrap();
        }
        let run = RUN_FRAMES as usize;
        let taken: Vec<u64> = (0..2 * run + run / 2)
            .map(|_| frames.alloc_sequential(Flags::MAY_SLEEP, Owner::Anon).unwrap())
            .collect();
        for block in taken.chunks(run) {
            assert!(
                block.windows(2).all(|pair| pair[1] == pair[0] + PAGE_SIZE as u64),
                "{block:x?}"
            );
        }
        assert!(taken.iter().all(|phys| !jumble.contains(phys)));
        assert_eq!(frames.owner_pages(Owner::Anon), taken.len() as u64);
        assert_eq!(frames.free_pages(), 2048 - taken.len() as u64);
        frames.check().unwrap();
        // The rest of the last run goes back with the cache.
        for &phys in &taken {
            frames.free(phys, 0, Owner::Anon).unwrap();
        }
        frames.drain_caches();
        assert_eq!(frames.free_pages(), 2048);
        assert_eq!(frames.zones[0].cached.load(Ordering::Relaxed), 0);
        frames.check().unwrap();
        // Without a free block for a run, any frame does: every one above
        // the reserve is still handed out.
        let all =
            core::iter::from_fn(|| frames.alloc_sequential(Flags::MAY_SLEEP, Owner::Anon).ok())
                .collect::<Vec<_>>();
        assert_eq!(all.len(), 2048 - 16);
        for phys in all {
            frames.free(phys, 0, Owner::Anon).unwrap();
        }
        frames.drain_caches();
        assert_eq!(frames.free_pages(), 2048);
        frames.check().unwrap();
    }

    #[test]
    fn caches_give_frames_back_past_their_limit_and_before_failing() {
        use crate::tests::become_cpu;
        let frames = frames_with(&[(0, 256, 2048)]);
        let taken: Vec<u64> =
            core::iter::from_fn(|| frames.alloc(0, Flags::MAY_SLEEP, Owner::Kernel).ok()).collect();
        assert_eq!(taken.len(), 2048 - 16);
        // CPU 1 frees half of them: its cache keeps at most 64 (the least
        // a cache may keep), the zone gets the rest back in batches.
        let (by_cpu1, by_cpu0) = taken.split_at(1016);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                become_cpu(1);
                for &phys in by_cpu1 {
                    frames.free(phys, 0, Owner::Kernel).unwrap();
                }
            });
        });
        let cached = frames.zones[0].cached.load(Ordering::Relaxed);
        assert!((1..=64).contains(&cached), "{cached} frames cached");
        assert_eq!(frames.free_pages(), 16 + 1016);
        frames.check().unwrap();
        // CPU 0 gets every one of them: from the zone, then, when that has
        // only its reserve left, from CPU 1's cache, which it empties.
        let again =
            core::iter::from_fn(|| frames.alloc(0, Flags::MAY_SLEEP, Owner::Kernel).ok()).count();
        assert_eq!(again, 1016);
        assert_eq!(frames.zones[0].cached.load(Ordering::Relaxed), 0);
        for &phys in by_cpu0 {
            frames.free(phys, 0, Owner::Kernel).unwrap();
        }
        frames.drain_caches();
        assert_eq!(frames.free_pages(), 16 + 1016);
        assert_eq!(frames.zones[0].cached.load(Ordering::Relaxed), 0);
        frames.check().unwrap();
    }

    #[test]
    fn the_out_of_memory_hook_frees_memory_for_small_blocks() {
        use std::sync::Mutex;
        // The hook gives back one page that a stand-in victim held.
        static HELD: Mutex<Vec<(&'static Frames, u64)>> = Mutex::new(Vec::new());
        fn oom(_order: u32) -> bool {
            match HELD.lock().unwrap().pop() {
                Some((frames, phys)) => frames.free(phys, 0, Owner::Kernel).is_ok(),
                None => false,
            }
        }
        let frames = frames_with(&[(0, 256, 2048)]);
        frames.set_oom(oom);
        let mut taken = Vec::new();
        while let Ok(phys) = frames.alloc(0, Flags::MAY_SLEEP, Owner::Kernel) {
            taken.push(phys);
        }
        let calls = frames.oom_calls();
        assert!(calls >= 1);
        // A victim's page comes back, and the next allocation gets it.
        HELD.lock().unwrap().push((frames, taken.pop().unwrap()));
        let again = frames.alloc(0, Flags::MAY_SLEEP, Owner::Kernel).unwrap();
        assert!(frames.oom_calls() > calls);
        // Large blocks fail without asking.
        let before = frames.oom_calls();
        assert!(frames.alloc(OOM_MAX_ORDER + 1, Flags::MAY_SLEEP, Owner::Kernel).is_err());
        assert_eq!(frames.oom_calls(), before);
        for phys in taken.into_iter().chain([again]) {
            frames.free(phys, 0, Owner::Kernel).unwrap();
        }
        frames.check().unwrap();
    }

    #[test]
    fn the_out_of_memory_hook_is_asked_a_bounded_number_of_times() {
        // A hook that says it freed memory but did not: each call stops a
        // domain for nothing, so an allocation asks at most OOM_ATTEMPTS
        // times, and tries again after each.
        static CALLS: AtomicU64 = AtomicU64::new(0);
        fn oom(_order: u32) -> bool {
            CALLS.fetch_add(1, Ordering::Relaxed);
            true
        }
        let frames = frames_with(&[(0, 256, 2048)]);
        frames.set_oom(oom);
        let mut taken = Vec::new();
        while let Ok(phys) = frames.alloc(0, Flags::MAY_SLEEP, Owner::Kernel) {
            taken.push(phys);
        }
        assert_eq!(CALLS.load(Ordering::Relaxed), u64::from(OOM_ATTEMPTS));
        assert_eq!(frames.oom_calls(), u64::from(OOM_ATTEMPTS));
        for phys in taken {
            frames.free(phys, 0, Owner::Kernel).unwrap();
        }
        frames.check().unwrap();
    }

    #[test]
    fn reserves_are_for_atomic_callers() {
        let frames = frames_with(&[(0, 256, 2048)]);
        let reserve = 16;
        let mut taken = Vec::new();
        while let Ok(phys) = frames.alloc(0, Flags::MAY_SLEEP, Owner::Kernel) {
            taken.push(phys);
        }
        assert_eq!(frames.free_pages(), reserve);
        let atomic = frames.alloc(0, Flags::ATOMIC, Owner::Kernel).unwrap();
        assert_eq!(frames.failures(), 1);
        frames.free(atomic, 0, Owner::Kernel).unwrap();
        for phys in taken {
            frames.free(phys, 0, Owner::Kernel).unwrap();
        }
        assert_eq!(frames.free_pages(), 2048);
    }

    #[test]
    fn dma32_is_kept_for_devices_when_there_is_more() {
        let frames = frames_with(&[(0, 256, 1024), (0, DMA32_END_PFN, 64)]);
        let mut normal = Vec::new();
        while let Ok(phys) = frames.alloc(0, Flags::ATOMIC, Owner::Kernel) {
            normal.push(phys);
        }
        // 64 frames above 4 GiB, and all but 1/16 of the DMA32 zone.
        assert_eq!(normal.len(), 64 + 1024 - 64);
        assert!(frames.alloc(0, Flags::ATOMIC.dma32(), Owner::Kernel).is_ok());
    }

    #[test]
    fn rejects_bad_regions_and_frees() {
        let frames = frames_with(&[(0, 256, 64)]);
        assert_eq!(
            frames.add_region(0, Region { base: 1024, pages: records(4) }),
            Err(SetupError::AlreadyRunning)
        );
        assert_eq!(frames.free(0x1234, 0, Owner::Kernel), Err(FreeError::Misaligned));
        assert_eq!(frames.free(0, 0, Owner::Kernel), Err(FreeError::NotManaged));
        let fresh = Frames::new();
        let crossing = Region { base: DMA32_END_PFN - 1, pages: records(2) };
        assert_eq!(fresh.add_region(0, crossing), Err(SetupError::InvalidRegion));
    }

    #[test]
    fn a_retired_frame_is_never_handed_out_again() {
        let frames = frames_with(&[(0, 256, 2048)]);
        let page = PAGE_SIZE as u64;
        // A free frame leaves at once, from this CPU's cache, from its run
        // or from the zone.
        let cached = frames.alloc(0, Flags::MAY_SLEEP, Owner::Kernel).unwrap();
        frames.free(cached, 0, Owner::Kernel).unwrap();
        assert_eq!(frames.retire(cached), Retired::Now);
        let run = frames.alloc_sequential(Flags::MAY_SLEEP, Owner::Anon).unwrap();
        assert_eq!(frames.retire(run + 3 * page), Retired::Now);
        let last = (256 + 2047) << PAGE_SHIFT;
        assert!(matches!(frames.info(last).unwrap().state(), State::Free | State::Tail));
        assert_eq!(frames.retire(last), Retired::Now);
        // One in use leaves when its block is freed, through the cache or
        // not.
        let block = frames.alloc(2, Flags::MAY_SLEEP, Owner::Kernel).unwrap();
        let single = frames.alloc(0, Flags::MAY_SLEEP, Owner::Anon).unwrap();
        assert_eq!(frames.retire(block + 2 * page), Retired::WhenFreed);
        assert_eq!(frames.retire(single), Retired::WhenFreed);
        assert_eq!(frames.managed_pages(), 2048 - 3);
        frames.free(block, 2, Owner::Kernel).unwrap();
        frames.free(single, 0, Owner::Anon).unwrap();
        // The frame freed last is not this CPU's to hand out again.
        let next = frames.alloc(0, Flags::MAY_SLEEP, Owner::Anon).unwrap();
        assert_ne!(next, single);
        frames.free(next, 0, Owner::Anon).unwrap();
        frames.free(run, 0, Owner::Anon).unwrap();
        assert_eq!(frames.owner_pages(Owner::Kernel) + frames.owner_pages(Owner::Anon), 0);
        // Once each, and only what the allocator manages.
        assert_eq!(frames.retire(cached), Retired::Now);
        assert_eq!(frames.retire(0), Retired::NotManaged);
        assert_eq!(frames.retired_pages(), 5);
        frames.drain_caches();
        assert_eq!(frames.managed_pages(), 2048 - 5);
        assert_eq!(frames.free_pages(), 2048 - 5);
        frames.check().unwrap();
        let retired = [cached, run + 3 * page, last, block + 2 * page, single];
        for order in [0, 2, 5] {
            let all: Vec<u64> =
                core::iter::from_fn(|| frames.alloc(order, Flags::ATOMIC, Owner::Kernel).ok())
                    .collect();
            for &phys in &all {
                for bad in retired {
                    assert!(!(phys..phys + (page << order)).contains(&bad), "{order}: {phys:#x}");
                }
            }
            for phys in all {
                frames.free(phys, order, Owner::Kernel).unwrap();
            }
            frames.drain_caches();
            assert_eq!(frames.free_pages(), 2048 - 5);
            frames.check().unwrap();
        }
    }

    #[test]
    fn a_retired_frame_another_cpu_takes_first_is_withheld_from_it() {
        let frames = frames_with(&[(0, 256, 2048)]);
        // Listed, but taken by no one yet: as when an allocation reaches
        // the zone between the listing and the zone's lock.
        let bad = 256 + 100;
        assert!(frames.list(bad));
        let all: Vec<u64> =
            core::iter::from_fn(|| frames.alloc(0, Flags::ATOMIC, Owner::Kernel).ok()).collect();
        assert_eq!(all.len(), 2047);
        assert!(!all.contains(&(bad << PAGE_SHIFT)));
        for phys in all {
            frames.free(phys, 0, Owner::Kernel).unwrap();
        }
        frames.drain_caches();
        assert_eq!(frames.free_pages(), 2047);
        assert_eq!(frames.managed_pages(), 2047);
        assert_eq!(frames.info(bad << PAGE_SHIFT).unwrap().state(), State::Reserved);
        frames.check().unwrap();
        // So is one a CPU's cache held when it was listed.
        let cached = frames.alloc(0, Flags::MAY_SLEEP, Owner::Kernel).unwrap();
        frames.free(cached, 0, Owner::Kernel).unwrap();
        assert!(frames.list(cached >> PAGE_SHIFT));
        frames.drain_caches();
        assert_eq!(frames.info(cached).unwrap().state(), State::Reserved);
        assert_eq!(frames.free_pages(), 2046);
        frames.check().unwrap();
    }

    #[test]
    fn retiring_stops_at_its_limit() {
        let frames = frames_with(&[(0, 256, 2048)]);
        for frame in 0..MAX_RETIRED as u64 {
            assert_eq!(frames.retire((256 + 2 * frame) << PAGE_SHIFT), Retired::Now);
        }
        assert_eq!(frames.retire((256 + 1) << PAGE_SHIFT), Retired::TooMany);
        assert_eq!(frames.retire(256 << PAGE_SHIFT), Retired::Now);
        assert_eq!(frames.retired_pages(), MAX_RETIRED as u64);
        assert_eq!(frames.free_pages(), 2048 - MAX_RETIRED as u64);
        frames.check().unwrap();
    }
}
