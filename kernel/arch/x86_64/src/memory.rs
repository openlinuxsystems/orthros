// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Physical memory: the memory map, the kernel's reservations, the layout
//! of the kernel's address space, and the frame allocators.
//!
//! The boot loader's memory map is converted once into the HAL's form. The
//! kernel then reserves what it must not hand out (its own image, the boot
//! information, modules, the first MiB, the crash log region) and gives the
//! rest of usable RAM to an early bump allocator of 4 KiB frames, which
//! builds the kernel's page tables. Once ACPI has said which NUMA node each
//! range of memory belongs to, [`start_frame_allocator`] hands everything
//! still free to the real frame allocator (`radix_mm::FRAMES`), which
//! serves every allocation from then on.
//!
//! [`choose_layout`] places the direct map of RAM and the mapping window
//! at random addresses (KASLR); [`kernel_base`] is where the image runs.

use core::cell::UnsafeCell;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_acpi::{Slit, Srat, SratEntry};
use radix_hal::{MemoryKind, MemoryRegion, Reservation};
use radix_mm::{FRAMES, Flags, MAX_NODES, Owner, PageInfo, Region, SetupError};
use radix_sync::{Once, SpinLock};

use crate::{cpu, kaslr, percpu};

/// Where the kernel image is linked: `KERNEL_OFFSET` + its physical
/// address. It runs [`kaslr::slide`] bytes higher.
pub const KERNEL_OFFSET: u64 = 0xFFFF_FFFF_8000_0000;
/// Size of a page.
pub const PAGE: u64 = 4096;
/// Size of a large page.
pub const LARGE_PAGE: u64 = 2 * 1024 * 1024;
/// Size of a huge page, and the granularity of the randomized layout.
pub const HUGE_PAGE: u64 = 1 << 30;
/// 4 GiB.
pub const FOUR_GIB: u64 = 1 << 32;

/// Where the direct map may go: the first 64 TiB of the higher half. The
/// boot page tables map the first 4 GiB at its start, which is where the
/// direct map stays when KASLR is off.
pub const DIRECT_MAP_AREA: Range<u64> = 0xFFFF_8000_0000_0000..0xFFFF_C000_0000_0000;
/// Where the mapping window may go: the next 32 TiB.
pub const WINDOW_AREA: Range<u64> = 0xFFFF_C000_0000_0000..0xFFFF_E000_0000_0000;
/// Size of the mapping window.
pub const WINDOW_SIZE: u64 = 512 << 30;

/// Most memory map entries kept.
const MAX_REGIONS: usize = 128;
/// Most reservations kept.
const MAX_RESERVATIONS: usize = 24;
/// Size of the crash log region.
pub const CRASH_LOG_SIZE: u64 = 1024 * 1024;

static DIRECT_MAP: AtomicU64 = AtomicU64::new(DIRECT_MAP_AREA.start);
static WINDOW: AtomicU64 = AtomicU64::new(WINDOW_AREA.start);
/// How many places the direct map and the window could have gone to.
static PLACES: [AtomicU64; 2] = [AtomicU64::new(1), AtomicU64::new(1)];

/// Base of the direct map: physical address `p` of RAM is at
/// `direct_map() + p`.
pub fn direct_map() -> u64 {
    DIRECT_MAP.load(Ordering::Relaxed)
}

/// Start of the mapping window.
pub fn window() -> u64 {
    WINDOW.load(Ordering::Relaxed)
}

/// How many places the direct map and the mapping window could have gone
/// to (1 each with KASLR off).
pub fn layout_places() -> (u64, u64) {
    (PLACES[0].load(Ordering::Relaxed), PLACES[1].load(Ordering::Relaxed))
}

/// Where the kernel image runs: the link address plus the KASLR slide.
pub fn kernel_base() -> u64 {
    KERNEL_OFFSET + kaslr::slide()
}

/// The physical address of `virt`, an address inside the kernel image.
pub fn image_phys(virt: u64) -> u64 {
    virt - kernel_base()
}

/// The virtual address of physical address `phys` in the direct map. Valid
/// for RAM; during early boot, for everything below 4 GiB.
pub fn phys_to_virt(phys: u64) -> usize {
    (direct_map() + phys) as usize
}

