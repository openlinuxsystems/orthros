// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/kernel`: the kernel's view of the hardware and its settings, one
//! value per file. `/sys`, Linux's name for it, is a link to it.
//!
//! The layout follows Linux's where both have a thing, so that ported
//! tools find it where they look (lspci reads `/sys/bus/pci/devices`):
//!
//! - `/kernel/version`, `/kernel/cmdline`, `/kernel/cpus`;
//! - `/kernel/block/<name>/size` (in 512-byte sectors) and `ro` (0 or 1),
//!   for every disk and partition; for a partition, its number in its
//!   disk's table (`partition`), its first sector (`start`) and its type
//!   as lsblk gives it (`parttype`, Radix's own); and for a
//!   disk, in its `queue`, the sizes and flags of radix-block(2)'s
//!   requests as Linux names them (`rotational`, `logical_block_size`,
//!   `discard_max_bytes` and the rest), and in its `device` the `model`,
//!   `serial` and `firmware_rev` it gives, for SATA, NVMe and USB disks;
//!   and its `stat`, what it has done, in the 17 numbers of Linux's
//!   `/sys/block/<disk>/stat` (block/stat.rst), which iostat(1) reads;
//! - `/kernel/bus/pci/devices/<address>/vendor`, `device`, `class`,
//!   `revision`, `subsystem_vendor` and `subsystem_device`, in
//!   hexadecimal, for every PCI function the scan found;
//! - `/kernel/bus/usb/devices`: each controller's root hub (`usb1`),
//!   each USB device (`1-3.2`) and each of its interfaces (`1-3.2:1.0`),
//!   with Linux's files (ADR 0230, [`usb`]);
//! - `/kernel/bus/i2c/devices/i2c-0/name`, when the kernel drives the
//!   chipset's SMBus host controller (ADR 0206), as Linux names its
//!   adapter (`SMBus I801 adapter at <port>`, or `SMBus PIIX4 adapter
//!   port 0 at <port>` for AMD's FCH);
//! - `/kernel/firmware/memmap/<n>/start`, `end` (its last byte, in
//!   hexadecimal) and `type` (Linux's names: `System RAM`, `Reserved`,
//!   `ACPI Tables`...), for each range of the firmware's memory map
//!   (ADR 0144);
//! - `/kernel/class/dmi/id`, where the firmware has SMBIOS tables
//!   (ADR 0231): Linux's files of the names the firmware gives the
//!   machine, its board, chassis and firmware (`sys_vendor`,
//!   `product_name`, `board_name`, `bios_version`, `chassis_type`...),
//!   those it gives, without the serial numbers and the UUID
//!   ([`crate::dmi`]);
//! - `/kernel/class/watchdog/watchdog0`, when the kernel drives a hardware
//!   watchdog (ADR 0143): its `identity`, its `state` (`active` or
//!   `inactive`), its `timeout`, `min_timeout` and `max_timeout` in
//!   seconds, and its `bootstatus`, 32 (Linux's WDIOF_CARDRESET) if it
//!   restarted the machine at the end of the boot before, else 0;
//! - `/kernel/class/power_supply/<name>`, for each battery and AC adapter
//!   of the firmware's (ADR 0201), as Linux's ACPI drivers show them: an
//!   adapter's `type` (`Mains`) and `online`; a battery's `type`
//!   (`Battery`), `present`, `status`, `capacity` (percent),
//!   `capacity_level`, `technology`, `cycle_count`, `voltage_min_design`
//!   and `voltage_now` (µV), `energy_full_design`, `energy_full`,
//!   `energy_now` (µWh) and `power_now` (µW), or for a battery that
//!   counts in amperes `charge_*` (µAh) and `current_now` (µA), and
//!   `model_name`, `manufacturer` and `serial_number`. A value the
//!   battery does not know, as the rest while its bay is empty, fails to
//!   read (EIO), as Linux's do (ENODEV);
//! - `/kernel/class/thermal/thermal_zone<n>`, for each of the firmware's
//!   thermal zones (ADR 0203), as Linux's ACPI thermal driver shows them:
//!   its `type` (`acpitz`), `temp` (thousandths of a degree Celsius, read
//!   from the zone when opened), `mode` (`enabled`), `policy` and
//!   `available_policies` (`step_wise`), and each trip point's
//!   `trip_point_<k>_type`, `_temp` and `_hyst` (0), critical, hot and
//!   passive in that order; and where the CPUs pick their own speed, a
//!   `cooling_device<n>` for each (`type` `Processor`, `max_state` 3 and
//!   `cur_state`, the steps passive cooling slows them by);
//! - `/kernel/class/hwmon/hwmon<n>`, for the temperature sensors
//!   (ADR 0205), as Linux's hwmon class shows them: an `acpitz` for the
//!   thermal zones, a `temp<k>_input` (and `temp<k>_crit`) each; AMD's
//!   processor sensors, `k10temp`, with `temp<k>_input` and
//!   `temp<k>_label` (`Tctl`, `Tccd1`...); and a `coretemp` for each of
//!   Intel's packages, with each sensor's `temp<k>_label` (`Package id
//!   0`, `Core 0`...), `temp<k>_input`, `temp<k>_crit` (TjMax),
//!   `temp<k>_max` where the processor gives it and `temp<k>_crit_alarm`;
//!   and `cros_ec`, Framework's embedded controller's (ADR 0207), with
//!   each fan's `fan<k>_input` and `fan<k>_fault` and each sensor's
//!   `temp<k>_input`, `temp<k>_fault` and `temp<k>_label`.
//!   Each `name` says which, and a sensor that gives no reading fails to
//!   read (EIO);
//! - `/kernel/devices/system/cpu`: `possible`, `present` and `online`, the
//!   CPUs as Linux's lists (`0-7`), and where the CPUs pick their own
//!   speed (ADR 0202), each CPU's `cpufreq/policy<n>`, as Linux's
//!   `intel_pstate` and `amd-pstate-epp` drivers have them, with
//!   `cpu<n>/cpufreq` a link to it ([`crate::pstate::files`]);
//! - `/kernel/devices/system/edac/mc/mc0`, while the machine-check banks
//!   are read (ADR 0169): Linux's EDAC counts of the memory errors they
//!   reported, `ce_count` and `ue_count`, `ce_noinfo_count` and
//!   `ue_noinfo_count` for those without a channel, and a `dimm<n>` for
//!   each bank and channel that had one (`dimm_label`, `dimm_location`,
//!   `dimm_ce_count`, `dimm_ue_count`), in the order they first did.
//!
//! Every file ends with a newline. The tree is read-only.

