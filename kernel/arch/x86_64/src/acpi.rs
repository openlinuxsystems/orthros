//! Finding and reading the ACPI tables.
//!
//! GRUB passes a copy of the RSDP in the boot information; on a BIOS machine
//! without one the kernel scans the EBDA and the BIOS ROM area for it. The
//! tables themselves are parsed by `radix-acpi`; this module gives that
//! crate access to physical memory and keeps the tables the kernel needs.

use core::fmt;

use radix_acpi::{
    Fadt, Hpet, Madt, MadtEntry, Memory, Rsdp, Sdt, Slit, Srat, SratEntry, Tables, mcfg_entries,
};
use radix_sync::Once;

use crate::memory;
use crate::paging::{self, Access, Cache};

/// Physical memory for the parser: RAM through the direct map, anything
/// else (some firmware keeps tables in reserved memory, and the BIOS ROM is
/// not RAM) through a new uncached mapping.
pub struct PhysicalMemory;

impl Memory<'static> for PhysicalMemory {
    fn bytes(&self, addr: u64, len: usize) -> Option<&'static [u8]> {
        let end = addr.checked_add(len as u64)?;
        let virt = if memory::is_ram(addr..end) {
            memory::phys_to_virt(addr)
        } else {
            paging::map_physical(addr, len as u64, Access::ReadOnly, Cache::Uncached)
        };
        // SAFETY: `virt` maps `len` bytes at `addr` for the rest of the
        // kernel's life (the direct map, or a mapping that is never
        // removed). The bytes are firmware tables, which nothing writes
        // after boot.
        Some(unsafe { core::slice::from_raw_parts(virt as *const u8, len) })
    }
}

/// What the kernel learnt from ACPI.
pub struct Acpi {
    /// Physical address of the RSDP, or `None` for the boot loader's copy.
    pub rsdp_address: Option<u64>,
    /// The RSDP.
    pub rsdp: Rsdp,
    /// Every table the root table lists.
    pub tables: Tables,
    /// The MADT, describing CPUs and interrupt controllers.
    pub madt: Option<Madt<'static>>,
    /// The FADT.
    pub fadt: Option<Fadt>,
    /// The HPET table.
    pub hpet: Option<Hpet>,
    /// The MCFG table.
    pub mcfg: Option<Sdt<'static>>,
    /// The SRAT: which NUMA domain each CPU and range of memory is in.
    pub srat: Option<Srat<'static>>,
    /// The SLIT: distances between NUMA domains.
    pub slit: Option<Slit<'static>>,
}

static ACPI: Once<Acpi> = Once::new();

/// The tables found by [`init`].
pub fn get() -> Option<&'static Acpi> {
    ACPI.get()
}

/// Scans the BIOS areas for the RSDP.
fn scan_bios() -> Option<(u64, Rsdp)> {
    let memory = PhysicalMemory;
    // The EBDA's segment is stored at 0x40E in the BIOS data area.
    let ebda = memory
        .bytes(0x40e, 2)
        .map(|b| u64::from(u16::from_le_bytes([b[0], b[1]])) << 4)
        .filter(|&base| (0x80000..0xa0000).contains(&base));
    let ebda_area = ebda.and_then(|base| Some((base, memory.bytes(base, 1024)?)));
    let rom_area = memory.bytes(0xe0000, 0x20000).map(|bytes| (0xe0000, bytes));
    let areas: [Option<(u64, &[u8])>; 2] = [ebda_area, rom_area];
    let mut found = None;
    for area in areas.iter().flatten() {
        if let Some(hit) = Rsdp::scan(core::slice::from_ref(area)) {
            found = Some(hit);
            break;
        }
    }
    found
}

/// Finds the RSDP and reads the tables. `copy` is the RSDP from the boot
/// information, if there was one.
pub fn init(copy: Option<&[u8]>) -> Result<&'static Acpi, &'static str> {
    let (rsdp_address, rsdp) = match copy.map(Rsdp::parse) {
        Some(Ok(rsdp)) => (None, rsdp),
        _ => match scan_bios() {
            Some((address, rsdp)) => (Some(address), rsdp),
            None => return Err("no ACPI RSDP found"),
        },
    };
    let memory = PhysicalMemory;
    let tables = Tables::load(&rsdp, &memory).map_err(|_| "the ACPI root table is invalid")?;
    let madt = tables.find(&memory, b"APIC").and_then(|sdt| Madt::parse(&sdt).ok());
    let fadt = tables.find(&memory, b"FACP").and_then(|sdt| Fadt::parse(&sdt).ok());
    let hpet = tables.find(&memory, b"HPET").and_then(|sdt| Hpet::parse(&sdt).ok());
    let mcfg = tables.find(&memory, b"MCFG");
    let srat = tables.find(&memory, b"SRAT").and_then(|sdt| Srat::parse(&sdt).ok());
    let slit = tables.find(&memory, b"SLIT").and_then(|sdt| Slit::parse(&sdt).ok());
    let acpi = Acpi { rsdp_address, rsdp, tables, madt, fadt, hpet, mcfg, srat, slit };
    Ok(ACPI.call_once(|| acpi))
}