/// Two page-directory-pointer tables in the image, for the boot alias of a
/// randomized direct map (it may cross into a second PML4 slot).
#[repr(C, align(4096))]
struct AliasTables(UnsafeCell<[[u64; 512]; 2]>);

// SAFETY: written once, by the boot CPU, before any other CPU runs.
unsafe impl Sync for AliasTables {}

static BOOT_ALIAS: AliasTables = AliasTables(UnsafeCell::new([[0; 512]; 2]));

/// Picks the addresses of the direct map and the mapping window, at random
/// multiples of 1 GiB when `randomize`, and maps the first 4 GiB at the
/// new direct map in the boot page tables, so [`phys_to_virt`] works from
/// here on. Called once, by the boot CPU, after [`set_memory_map`] and
/// before anything uses the direct map.
pub fn choose_layout(randomize: bool) {
    let ram_end = memory_map()
        .iter()
        .filter(|r| {
            matches!(r.kind, MemoryKind::Usable | MemoryKind::AcpiReclaimable | MemoryKind::AcpiNvs)
        })
        .map(|r| r.end)
        .max()
        .unwrap_or(0)
        .max(FOUR_GIB);
    let span = ram_end.next_multiple_of(HUGE_PAGE);
    let area = DIRECT_MAP_AREA.end - DIRECT_MAP_AREA.start;
    let places = [
        area.checked_sub(span).map_or(1, |room| room / HUGE_PAGE + 1),
        (WINDOW_AREA.end - WINDOW_AREA.start - WINDOW_SIZE) / HUGE_PAGE + 1,
    ];
    if !randomize {
        return;
    }
    let pick = |places: u64| radix_random::below(places) * HUGE_PAGE;
    DIRECT_MAP.store(DIRECT_MAP_AREA.start + pick(places[0]), Ordering::Relaxed);
    WINDOW.store(WINDOW_AREA.start + pick(places[1]), Ordering::Relaxed);
    PLACES[0].store(places[0], Ordering::Relaxed);
    PLACES[1].store(places[1], Ordering::Relaxed);

    let base = direct_map();
    if base == DIRECT_MAP_AREA.start {
        return;
    }
    // The boot tables: the PML4 in CR3 and, behind its slot 0, the PDPT
    // whose first four entries map the first 4 GiB. Both are below 4 GiB,
    // where the boot tables map memory one to one.
    let pml4 = (cpu::cr3() & 0x000f_ffff_ffff_f000) as *mut u64;
    // SAFETY: CR3 holds the boot PML4, mapped at its physical address.
    let low_pdpt = (unsafe { pml4.read() } & 0x000f_ffff_ffff_f000) as *const u64;
    let alias = BOOT_ALIAS.0.get();
    let alias_phys = image_phys(alias as u64);
    let (slot, first) = (((base >> 39) & 511) as usize, ((base >> 30) & 511) as usize);
    for gib in 0..4 {
        let at = first + gib;
        // SAFETY: the boot PDPT is mapped as above; `BOOT_ALIAS` is the
        // image's own, written only here, before any other CPU runs.
        unsafe { (*alias)[at / 512][at % 512] = low_pdpt.add(gib).read() };
    }
    for table in 0..=(first + 3) / 512 {
        // SAFETY: the boot PML4 is mapped as above; the new entries point
        // to page-aligned tables in the image, whose physical address is
        // its virtual address minus the kernel base.
        unsafe { pml4.add(slot + table).write((alias_phys + table as u64 * PAGE) | 0x3) };
    }
    cpu::flush_tlb();
}

struct Map {
    regions: [MemoryRegion; MAX_REGIONS],
    len: usize,
}

static MAP: Once<Map> = Once::new();