use core::fmt::{self, Write};

use radix_block::DeviceRef;
use radix_hal::{Devices, MemoryKind, MemoryRegion, Smp, Watchdog};
use radix_klog::TextBuf;
use radix_sync::Once;
use radix_vfs::synthetic::{FixedDir, Generated, Generator, Link, ListedDir, Listing, SyntheticFs};
use radix_vfs::{Error, FsKind, FsRef, KArc, Kind, NodeRef, Result, Text, mem, next_id};

use radix_uacpi::thermal::STEPS;

use super::disk;
use crate::acpi::{devices, thermal};

mod usb;
use crate::{Arch, hwmon, i2c, pstate};

static COMMAND_LINE: Once<&'static str> = Once::new();
static ROOT: Once<KArc<FixedDir>> = Once::new();

/// Records the kernel's command line, for `/kernel/cmdline`.
pub fn set_command_line(line: &'static str) {
    let _ = COMMAND_LINE.set(line);
}

/// A `sys` filesystem: every mount shows the same tree. Kept out of line,
/// so that [`super::make`]'s frame, under every mount and the deep ones
/// like a Btrfs disk's, does not carry its own.
#[inline(never)]
pub fn make() -> Result<FsRef> {
    if let Some(root) = ROOT.get() {
        let root: NodeRef = root.clone();
        return SyntheticFs::make(FsKind::Sys, root);
    }
    let root = FixedDir::new(next_id())?;
    root.add("version", Generated::from_fn(next_id(), version)?)?;
    root.add("cmdline", Generated::from_fn(next_id(), cmdline)?)?;
    root.add("cpus", Generated::from_fn(next_id(), cpus)?)?;
    let block: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(Blocks)?;
    root.add("block", ListedDir::make(next_id(), block)?)?;
    let bus = root.add_dir("bus", next_id())?;
    let pci = bus.add_dir("pci", next_id())?;
    let devices: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(PciDevices)?;
    pci.add("devices", ListedDir::make(next_id(), devices)?)?;
    usb::add(&bus)?;
    if i2c::found() {
        let i2c = bus.add_dir("i2c", next_id())?.add_dir("devices", next_id())?;
        let adapter = i2c.add_dir("i2c-0", next_id())?;
        adapter.add(
            "name",
            value(|text| {
                Arch::smbus_name(text)?;
                writeln!(text)
            })?,
        )?;
    }
    let firmware = root.add_dir("firmware", next_id())?;
    let memmap: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(MemoryMap)?;
    firmware.add("memmap", ListedDir::make(next_id(), memmap)?)?;
    let power = devices::batteries().next().is_some() || devices::adapters().next().is_some();
    let thermal = thermal::zones().next().is_some();
    let mut monitors = false;
    hwmon::devices(&mut |_| monitors = true);
    let dmi = crate::dmi::found();
    let class = if crate::watchdog::found().is_some() || power || thermal || monitors || dmi {
        Some(root.add_dir("class", next_id())?)
    } else {
        None
    };
    if let Some(found) = crate::watchdog::found()
        && let Some(class) = &class
    {
        let dir = class.add_dir("watchdog", next_id())?.add_dir("watchdog0", next_id())?;
        let Watchdog { name, min_seconds, max_seconds, restarted } = *found;
        dir.add("identity", value(move |text| writeln!(text, "{name}"))?)?;
        dir.add(
            "state",
            value(|text| {
                let active = crate::watchdog::state().0;
                writeln!(text, "{}", if active { "active" } else { "inactive" })
            })?,
        )?;
        dir.add("timeout", value(|text| writeln!(text, "{}", crate::watchdog::state().1))?)?;
        dir.add("min_timeout", value(move |text| writeln!(text, "{min_seconds}"))?)?;
        dir.add("max_timeout", value(move |text| writeln!(text, "{max_seconds}"))?)?;
        let status = if restarted { 32 } else { 0 };
        dir.add("bootstatus", value(move |text| writeln!(text, "{status}"))?)?;
    }
    if dmi && let Some(class) = &class {
        dmi_class(class)?;
    }
    if power && let Some(class) = &class {
        power_supply(class)?;
    }
    if thermal && let Some(class) = &class {
        thermal_class(class)?;
    }
    if monitors && let Some(class) = &class {
        hwmon_class(class)?;
    }
    let system = root.add_dir("devices", next_id())?.add_dir("system", next_id())?;
    cpu(&system)?;
    if crate::mce::on() {
        let mc = system.add_dir("edac", next_id())?.add_dir("mc", next_id())?;
        let mc0: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(MemoryController)?;
        mc.add("mc0", ListedDir::make(next_id(), mc0)?)?;
    }
    // Another thread may have made one meanwhile; either will do.
    let root = match ROOT.set(root) {
        Ok(root) => root.clone(),
        Err(root) => root,
    };
    let root: NodeRef = root;
    SyntheticFs::make(FsKind::Sys, root)
}

