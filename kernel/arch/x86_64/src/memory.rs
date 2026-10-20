//! Physical memory: the memory map, the kernel's reservations and the early
//! frame allocator.
//!
//! The boot loader's memory map is converted once into the HAL's form. The
//! kernel then reserves what it must not hand out (its own image, the boot
//! information, modules, the first MiB, the crash log region) and gives the
//! rest of usable RAM to a bump allocator of 4 KiB frames. Phase 1 never
//! frees memory; the real physical memory manager arrives in Phase 2.

use core::ops::Range;

use radix_hal::{MemoryKind, MemoryRegion, Reservation};
use radix_sync::{Once, SpinLock};

/// Base of the direct map: physical address `p` of RAM is at `DIRECT_MAP + p`.
pub const DIRECT_MAP: u64 = 0xFFFF_8000_0000_0000;
/// The kernel image is linked at `KERNEL_OFFSET` + its physical address.
pub const KERNEL_OFFSET: u64 = 0xFFFF_FFFF_8000_0000;
/// Size of a page.
pub const PAGE: u64 = 4096;
/// Size of a large page.
pub const LARGE_PAGE: u64 = 2 * 1024 * 1024;
/// 4 GiB.
pub const FOUR_GIB: u64 = 1 << 32;

/// Most memory map entries kept.
const MAX_REGIONS: usize = 128;
/// Most reservations kept.
const MAX_RESERVATIONS: usize = 24;
/// Size of the crash log region.
pub const CRASH_LOG_SIZE: u64 = 1024 * 1024;

/// The virtual address of physical address `phys` in the direct map. Valid
/// for RAM; during early boot, for everything below 4 GiB.
pub fn phys_to_virt(phys: u64) -> usize {
    (DIRECT_MAP + phys) as usize
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
    if len < MAX_RESERVATIONS && end > start {
        reservations.list[len] = Reservation { name, start, end };
        reservations.len += 1;
    }
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

static FRAMES: SpinLock<Frames> =
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
    let mut frames = FRAMES.lock();
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

/// Allocates `pages` contiguous zeroed pages and returns the physical
/// address of the first. With `below_4g`, only memory under 4 GiB is used
/// (page tables must be reachable before the kernel's own direct map
/// exists, and the AP trampoline loads CR3 in 32-bit mode).
pub fn alloc_pages(pages: u64, below_4g: bool) -> Option<u64> {
    let bytes = pages * PAGE;
    let mut frames = FRAMES.lock();
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

/// Bytes allocated so far and the range they came from.
pub fn allocated() -> (u64, Range<u64>) {
    let frames = FRAMES.lock();
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
