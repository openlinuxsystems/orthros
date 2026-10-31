// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Chromium embedded controller of Framework's machines (Phase 11e,
//! ADR 0207), through `radix-crosec`: its fans and temperature sensors,
//! which `/kernel/class/hwmon` shows as Linux's `cros_ec` hwmon device
//! does.
//!
//! [`find`] looks for it once uACPI has started: the firmware's ACPI
//! device for it, Framework's `FRMWC004` with the memory map at 0xe00, or
//! Chromebooks' `GOOG0004` with it at 0x900, as Linux's `cros_ec_lpcs`
//! matches. The kernel then checks that the EC answers, logs its
//! firmware's version, its fans and its sensors, and asks each sensor its
//! name. Its host commands are one at a time, under a sleeping lock.

use core::fmt::Write;

use radix_crosec::{Absent, Ec, Fan, Image, Platform, Temperature};
use radix_hal::{Devices, Timer};
use radix_klog::{TextBuf, info, warn};
use radix_sync::{Mutex, Once, SpinLock};

use crate::{Arch, acpi, sched};

/// The EC's ACPI devices, and where each has its memory map.
const DEVICES: [(&core::ffi::CStr, &str, u16); 2] = [
    (c"FRMWC004", "Framework's", radix_crosec::FRAMEWORK_MEMMAP),
    (c"GOOG0004", "the Chromium", radix_crosec::MEMMAP),
];

/// The EC's ports, through the HAL.
struct Ports;

impl Platform for Ports {
    fn in8(&self, port: u16) -> u8 {
        Arch::ec_read(port)
    }

    fn out8(&self, port: u16, value: u8) {
        Arch::ec_write(port, value);
    }

    fn delay_us(&self, us: u64) {
        if us >= 1000 {
            sched::sleep_ns(us * 1000);
        } else {
            Arch::delay_us(us);
        }
    }
}

/// The EC found.
struct Found {
    ec: Mutex<Ec<Ports>>,
    /// The fans the memory map has, a bit each.
    fans: u8,
    /// The sensors it has, a bit each.
    sensors: u32,
}

static FOUND: Once<Found> = Once::new();

/// Each sensor's name, as the EC gives it.
static NAMES: [SpinLock<TextBuf<32>>; radix_crosec::TEMP_SENSORS as usize] =
    [const { SpinLock::new(TextBuf::new()) }; radix_crosec::TEMP_SENSORS as usize];

/// Looks for the EC, and logs it. Called once, once uACPI has started.
pub fn find() {
    if !acpi::running() {
        return;
    }
    let mut device = None;
    for &(hid, whose, memmap) in &DEVICES {
        radix_uacpi::find_devices(hid, &mut |_| device = Some((hid, whose, memmap)));
        if device.is_some() {
            break;
        }
    }
    let Some((hid, whose, memmap)) = device else { return };
    let hid = hid.to_str().unwrap_or("");
    let mut ec = match Ec::probe(Ports, memmap) {
        Ok(ec) => ec,
        Err(why @ (Absent::NoMap | Absent::OldProtocol)) => {
            warn!("EC: the firmware names {hid}, but {why} at port {memmap:#x}; it is not used");
            return;
        }
    };
    if let Err(why) = ec.hello() {
        warn!("EC: {hid} does not answer its first command: {why}; it is not used");
        return;
    }
    let mut version = TextBuf::<40>::new();
    let image = match ec.version(&mut version) {
        Ok(Image::ReadOnly) => "read-only",
        Ok(Image::ReadWrite) => "read-write",
        Ok(Image::Unknown) | Err(_) => "unknown",
    };
    let fans = (0..radix_crosec::FANS)
        .filter(|&fan| ec.fan(fan) != Fan::Absent)
        .fold(0u8, |bits, fan| bits | 1 << fan);
    let mut sensors = 0u32;
    for sensor in 0..radix_crosec::TEMP_SENSORS {
        if ec.temperature(sensor) == Temperature::Absent {
            continue;
        }
        sensors |= 1 << sensor;
        let mut name = TextBuf::<32>::new();
        if ec.sensor_name(sensor, &mut name).is_err() || name.is_empty() {
            name.clear();
            let _ = write!(name, "sensor {sensor}");
        }
        *NAMES[usize::from(sensor)].lock() = name;
    }
    let found = Found { ec: Mutex::new(ec), fans, sensors };
    let (fans, sensors) = (fans.count_ones(), sensors.count_ones());
    info!(
        "EC: {whose} embedded controller ({hid}), firmware {version} ({image} image), with \
         {fans} fan{} and {sensors} temperature sensor{}",
        if fans == 1 { "" } else { "s" },
        if sensors == 1 { "" } else { "s" },
    );
    let _ = FOUND.set(found);
}

/// Whether the EC was found.
pub fn found() -> bool {
    FOUND.get().is_some()
}

/// The fans the EC has, by their numbers in its memory map.
pub fn fans() -> impl Iterator<Item = u8> {
    let bits = FOUND.get().map_or(0, |found| found.fans);
    (0..radix_crosec::FANS).filter(move |fan| bits & 1 << fan != 0)
}

/// The temperature sensors it has.
pub fn sensors() -> impl Iterator<Item = u8> {
    let bits = FOUND.get().map_or(0, |found| found.sensors);
    (0..radix_crosec::TEMP_SENSORS).filter(move |sensor| bits & 1 << sensor != 0)
}

/// What fan `fan` says now.
pub fn fan(fan: u8) -> Fan {
    FOUND.get().map_or(Fan::Absent, |found| found.ec.lock().fan(fan))
}

/// What sensor `sensor` says now.
pub fn temperature(sensor: u8) -> Temperature {
    FOUND.get().map_or(Temperature::Absent, |found| found.ec.lock().temperature(sensor))
}

/// Writes sensor `sensor`'s name, as the EC gave it.
pub fn write_sensor_name(sensor: u8, out: &mut dyn Write) -> core::fmt::Result {
    let name = NAMES.get(usize::from(sensor)).ok_or(core::fmt::Error)?.lock().clone();
    writeln!(out, "{}", name.as_str())
}