fn version(text: &mut Text) -> fmt::Result {
    writeln!(text, "{}", crate::BANNER)
}

fn cmdline(text: &mut Text) -> fmt::Result {
    writeln!(text, "{}", COMMAND_LINE.get().copied().unwrap_or(""))
}

fn cpus(text: &mut Text) -> fmt::Result {
    writeln!(text, "{}", crate::sched::online().len())
}

/// `/kernel/class/power_supply`: the firmware's batteries and AC adapters.
fn power_supply(class: &FixedDir) -> Result<()> {
    let supplies = class.add_dir("power_supply", next_id())?;
    for battery in devices::batteries() {
        let dir = supplies.add_dir(battery.name(), next_id())?;
        for &(name, file) in devices::battery_files(battery) {
            dir.add(name, value(move |text| devices::write_battery_file(battery, file, text))?)?;
        }
    }
    for adapter in devices::adapters() {
        let dir = supplies.add_dir(adapter.name(), next_id())?;
        dir.add("type", value(|text| writeln!(text, "Mains"))?)?;
        dir.add(
            "online",
            value(move |text| writeln!(text, "{}", u8::from(adapter.online().ok_or(fmt::Error)?)))?,
        )?;
    }
    Ok(())
}

/// `/kernel/class/thermal`: the firmware's thermal zones, and the CPUs
/// their passive cooling slows.
fn thermal_class(class: &FixedDir) -> Result<()> {
    let dir = class.add_dir("thermal", next_id())?;
    if pstate::driver().is_some() {
        for (index, _) in pstate::cpus().enumerate() {
            let mut name = TextBuf::<24>::new();
            let _ = write!(name, "cooling_device{index}");
            let device = dir.add_dir(name.as_str(), next_id())?;
            device.add("type", value(|text| writeln!(text, "Processor"))?)?;
            device.add("max_state", value(|text| writeln!(text, "{STEPS}"))?)?;
            device.add("cur_state", value(|text| writeln!(text, "{}", pstate::step()))?)?;
        }
    }
    for (index, zone) in thermal::zones().enumerate() {
        let mut name = TextBuf::<24>::new();
        let _ = write!(name, "thermal_zone{index}");
        let zone_dir = dir.add_dir(name.as_str(), next_id())?;
        zone_dir.add("type", value(|text| writeln!(text, "acpitz"))?)?;
        zone_dir.add(
            "temp",
            value(move |text| {
                let now = zone.temperature().ok_or(fmt::Error)?;
                writeln!(text, "{}", now.millicelsius())
            })?,
        )?;
        zone_dir.add("mode", value(|text| writeln!(text, "enabled"))?)?;
        zone_dir.add("policy", value(|text| writeln!(text, "step_wise"))?)?;
        zone_dir.add("available_policies", value(|text| writeln!(text, "step_wise"))?)?;
        for (trip, (kind, _)) in zone.trips().listed().enumerate() {
            let mut name = TextBuf::<32>::new();
            let _ = write!(name, "trip_point_{trip}_type");
            zone_dir.add(name.as_str(), value(move |text| writeln!(text, "{kind}"))?)?;
            name.clear();
            let _ = write!(name, "trip_point_{trip}_temp");
            // As the firmware last said, which it may change (0x81).
            let temp = move |text: &mut Text| {
                let (_, at) =
                    zone.trips().listed().find(|&(now, _)| now == kind).ok_or(fmt::Error)?;
                writeln!(text, "{}", at.millicelsius())
            };
            zone_dir.add(name.as_str(), value(temp)?)?;
            name.clear();
            let _ = write!(name, "trip_point_{trip}_hyst");
            zone_dir.add(name.as_str(), value(|text| writeln!(text, "0"))?)?;
        }
    }
    Ok(())
}