/// Converts the Multiboot2 memory map, sorting it and merging touching
/// regions of the same kind. Called once.
pub fn set_memory_map(regions: impl Iterator<Item = radix_bootinfo::MemoryRegion>) {
    let mut map = Map {
        regions: [MemoryRegion { start: 0, end: 0, kind: MemoryKind::Reserved }; MAX_REGIONS],
        len: 0,
    };
    for region in regions {
        let kind = match region.kind {
            1 => MemoryKind::Usable,
            3 => MemoryKind::AcpiReclaimable,
            4 => MemoryKind::AcpiNvs,
            5 => MemoryKind::Defective,
            _ => MemoryKind::Reserved,
        };
        let Some(end) = region.base.checked_add(region.length) else { continue };
        if end > region.base && map.len < MAX_REGIONS {
            map.regions[map.len] = MemoryRegion { start: region.base, end, kind };
            map.len += 1;
        }
    }
    let regions = &mut map.regions[..map.len];
    regions.sort_unstable_by_key(|r| r.start);
    // Merge neighbours of the same kind; overlapping entries of different
    // kinds are left as they are (the allocator only trusts `Usable`
    // ranges that no other entry overlaps).
    let mut merged = 0;
    for i in 0..map.len {
        let region = map.regions[i];
        if merged > 0 {
            let last = &mut map.regions[merged - 1];
            if last.kind == region.kind && region.start <= last.end {
                last.end = last.end.max(region.end);
                continue;
            }
        }
        map.regions[merged] = region;
        merged += 1;
    }
    map.len = merged;
    let _ = MAP.set(map);
}

/// The memory map, sorted by address.
pub fn memory_map() -> &'static [MemoryRegion] {
    MAP.get().map_or(&[], |map| &map.regions[..map.len])
}

/// Whether the memory map lists `range` as RAM the kernel may read: usable
/// memory or ACPI memory. Device memory and holes are not RAM.
pub fn is_ram(range: Range<u64>) -> bool {
    let mut position = range.start;
    for region in memory_map() {
        let ram = matches!(
            region.kind,
            MemoryKind::Usable | MemoryKind::AcpiReclaimable | MemoryKind::AcpiNvs
        );
        if !ram || region.end <= position {
            continue;
        }
        if region.start > position {
            return false;
        }
        position = region.end;
        if position >= range.end {
            return true;
        }
    }
    position >= range.end
}

struct Reservations {
    list: [Reservation; MAX_RESERVATIONS],
    len: usize,
}

static RESERVATIONS: SpinLock<Reservations> = SpinLock::new(Reservations {
    list: [Reservation { name: "", start: 0, end: 0 }; MAX_RESERVATIONS],
    len: 0,
});

/// Records that `start..end` is in use. Must happen before
/// [`init_allocator`] for the allocator to avoid it.
pub fn reserve(name: &'static str, start: u64, end: u64) {
    let mut reservations = RESERVATIONS.lock();
    let len = reservations.len;
    if end <= start {
        return;
    }
    // Dropping one would hand out memory in use (a boot module, say).
    assert!(len < MAX_RESERVATIONS, "more than {MAX_RESERVATIONS} memory reservations at boot");
    reservations.list[len] = Reservation { name, start, end };
    reservations.len += 1;
}

static RESERVATIONS_SNAPSHOT: Once<Reservations> = Once::new();

/// The reservations made so far, sorted by address, frozen for the boot
/// report.
pub fn reservations() -> &'static [Reservation] {
    let snapshot = RESERVATIONS_SNAPSHOT.call_once(|| {
        let reservations = RESERVATIONS.lock();
        let mut copy = Reservations { list: reservations.list, len: reservations.len };
        copy.list[..copy.len].sort_unstable_by_key(|r| r.start);
        copy
    });
    &snapshot.list[..snapshot.len]
}

/// A bump allocator over free RAM.
struct Frames {
    /// Free ranges, sorted, page aligned.
    free: [Range<u64>; 64],
    count: usize,
    /// Bytes handed out.
    used: u64,
    /// The lowest and highest address handed out, for the boot report;
    /// starts out inverted (empty) until the first allocation.
    span: (u64, u64),
}

static FRAMES_EARLY: SpinLock<Frames> =
    SpinLock::new(Frames { free: [const { 0..0 }; 64], count: 0, used: 0, span: (u64::MAX, 0) });