fn oem(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("?").trim_end()
}

/// Writes a description of the tables, one line per call of `line`.
pub fn describe(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
    let Some(acpi) = get() else {
        line(format_args!("ACPI: no tables"));
        return;
    };
    let root = acpi.tables.root();
    match acpi.rsdp_address {
        Some(address) => line(format_args!(
            "ACPI: RSDP at {address:#x}, revision {}, OEM \"{}\"",
            acpi.rsdp.revision,
            oem(&acpi.rsdp.oem_id)
        )),
        None => line(format_args!(
            "ACPI: RSDP from the boot loader, revision {}, OEM \"{}\"",
            acpi.rsdp.revision,
            oem(&acpi.rsdp.oem_id)
        )),
    }
    line(format_args!(
        "ACPI: {} at {:#x}, {} bytes, lists {} tables",
        root.signature_str(),
        root.address,
        root.length,
        acpi.tables.iter().count()
    ));
    for table in acpi.tables.iter() {
        match table.status {
            Ok(()) => line(format_args!(
                "ACPI:   {} at {:#x}, {} bytes",
                table.signature_str(),
                table.address,
                table.length
            )),
            Err(err) => line(format_args!(
                "ACPI:   {} at {:#x}: ignored, {err}",
                table.signature_str(),
                table.address
            )),
        }
    }
    if let Some(madt) = &acpi.madt {
        let (mut cpus, mut ioapics, mut overrides) = (0, 0, 0);
        for entry in madt.entries() {
            match entry {
                MadtEntry::LocalApic { enabled: true, .. } => cpus += 1,
                MadtEntry::IoApic { .. } => ioapics += 1,
                MadtEntry::InterruptSourceOverride { .. } => overrides += 1,
                _ => {}
            }
        }
        line(format_args!(
            "ACPI: MADT: local APIC at {:#x}, {cpus} usable CPUs, {ioapics} I/O APICs, \
             {overrides} interrupt overrides{}",
            madt.local_apic_address,
            if madt.has_8259 { ", dual 8259 PICs" } else { "" }
        ));
    } else {
        line(format_args!("ACPI: no MADT; only the boot CPU can be used"));
    }
    if let Some(fadt) = &acpi.fadt {
        match fadt.pm_timer {
            Some((timer, extended)) => line(format_args!(
                "ACPI: FADT: PM timer at {} {:#x} ({} bits)",
                if timer.space == radix_acpi::GenericAddress::IO { "port" } else { "address" },
                timer.address,
                if extended { 32 } else { 24 }
            )),
            None => line(format_args!("ACPI: FADT: no PM timer")),
        }
        match fadt.reset {
            Some((register, value)) => line(format_args!(
                "ACPI: FADT: reset by writing {value:#x} to {} {:#x}",
                if register.space == radix_acpi::GenericAddress::IO { "port" } else { "address" },
                register.address
            )),
            None => line(format_args!("ACPI: FADT: no reset register")),
        }
    }
    if let Some(hpet) = &acpi.hpet {
        line(format_args!("ACPI: HPET {} at {:#x}", hpet.number, hpet.address));
    }
    if let Some(srat) = &acpi.srat {
        let (mut cpus, mut ranges, mut domains) = (0, 0, 0u64);
        for entry in srat.entries() {
            let domain = match entry {
                SratEntry::Cpu { domain, .. } => {
                    cpus += 1;
                    domain
                }
                SratEntry::Memory { domain, .. } => {
                    ranges += 1;
                    domain
                }
            };
            domains |= 1u64.checked_shl(domain).unwrap_or(0);
        }
        line(format_args!(
            "ACPI: SRAT: {cpus} CPUs and {ranges} memory ranges in {} proximity domains",
            domains.count_ones()
        ));
    }
    if let Some(slit) = &acpi.slit {
        line(format_args!("ACPI: SLIT: distances between {} localities", slit.localities));
    }
    if let Some(mcfg) = &acpi.mcfg {
        for window in mcfg_entries(mcfg) {
            line(format_args!(
                "ACPI: MCFG: PCI Express configuration space at {:#x}, segment {}, buses {}-{}",
                window.base, window.segment, window.start_bus, window.end_bus
            ));
        }
    }
}