/// `/kernel/class/dmi/id`: the firmware's names for the machine.
fn dmi_class(class: &FixedDir) -> Result<()> {
    let id = class.add_dir("dmi", next_id())?.add_dir("id", next_id())?;
    let mut result = Ok(());
    crate::dmi::files(&mut |name, file| {
        if result.is_ok() {
            result =
                value(move |text| writeln!(text, "{file}")).and_then(|node| id.add(name, node));
        }
    });
    result
}

/// `/kernel/class/hwmon`: the temperature sensors.
fn hwmon_class(class: &FixedDir) -> Result<()> {
    let dir = class.add_dir("hwmon", next_id())?;
    let mut index = 0;
    let mut result = Ok(());
    hwmon::devices(&mut |device| {
        if result.is_ok() {
            result = hwmon_device(&dir, index, device);
            index += 1;
        }
    });
    result
}

/// `hwmon<index>`, for `device`.
fn hwmon_device(dir: &FixedDir, index: usize, device: hwmon::Device) -> Result<()> {
    let mut name = TextBuf::<16>::new();
    let _ = write!(name, "hwmon{index}");
    let device_dir = dir.add_dir(name.as_str(), next_id())?;
    let device_name = device.name();
    device_dir.add("name", value(move |text| writeln!(text, "{device_name}"))?)?;
    let mut result = Ok(());
    hwmon::channels(device, &mut |channel| {
        for &file in channel.files() {
            if result.is_err() {
                return;
            }
            let mut name = TextBuf::<32>::new();
            let _ = write!(name, "{}{}_{}", channel.kind(), channel.number(), file.suffix());
            result = value(move |text| hwmon::write_file(channel, file, text))
                .and_then(|generated| device_dir.add(name.as_str(), generated));
        }
    });
    result
}