/// Removes `cut` from the ranges in `free`.
fn subtract(free: &mut [Range<u64>; 64], count: &mut usize, cut: &Range<u64>) {
    let mut i = 0;
    while i < *count {
        let range = free[i].clone();
        if cut.end <= range.start || cut.start >= range.end {
            i += 1;
            continue;
        }
        let below = range.start..cut.start.max(range.start);
        let above = cut.end.min(range.end)..range.end;
        match (below.is_empty(), above.is_empty()) {
            (true, true) => {
                free[i] = free[*count - 1].clone();
                *count -= 1;
                continue;
            }
            (false, true) => free[i] = below,
            (true, false) => free[i] = above,
            (false, false) => {
                free[i] = below;
                // With the table full, the part above is left unused.
                if *count < free.len() {
                    free[*count] = above;
                    *count += 1;
                }
            }
        }
        i += 1;
    }
}

/// Builds the allocator from the usable RAM above 1 MiB, minus every
/// reservation made so far and every memory map entry that is not usable.
pub fn init_allocator() {
    let mut frames = FRAMES_EARLY.lock();
    let mut free = [const { 0..0 }; 64];
    let mut count = 0;
    for region in memory_map().iter().filter(|r| r.kind == MemoryKind::Usable) {
        let start = region.start.max(1 << 20).next_multiple_of(PAGE);
        let end = region.end & !(PAGE - 1);
        if end > start && count < free.len() {
            free[count] = start..end;
            count += 1;
        }
    }
    for region in memory_map().iter().filter(|r| r.kind != MemoryKind::Usable) {
        subtract(&mut free, &mut count, &(region.start..region.end));
    }
    let reservations = RESERVATIONS.lock();
    for reservation in &reservations.list[..reservations.len] {
        let cut = (reservation.start & !(PAGE - 1))..reservation.end.next_multiple_of(PAGE);
        subtract(&mut free, &mut count, &cut);
    }
    free[..count].sort_unstable_by_key(|r| r.start);
    frames.free = free;
    frames.count = count;
}

/// Set once the frame allocator has taken over from the bump allocator.
static HANDED_OVER: AtomicBool = AtomicBool::new(false);

/// Allocates `pages` contiguous zeroed pages for `owner` and returns the
/// physical address of the first, or `None` when memory ran out. With
/// `below_4g`, only memory under 4 GiB is used (page tables must be
/// reachable before the kernel's own direct map exists, and the AP
/// trampoline loads CR3 in 32-bit mode). Before [`start_frame_allocator`]
/// this takes from the bump allocator; afterwards from the frame allocator
/// (a power-of-two block, without sleeping).
pub fn alloc_pages(pages: u64, below_4g: bool, owner: Owner) -> Option<u64> {
    if HANDED_OVER.load(Ordering::Acquire) {
        let order = pages.max(1).next_power_of_two().trailing_zeros();
        let flags = if below_4g { Flags::ATOMIC.zeroed().dma32() } else { Flags::ATOMIC.zeroed() };
        return FRAMES.alloc(order, flags, owner).ok();
    }
    let bytes = pages * PAGE;
    let mut frames = FRAMES_EARLY.lock();
    let count = frames.count;
    let range = frames.free[..count]
        .iter_mut()
        .find(|r| r.end - r.start >= bytes && (!below_4g || r.start + bytes <= FOUR_GIB))?;
    let start = range.start;
    range.start += bytes;
    frames.used += bytes;
    frames.span = (frames.span.0.min(start), frames.span.1.max(start + bytes));
    drop(frames);
    zero_physical(start, bytes);
    Some(start)
}

/// Gives back pages from [`alloc_pages`] after the frame allocator took
/// over (the bump allocator cannot take anything back).
pub fn free_pages(phys: u64, pages: u64, owner: Owner) {
    if HANDED_OVER.load(Ordering::Acquire) {
        let order = pages.max(1).next_power_of_two().trailing_zeros();
        if let Err(err) = FRAMES.free(phys, order, owner) {
            panic!("freeing {pages} pages at {phys:#x}: {err:?}");
        }
    }
}

/// Bytes the bump allocator handed out and the range they came from.
pub fn allocated() -> (u64, Range<u64>) {
    let frames = FRAMES_EARLY.lock();
    (frames.used, frames.span.0.min(frames.span.1)..frames.span.1)
}

