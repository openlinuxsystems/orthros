//! The boot report: memory map, reservations, frame allocator zones, the
//! heap and CPUs.

use core::fmt;

use radix_hal::{BootParams, CpuInfo, MemoryKind, MemoryRegion, Reservation, Smp};
use radix_klog::info;
use radix_mm::{FRAMES, HEAP, PAGE_SIZE};

use crate::Arch;

/// A byte count in the largest whole unit.
pub struct Size(pub u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let units = [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
        for (unit, size) in units {
            if self.0 >= size && self.0.is_multiple_of(size) || self.0 >= 16 * size {
                return write!(f, "{} {unit}", self.0 / size);
            }
        }
        write!(f, "{} bytes", self.0)
    }
}

/// Writes the memory map, one line per region.
pub fn write_memory_map(regions: &[MemoryRegion], line: &mut dyn FnMut(fmt::Arguments<'_>)) {
    for region in regions {
        line(format_args!(
            "  {:#014x}-{:#014x} {:>9}  {}",
            region.start,
            region.end - 1,
            Size(region.len()).to_string_buf().as_str(),
            region.kind
        ));
    }
}

/// Writes the reservations, one line per range.
pub fn write_reservations(reservations: &[Reservation], line: &mut dyn FnMut(fmt::Arguments<'_>)) {
    for r in reservations {
        line(format_args!(
            "  {:#014x}-{:#014x} {:>9}  {}",
            r.start,
            r.end - 1,
            Size(r.end - r.start).to_string_buf().as_str(),
            r.name
        ));
    }
}

impl Size {
    fn to_string_buf(&self) -> radix_klog::TextBuf<16> {
        use core::fmt::Write;
        let mut buf = radix_klog::TextBuf::new();
        let _ = write!(buf, "{self}");
        buf
    }
}

/// Logs the memory map and the kernel's reservations.
pub fn memory(params: &BootParams) {
    let usable: u64 = params
        .memory_map
        .iter()
        .filter(|r| r.kind == MemoryKind::Usable)
        .map(MemoryRegion::len)
        .sum();
    let regions = params.memory_map.iter().filter(|r| r.kind == MemoryKind::Usable).count();
    info!("Memory: {} MiB usable in {regions} regions; memory map:", usable >> 20);
    write_memory_map(params.memory_map, &mut |args| info!("{args}"));
    info!("Memory: reserved by the kernel:");
    write_reservations(params.reservations, &mut |args| info!("{args}"));
}

/// Writes the frame allocator's zones, one line each.
pub fn write_zones(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
    FRAMES.zone_stats(|zone| {
        line(format_args!(
            "  node {} {:<6} {:#014x}-{:#014x} {:>9} managed, {:>9} free, {:>7} reserve",
            zone.node,
            zone.kind.name(),
            zone.span.0,
            zone.span.1.saturating_sub(1),
            Size(zone.managed * PAGE_SIZE as u64).to_string_buf().as_str(),
            Size(zone.free * PAGE_SIZE as u64).to_string_buf().as_str(),
            Size(zone.reserve * PAGE_SIZE as u64).to_string_buf().as_str(),
        ));
    });
}

/// Writes the heap's totals and its size classes, one line each.
pub fn write_heap(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
    let stats = HEAP.stats();
    line(format_args!(
        "Heap: {} objects in use ({}), {} cached in magazines, {} of slabs, {} large blocks \
         ({}); {} failed requests, {} damaged objects found",
        stats.objects,
        Size(stats.object_bytes).to_string_buf().as_str(),
        stats.cached,
        Size(stats.slab_bytes).to_string_buf().as_str(),
        stats.large_blocks,
        Size(stats.large_bytes).to_string_buf().as_str(),
        stats.failures,
        stats.corruptions
    ));
    HEAP.class_stats(|class| {
        line(format_args!(
            "  {:>5} bytes: {:>5} slabs ({:>9}), {:>7} in use, {:>6} cached{}",
            class.size,
            class.slabs,
            Size(class.slab_bytes).to_string_buf().as_str(),
            class.in_use,
            class.cached,
            if class.quarantined > 0 { ", has quarantined slabs" } else { "" }
        ));
    });
}

/// Logs the frame allocator's zones once it has started.
pub fn frames() {
    if !FRAMES.is_running() {
        info!("Memory: the frame allocator did not start");
        return;
    }
    info!(
        "Memory: {} free of {} in the frame allocator's zones:",
        Size(FRAMES.free_pages() * PAGE_SIZE as u64),
        Size(FRAMES.managed_pages() * PAGE_SIZE as u64)
    );
    write_zones(&mut |args| info!("{args}"));
}

/// Writes one line per CPU.
pub fn write_cpus(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
    for index in 0..Arch::cpu_count() {
        let Some(CpuInfo { hardware_id, boot_cpu, online, .. }) = Arch::cpu_info(index) else {
            continue;
        };
        line(format_args!(
            "  cpu{index}: APIC ID {hardware_id}{}, {}",
            if boot_cpu { ", boot CPU" } else { "" },
            if online { "online" } else { "offline" }
        ));
    }
}

/// Logs the CPU list.
pub fn cpus(online: usize) {
    info!("CPUs: {online} of {} online:", Arch::cpu_count());
    write_cpus(&mut |args| info!("{args}"));
}