/// `/kernel/devices/system/cpu`: the CPUs' lists, and each CPU's
/// frequency's policy where they pick their own speed (ADR 0202).
fn cpu(system: &FixedDir) -> Result<()> {
    let dir = system.add_dir("cpu", next_id())?;
    let present = |text: &mut Text| match Arch::cpu_count() {
        0 | 1 => writeln!(text, "0"),
        count => writeln!(text, "0-{}", count - 1),
    };
    dir.add("possible", value(present)?)?;
    dir.add("present", value(present)?)?;
    dir.add("online", value(|text| writeln!(text, "{}", crate::sched::online()))?)?;
    let driver = pstate::driver();
    let cpufreq = match driver {
        Some(_) => Some(dir.add_dir("cpufreq", next_id())?),
        None => None,
    };
    for cpu in 0..Arch::cpu_count() {
        let mut name = TextBuf::<16>::new();
        let _ = write!(name, "cpu{cpu}");
        let cpu_dir = dir.add_dir(name.as_str(), next_id())?;
        let (Some(driver), Some(cpufreq)) = (driver, &cpufreq) else { continue };
        if !pstate::cpus().any(|on| on == cpu) {
            continue;
        }
        let mut name = TextBuf::<16>::new();
        let _ = write!(name, "policy{cpu}");
        let policy = cpufreq.add_dir(name.as_str(), next_id())?;
        for &(file_name, file) in pstate::files(driver) {
            policy.add(file_name, value(move |text| pstate::write_file(cpu, file, text))?)?;
        }
        let mut target = TextBuf::<32>::new();
        let _ = write!(target, "../cpufreq/policy{cpu}");
        cpu_dir.add("cpufreq", Link::make(next_id(), target.as_str())?)?;
    }
    Ok(())
}

/// A file of one value, written when it is opened.
fn value(generate: impl Fn(&mut Text) -> fmt::Result + Send + Sync + 'static) -> Result<NodeRef> {
    let generate: Generator = mem::boxed(generate)?;
    Generated::make(next_id(), generate)
}

/// `/kernel/block`: a directory per disk and partition.
struct Blocks;

impl Listing for Blocks {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        let mut more = true;
        disk::for_each(|entry| {
            if more {
                more = f(entry.name.as_str(), Kind::Directory);
            }
        });
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let mut found: Option<(DeviceRef, Option<disk::Place>, Option<disk::Ident>)> = None;
        disk::for_each(|entry| {
            if entry.name.as_str() == name {
                found = Some((entry.device.clone(), entry.place, entry.ident));
            }
        });
        let (device, place, ident) = found.ok_or(Error::NotFound)?;
        let dir = FixedDir::new(next_id())?;
        let info = device.info();
        dir.add("size", value(move |text| writeln!(text, "{}", info.sectors))?)?;
        dir.add("ro", value(move |text| writeln!(text, "{}", u8::from(info.read_only)))?)?;
        // A partition has its number and first sector, and its disk the
        // queue, as on Linux.
        if let Some(place) = place {
            dir.add("partition", value(move |text| writeln!(text, "{}", place.number))?)?;
            dir.add("start", value(move |text| writeln!(text, "{}", place.first))?)?;
            dir.add("parttype", value(move |text| writeln!(text, "{}", place.kind))?)?;
            return Ok(dir);
        }
        // What the disk says it is, where Linux has an NVMe controller's.
        if let Some(ident) = ident {
            let device = dir.add_dir("device", next_id())?;
            device.add("model", value(move |text| writeln!(text, "{}", ident.model()))?)?;
            device.add("serial", value(move |text| writeln!(text, "{}", ident.serial()))?)?;
            let firmware = value(move |text| writeln!(text, "{}", ident.firmware()))?;
            device.add("firmware_rev", firmware)?;
        }
        if device.stats().is_some() {
            let device = device.clone();
            dir.add("stat", value(move |text| stat(text, &device.stats().unwrap_or_default()))?)?;
        }
        let queue = dir.add_dir("queue", next_id())?;
        let (sector, logical) = (radix_block::SECTOR, u64::from(info.logical_block));
        for (name, number) in [
            ("logical_block_size", logical),
            ("physical_block_size", u64::from(info.block_size)),
            ("minimum_io_size", u64::from(info.block_size)),
            ("optimal_io_size", 0),
            ("rotational", u64::from(info.rotational)),
            ("discard_granularity", if info.discard_sectors > 0 { logical } else { 0 }),
            ("discard_max_bytes", info.discard_sectors.saturating_mul(sector)),
        ] {
            queue.add(name, value(move |text| writeln!(text, "{number}"))?)?;
        }
        Ok(dir)
    }
}