fn zero_physical(start: u64, bytes: u64) {
    let virt = phys_to_virt(start);
    // SAFETY: the range was just taken from the allocator, so nothing else
    // uses it, and it is RAM reachable through the direct map (or, before
    // the kernel's tables are live, through the boot alias below 4 GiB,
    // which is where early allocations come from).
    unsafe { core::ptr::write_bytes(virt as *mut u8, 0, bytes as usize) };
}

/// The machine as the memory manager sees it.
struct MmPlatform;

// SAFETY: the direct map covers all RAM the frame allocator manages (it
// is given only usable RAM, which `paging` maps) for the kernel's whole
// life, and the CPU index is below `MAX_CPUS` and fixed per CPU.
unsafe impl radix_mm::Platform for MmPlatform {
    fn current_cpu(&self) -> usize {
        percpu::current_index()
    }

    fn cpu_node(&self, cpu: usize) -> usize {
        percpu::get(cpu).map_or(0, |c| usize::from(c.node.load(Ordering::Relaxed)))
    }

    fn disable_interrupts(&self) -> bool {
        cpu::disable_interrupts()
    }

    fn restore_interrupts(&self, were_enabled: bool) {
        if were_enabled {
            cpu::enable_interrupts();
        }
    }

    fn direct_map(&self) -> usize {
        direct_map() as usize
    }
}

static MM_PLATFORM: MmPlatform = MmPlatform;

/// NUMA nodes: the SRAT's proximity domains, numbered in the order the
/// table first mentions them.
struct Nodes {
    domains: [u32; MAX_NODES],
    count: usize,
}

impl Nodes {
    fn from_srat(srat: Option<&Srat<'_>>) -> Nodes {
        let mut nodes = Nodes { domains: [0; MAX_NODES], count: 1 };
        let Some(srat) = srat else { return nodes };
        nodes.count = 0;
        for entry in srat.entries() {
            let domain = match entry {
                SratEntry::Cpu { domain, .. } | SratEntry::Memory { domain, .. } => domain,
            };
            if !nodes.domains[..nodes.count].contains(&domain) && nodes.count < MAX_NODES {
                nodes.domains[nodes.count] = domain;
                nodes.count += 1;
            }
        }
        nodes.count = nodes.count.max(1);
        nodes
    }

    /// The node of `domain`; domains beyond `MAX_NODES` share node 0.
    fn of(&self, domain: u32) -> usize {
        self.domains[..self.count].iter().position(|&d| d == domain).unwrap_or(0)
    }
}

/// The node of physical address `phys`, and the next address where the
/// SRAT's ranges begin or end (where a region must be split).
fn node_at(nodes: &Nodes, srat: Option<&Srat<'_>>, phys: u64) -> (usize, u64) {
    let (mut node, mut boundary) = (0, u64::MAX);
    for entry in srat.into_iter().flat_map(|srat| srat.entries()) {
        if let SratEntry::Memory { domain, base, length, .. } = entry {
            let end = base.saturating_add(length);
            if (base..end).contains(&phys) {
                node = nodes.of(domain);
            }
            for edge in [base, end] {
                if edge > phys {
                    boundary = boundary.min(edge);
                }
            }
        }
    }
    (node, boundary)
}

/// What [`start_frame_allocator`] did, for the boot report.
#[derive(Clone, Copy, Debug, Default)]
pub struct Handover {
    /// Regions given to the frame allocator.
    pub regions: usize,
    /// Bytes of frame records.
    pub record_bytes: u64,
    /// Frames released as free.
    pub free_frames: u64,
    /// RAM left out because the region table was full.
    pub skipped_bytes: u64,
}

static HANDOVER: Once<Handover> = Once::new();

/// What [`start_frame_allocator`] did.
pub fn handover() -> Option<&'static Handover> {
    HANDOVER.get()
}

