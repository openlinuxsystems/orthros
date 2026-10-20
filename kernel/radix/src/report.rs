//! The boot report: memory map, reservations and CPUs.

use core::fmt;

use radix_hal::{BootParams, CpuInfo, MemoryKind, MemoryRegion, Reservation, Smp};
use radix_klog::info;

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