/// A disk's `stat`, as Linux writes it: reads, reads merged, sectors
/// read and milliseconds reading; the same of writes; requests in
/// flight, milliseconds busy and milliseconds of every request; the same
/// four of discards; flushes and milliseconds flushing. Radix merges no
/// requests, so those are 0.
fn stat(text: &mut Text, stats: &radix_block::Stats) -> fmt::Result {
    use radix_block::Stats;
    let ms = |ns: u64| ns / 1_000_000;
    let all_ns: u64 = stats.ns.iter().sum();
    let fields = [
        stats.requests[Stats::READ],
        0,
        stats.sectors[Stats::READ],
        ms(stats.ns[Stats::READ]),
        stats.requests[Stats::WRITE],
        0,
        stats.sectors[Stats::WRITE],
        ms(stats.ns[Stats::WRITE]),
        stats.in_flight,
        ms(stats.busy_ns),
        ms(all_ns),
        stats.requests[Stats::DISCARD],
        0,
        stats.sectors[Stats::DISCARD],
        ms(stats.ns[Stats::DISCARD]),
        stats.requests[Stats::FLUSH],
        ms(stats.ns[Stats::FLUSH]),
    ];
    for (i, field) in fields.iter().enumerate() {
        let gap = if i == 0 { "" } else { " " };
        write!(text, "{gap}{field:>8}")?;
    }
    text.write_char('\n')
}

/// `/kernel/firmware/memmap`: a directory per range of the firmware's
/// memory map, numbered in its order.
struct MemoryMap;

/// The firmware's memory map, as the kernel was given it.
fn memory_map() -> &'static [MemoryRegion] {
    crate::boot_summary().map_or(&[], |boot| boot.memory_map)
}

/// Linux's name for a kind of memory, as its `/sys/firmware/memmap`
/// gives it.
fn linux_name(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Usable => "System RAM",
        MemoryKind::Reserved => "Reserved",
        MemoryKind::AcpiReclaimable => "ACPI Tables",
        MemoryKind::AcpiNvs => "ACPI Non-volatile Storage",
        MemoryKind::Defective => "Unusable memory",
    }
}

impl Listing for MemoryMap {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        for index in 0..memory_map().len() {
            let mut name = TextBuf::<8>::new();
            let _ = write!(name, "{index}");
            if !f(name.as_str(), Kind::Directory) {
                break;
            }
        }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let index: usize = name.parse().map_err(|_| Error::NotFound)?;
        // "1" and not "01": one name for each.
        if name.len() > 1 && name.starts_with('0') {
            return Err(Error::NotFound);
        }
        let region = *memory_map().get(index).ok_or(Error::NotFound)?;
        let dir = FixedDir::new(next_id())?;
        dir.add("start", value(move |text| writeln!(text, "{:#x}", region.start))?)?;
        dir.add("end", value(move |text| writeln!(text, "{:#x}", region.end - 1))?)?;
        dir.add("type", value(move |text| writeln!(text, "{}", linux_name(region.kind)))?)?;
        Ok(dir)
    }
}