/// Hands physical memory to the frame allocator: describes every range of
/// usable RAM above 1 MiB (split at 4 GiB and at NUMA node boundaries) with
/// frame records taken from the bump allocator, gives it what the bump
/// allocator has not handed out, and starts it. Each CPU's node comes from
/// the SRAT; distances from the SLIT. Called once, by the boot CPU, after
/// ACPI and before the other CPUs start.
pub fn start_frame_allocator(srat: Option<&Srat<'_>>, slit: Option<&Slit<'_>>) {
    let nodes = Nodes::from_srat(srat);
    for cpu in (0..radix_hal::MAX_CPUS).filter_map(percpu::get) {
        let apic_id = cpu.apic_id.load(Ordering::Relaxed);
        let domain = srat.into_iter().flat_map(|srat| srat.entries()).find_map(|e| match e {
            SratEntry::Cpu { domain, apic_id: id } if id == apic_id => Some(domain),
            _ => None,
        });
        cpu.node.store(domain.map_or(0, |d| nodes.of(d)) as u8, Ordering::Relaxed);
    }
    FRAMES.set_platform(&MM_PLATFORM);
    FRAMES.set_topology(nodes.count, |from, to| {
        let local = if from == to { 10 } else { 20 };
        slit.and_then(|slit| {
            slit.distance(nodes.domains[from] as usize, nodes.domains[to] as usize)
        })
        .unwrap_or(local)
    });

    let mut report = Handover::default();
    for region in memory_map().iter().filter(|r| r.kind == MemoryKind::Usable) {
        let mut start = region.start.max(1 << 20).next_multiple_of(PAGE);
        let end = region.end & !(PAGE - 1);
        while start < end {
            let (node, boundary) = node_at(&nodes, srat, start);
            let limit = if start < FOUR_GIB { FOUR_GIB } else { u64::MAX };
            let piece_end = end.min(boundary).min(limit);
            let pages = (piece_end - start) / PAGE;
            let bytes = pages * core::mem::size_of::<PageInfo>() as u64;
            let added = alloc_pages(bytes.div_ceil(PAGE), false, Owner::Kernel).map(|records| {
                // SAFETY: the bump allocator just handed out these zeroed
                // pages for good; zero bytes are valid `PageInfo` records,
                // and the direct map keeps them mapped.
                let pages = unsafe {
                    core::slice::from_raw_parts(
                        phys_to_virt(records) as *const PageInfo,
                        pages as usize,
                    )
                };
                FRAMES.add_region(node, Region { base: start / PAGE, pages })
            });
            match added {
                Some(Ok(())) => {
                    report.regions += 1;
                    report.record_bytes += bytes;
                }
                Some(Err(SetupError::TooManyRegions)) | None => {
                    report.skipped_bytes += piece_end - start;
                }
                Some(Err(err)) => {
                    radix_klog::warn!("Memory: {start:#x}-{piece_end:#x} left out: {err:?}");
                    report.skipped_bytes += piece_end - start;
                }
            }
            start = piece_end;
        }
    }
    // From here on every allocation goes to the frame allocator.
    let frames = FRAMES_EARLY.lock();
    HANDED_OVER.store(true, Ordering::Release);
    for range in &frames.free[..frames.count] {
        report.free_frames += FRAMES.release(range.start, range.end);
    }
    drop(frames);
    FRAMES.set_reclaim(|| radix_mm::HEAP.drain());
    FRAMES.start();
    let _ = HANDOVER.set(report);
}

/// Picks the crash log's place: the lowest 2 MiB boundary at or above
/// 4 GiB with `CRASH_LOG_SIZE` of usable RAM, where firmware and boot
/// loaders rarely write; otherwise the lowest 2 MiB boundary at or above
/// 64 MiB. The choice depends only on the memory map, so it is the same on
/// every boot of the same machine.
pub fn choose_crash_log_region() -> Option<u64> {
    let fits = |floor: u64| {
        memory_map().iter().filter(|r| r.kind == MemoryKind::Usable).find_map(|r| {
            let start = r.start.max(floor).next_multiple_of(LARGE_PAGE);
            (start + CRASH_LOG_SIZE <= r.end).then_some(start)
        })
    };
    fits(FOUR_GIB).or_else(|| fits(64 << 20))
}

/// The page below 1 MiB that starts the other CPUs: 0x8000 if it is
/// usable, otherwise the first usable page from 0x1000.
pub fn choose_trampoline_page() -> Option<u64> {
    let usable = |page: u64| {
        memory_map()
            .iter()
            .any(|r| r.kind == MemoryKind::Usable && r.start <= page && page + PAGE <= r.end)
    };
    if usable(0x8000) {
        return Some(0x8000);
    }
    (1..0x9f).map(|n| n * PAGE).find(|&page| usable(page))
}
