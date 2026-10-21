// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
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
//! heap's [`crate::Heap::drain`]) and tries once more.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use radix_sync::{Once, SpinLock};

use crate::buddy::{Buddy, FreeError, MAX_ORDER, ORDERS, RegionError};
use crate::context::{AllocError, Context, Flags, NodePolicy, ZoneLimit};
use crate::page::{Owner, PAGE_SHIFT, PAGE_SIZE, PageInfo, Region};
use crate::platform::{NoInterrupts, Platform};

/// Most NUMA nodes.
pub const MAX_NODES: usize = 8;
/// Most regions of RAM across all zones.
pub const MAX_REGIONS: usize = 64;
/// First frame of the `Normal` zone: 4 GiB.
const DMA32_END_PFN: u64 = 1 << (32 - PAGE_SHIFT);
/// Zones per node.
const KINDS: usize = 2;

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
    /// Frames free.
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
}

impl Zone {
    const fn new() -> Zone {
        Zone {
            buddy: SpinLock::new(Buddy::new()),
            reserve: AtomicU64::new(0),
            protect: AtomicU64::new(0),
        }
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
}

impl RegionTable {
    const fn new() -> RegionTable {
        RegionTable { entries: [None; MAX_REGIONS], len: 0 }
    }

    fn iter(&self) -> impl Iterator<Item = &RegionEntry> {
        self.entries[..self.len].iter().flatten()
    }

    /// The entry containing frame `pfn` (binary search; sorted by base).
    fn find(&self, pfn: u64) -> Option<&RegionEntry> {
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
    nodes: AtomicUsize,
    /// Nodes in fallback order, per node: nearest first.
    fallback: [[AtomicU8; MAX_NODES]; MAX_NODES],
    distance: [[AtomicU8; MAX_NODES]; MAX_NODES],
    owner_pages: [AtomicU64; Owner::ALL.len()],
    failures: AtomicU64,
    reclaim: Once<fn()>,
    /// Whether any `Normal` zone has memory.
    has_normal: AtomicBool,
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
            nodes: AtomicUsize::new(1),
            fallback: [const { [const { AtomicU8::new(0) }; MAX_NODES] }; MAX_NODES],
            distance: [const { [const { AtomicU8::new(10) }; MAX_NODES] }; MAX_NODES],
            owner_pages: [const { AtomicU64::new(0) }; Owner::ALL.len()],
            failures: AtomicU64::new(0),
            reclaim: Once::new(),
            has_normal: AtomicBool::new(false),
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
        self.alloc_inner(order, flags, owner, tag, true)
    }

    /// The allocation itself. Without `reclaim`, a `MaySleep` allocation
    /// keeps out of the reserves but does not run the reclaim hook: the
    /// heap allocates its own bookkeeping this way, so reclaim never runs
    /// inside the heap.
    pub(crate) fn alloc_inner(
        &self,
        order: u32,
        flags: Flags,
        owner: Owner,
        tag: u8,
        reclaim: bool,
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
        for attempt in 0..2 {
            if let Some(pfn) = self.try_zones(origin, order, flags, owner, tag) {
                self.owner_pages[owner as usize].fetch_add(1 << order, Ordering::Relaxed);
                let phys = pfn << PAGE_SHIFT;
                if let (true, Some(platform)) = (flags.zero, platform) {
                    // SAFETY: the block was just taken from a free list, so
                    // nothing else uses it, and the platform's direct map
                    // maps every managed frame writable (`Platform`).
                    #[allow(unsafe_code)]
                    unsafe {
                        crate::raw::zero(platform.direct_map() + phys as usize, PAGE_SIZE << order)
                    };
                }
                return Ok(phys);
            }
            match (attempt, flags.context, self.reclaim.get()) {
                (0, Context::MaySleep, Some(hook)) if reclaim => hook(),
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
                if let Some(pfn) = buddy.alloc(order, owner, tag) {
                    return Some(pfn);
                }
            }
        }
        None
    }

    /// Frees the block of 2^`order` frames at physical address `phys`,
    /// which `owner` allocated.
    pub fn free(&self, phys: u64, order: u32, owner: Owner) -> Result<(), FreeError> {
        let pfn = phys >> PAGE_SHIFT;
        if !phys.is_multiple_of(PAGE_SIZE as u64) {
            return Err(FreeError::Misaligned);
        }
        let table = self.regions.get().ok_or(FreeError::NotManaged)?;
        let entry = table.find(pfn).ok_or(FreeError::NotManaged)?;
        let _irqs = NoInterrupts::new(self.platform());
        self.zones[entry.zone].buddy.lock().free(pfn, order, owner)?;
        self.owner_pages[owner as usize].fetch_sub(1 << order, Ordering::Relaxed);
        Ok(())
    }

    /// The record of the frame at physical address `phys`.
    pub fn info(&self, phys: u64) -> Option<&'static PageInfo> {
        self.regions.get()?.find(phys >> PAGE_SHIFT)?.region.info(phys >> PAGE_SHIFT)
    }

    /// The node whose memory holds physical address `phys`.
    pub fn node_of(&self, phys: u64) -> Option<usize> {
        Some(self.regions.get()?.find(phys >> PAGE_SHIFT)?.zone / KINDS)
    }

    /// Free frames in all zones.
    pub fn free_pages(&self) -> u64 {
        self.zones.iter().map(|zone| zone.buddy.lock().free_pages()).sum()
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
                    free: buddy.free_pages(),
                    reserve: zone.reserve.load(Ordering::Relaxed),
                    blocks,
                    span,
                }
            };
            f(&stats);
        }
    }

    /// Checks every zone's free lists ([`Buddy::check`]).
    pub fn check(&self) -> Result<(), &'static str> {
        for zone in &self.zones {
            let _irqs = NoInterrupts::new(self.platform());
            zone.buddy.lock().check()?;
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
}