/// `/kernel/bus/pci/devices`: a directory per PCI function.
struct PciDevices;

fn address_of(function: &disk::PciFunction) -> TextBuf<16> {
    let mut name = TextBuf::<16>::new();
    let _ = write!(name, "{}", function.address);
    name
}

impl Listing for PciDevices {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        let mut more = true;
        disk::pci_functions(|function| {
            if more {
                more = f(address_of(function).as_str(), Kind::Directory);
            }
        });
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let mut found = None;
        disk::pci_functions(|function| {
            if address_of(function).as_str() == name {
                found = Some(*function);
            }
        });
        let function = found.ok_or(Error::NotFound)?;
        let dir = FixedDir::new(next_id())?;
        dir.add("vendor", value(move |text| writeln!(text, "{:#06x}", function.vendor))?)?;
        dir.add("device", value(move |text| writeln!(text, "{:#06x}", function.device))?)?;
        dir.add("class", value(move |text| writeln!(text, "{:#08x}", function.class))?)?;
        dir.add("revision", value(move |text| writeln!(text, "{:#04x}", function.revision))?)?;
        let (vendor, device) = function.subsystem;
        dir.add("subsystem_vendor", value(move |text| writeln!(text, "{vendor:#06x}"))?)?;
        dir.add("subsystem_device", value(move |text| writeln!(text, "{device:#06x}"))?)?;
        Ok(dir)
    }
}

/// `/kernel/devices/system/edac/mc/mc0`: the memory errors the
/// machine-check banks reported since boot (`crate::mce`), as one memory
/// controller whose DIMMs are the banks' channels.
struct MemoryController;

/// mc0's files, besides the `dimm<n>` directories.
const MC_FILES: [&str; 6] = [
    "mc_name",
    "seconds_since_reset",
    "ce_count",
    "ce_noinfo_count",
    "ue_count",
    "ue_noinfo_count",
];

/// The channel `dimm<n>` names: `n` without leading zeros.
fn dimm_index(name: &str) -> Option<usize> {
    let digits = name.strip_prefix("dimm")?;
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    digits.parse().ok()
}

impl Listing for MemoryController {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        for name in MC_FILES {
            if !f(name, Kind::File) {
                return;
            }
        }
        for index in 0..crate::mce::memory_errors().channels().len() {
            let mut name = TextBuf::<16>::new();
            let _ = write!(name, "dimm{index}");
            if !f(name.as_str(), Kind::Directory) {
                return;
            }
        }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        use crate::mce::memory_errors;
        let count = |pick: fn(&crate::mce::MemoryErrors) -> u64| {
            value(move |text| writeln!(text, "{}", pick(&memory_errors())))
        };
        match name {
            "mc_name" => value(|text| writeln!(text, "Machine check banks")),
            "seconds_since_reset" => {
                value(|text| writeln!(text, "{}", crate::sched::now_ns() / 1_000_000_000))
            }
            "ce_count" => count(|errors| errors.corrected),
            "ce_noinfo_count" => count(|errors| errors.corrected_unplaced),
            "ue_count" => count(|errors| errors.uncorrected),
            "ue_noinfo_count" => count(|errors| errors.uncorrected_unplaced),
            _ => {
                let index = dimm_index(name).ok_or(Error::NotFound)?;
                let place = *memory_errors().channels().get(index).ok_or(Error::NotFound)?;
                let (bank, channel) = (place.bank, place.channel);
                let dir = FixedDir::new(next_id())?;
                let label = move |text: &mut Text| writeln!(text, "bank {bank} channel {channel}");
                dir.add("dimm_label", value(label)?)?;
                dir.add("dimm_location", value(label)?)?;
                let counter = move |corrected: bool| {
                    value(move |text| {
                        let errors = memory_errors();
                        let place = errors.channels().get(index).copied().unwrap_or_default();
                        let count = if corrected { place.corrected } else { place.uncorrected };
                        writeln!(text, "{count}")
                    })
                };
                dir.add("dimm_ce_count", counter(true)?)?;
                dir.add("dimm_ue_count", counter(false)?)?;
                Ok(dir)
            }
        }
    }
}
