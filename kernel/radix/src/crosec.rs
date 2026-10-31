// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Chromium embedded controller of Framework's machines (Phase 11e,
//! ADR 0207), through `radix-crosec`: its fans and temperature sensors,
//! which `/kernel/class/hwmon` shows as Linux's `cros_ec` hwmon device
//! does, and the controls programs given the platform capability change
//! (radix-platform(2), ADR 0208): fan duties and curves, LEDs, and the
//! ARGB header's colors.
//!
//! [`find`] looks for it once uACPI has started: the firmware's ACPI
//! device for it, Framework's `FRMWC004` with the memory map at 0xe00, or
//! Chromebooks' `GOOG0004` with it at 0x900, as Linux's `cros_ec_lpcs`
//! matches. The kernel then checks that the EC answers, logs its
//! firmware's version, its fans and its sensors, asks each sensor its
//! name, and asks which of the controls' commands it takes and which LEDs
//! it has. Its host commands are one at a time, under a sleeping lock.
//!
//! A program that takes a fan from the EC, or gives one back, is logged
//! once for each change, so that a fan someone stopped is in the log.

use core::fmt::Write;
use core::sync::atomic::{AtomicU8, Ordering};

use radix_crosec::{Absent, Control, Ec, Error, Fan, FanCurve, Image, Led, Platform, Temperature};
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
    /// The LEDs it has, a bit each by the EC's number for it.
    leds: u32,
    /// The controls it has, a bit each in [`Control::ALL`]'s order.
    controls: u8,
}

static FOUND: Once<Found> = Once::new();

/// The fans a program took from the EC, a bit each.
static TAKEN: AtomicU8 = AtomicU8::new(0);

/// Why a control could not be used.
#[derive(Clone, Copy, Debug)]
pub enum Failed {
    /// There is no EC, or it lacks the control's commands.
    NotSupported,
    /// The EC has no such fan, sensor or LED.
    OutOfRange,
    /// The EC failed the command.
    Ec(Error),
}

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
    let leds = Led::ALL
        .into_iter()
        .filter(|&led| ec.led_range(led).is_ok())
        .fold(0u32, |bits, led| bits | 1 << led as u8);
    let controls = Control::ALL
        .into_iter()
        .enumerate()
        .filter(|&(_, control)| ec.has(control) == Ok(true))
        .fold(0u8, |bits, (at, _)| bits | 1 << at);
    let found = Found { ec: Mutex::new(ec), fans, sensors, leds, controls };
    let (fans, sensors, leds) = (fans.count_ones(), sensors.count_ones(), leds.count_ones());
    // What it takes, in words: "fan duties, LEDs and ARGB colors".
    let mut takes = TextBuf::<64>::new();
    let names = ["fan duties", "fan curves", "LEDs", "ARGB colors"];
    let count = controls.count_ones() as usize;
    let has = names.iter().enumerate().filter(|&(at, _)| controls & 1 << at != 0);
    for (n, (_, name)) in has.enumerate() {
        let joint = match n {
            0 => "",
            n if n + 1 == count => " and ",
            _ => ", ",
        };
        let _ = write!(takes, "{joint}{name}");
    }
    if count == 0 {
        let _ = takes.write_str("no controls");
    }
    info!(
        "EC: {whose} embedded controller ({hid}), firmware {version} ({image} image), with \
         {fans} fan{}, {sensors} temperature sensor{} and {leds} LED{}; it takes {takes}",
        if fans == 1 { "" } else { "s" },
        if sensors == 1 { "" } else { "s" },
        if leds == 1 { "" } else { "s" },
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

/// What the EC has: its controls (a bit each, in [`Control::ALL`]'s
/// order), fans, sensors and LEDs, a bit each; None without one.
pub fn describe() -> Option<(u8, u8, u32, u32)> {
    FOUND.get().map(|found| (found.controls, found.fans, found.sensors, found.leds))
}

/// The EC, if it has `control`.
fn with(control: Control) -> Result<&'static Found, Failed> {
    let found = FOUND.get().ok_or(Failed::NotSupported)?;
    let at = Control::ALL.iter().position(|&c| c == control).unwrap_or(0);
    if found.controls & 1 << at == 0 {
        return Err(Failed::NotSupported);
    }
    Ok(found)
}

impl Found {
    /// `fan`, if the EC has it.
    fn fan(&self, fan: u32) -> Result<u8, Failed> {
        match u8::try_from(fan) {
            Ok(fan) if fan < radix_crosec::FANS && self.fans & 1 << fan != 0 => Ok(fan),
            _ => Err(Failed::OutOfRange),
        }
    }

    /// `led`, if the EC has it.
    fn led(&self, led: u32) -> Result<Led, Failed> {
        let led = u8::try_from(led).ok().and_then(Led::from_id).ok_or(Failed::OutOfRange)?;
        if self.leds & 1 << led as u8 == 0 {
            return Err(Failed::OutOfRange);
        }
        Ok(led)
    }
}

/// Marks fan `fan` taken from the EC or given back, and logs the change
/// for `who`.
fn taken(fan: u8, take: bool, who: &str, percent: u8) {
    let bit = 1 << fan;
    let was = if take {
        TAKEN.fetch_or(bit, Ordering::AcqRel)
    } else {
        TAKEN.fetch_and(!bit, Ordering::AcqRel)
    };
    match (take, was & bit != 0) {
        (true, false) => info!("EC: `{who}` took fan {fan} from the EC, at {percent}%"),
        (false, true) => info!("EC: `{who}` gave fan {fan} back to the EC"),
        _ => {}
    }
}

/// Runs fan `fan` at `percent` (at most 100) of its duty, for `who`.
pub fn set_fan_duty(fan: u32, percent: u8, who: &str) -> Result<(), Failed> {
    let found = with(Control::FanDuty)?;
    let fan = found.fan(fan)?;
    found.ec.lock().set_fan_duty(fan, percent).map_err(Failed::Ec)?;
    taken(fan, true, who, percent);
    Ok(())
}

/// Gives fan `fan` back to the EC, for `who`.
pub fn auto_fan(fan: u32, who: &str) -> Result<(), Failed> {
    let found = with(Control::FanDuty)?;
    let fan = found.fan(fan)?;
    found.ec.lock().auto_fan(fan).map_err(Failed::Ec)?;
    taken(fan, false, who, 0);
    Ok(())
}

/// Fan `fan`'s curve.
pub fn fan_curve(fan: u32) -> Result<FanCurve, Failed> {
    let found = with(Control::FanCurve)?;
    let fan = found.fan(fan)?;
    found.ec.lock().fan_curve(fan).map_err(Failed::Ec)
}

/// Gives fan `fan` the curve `curve`, whose sensor must be one the EC
/// has, keeping the duty the EC gives it now.
pub fn set_fan_curve(fan: u32, curve: FanCurve) -> Result<(), Failed> {
    let found = with(Control::FanCurve)?;
    let fan = found.fan(fan)?;
    if u32::from(curve.sensor) >= u32::from(radix_crosec::TEMP_SENSORS)
        || found.sensors & 1 << curve.sensor == 0
    {
        return Err(Failed::OutOfRange);
    }
    let mut ec = found.ec.lock();
    let now = ec.fan_curve(fan).map_err(Failed::Ec)?;
    ec.set_fan_curve(fan, FanCurve { duty: now.duty, ..curve }).map(|_| ()).map_err(Failed::Ec)
}

/// The brightest LED `led` goes in each of its colors.
pub fn led_range(led: u32) -> Result<[u8; 6], Failed> {
    let found = with(Control::Leds)?;
    let led = found.led(led)?;
    found.ec.lock().led_range(led).map_err(Failed::Ec)
}

/// Lights LED `led` at `brightness` in each of its colors, or with None
/// gives it back to the EC.
pub fn set_led(led: u32, brightness: Option<[u8; 6]>) -> Result<(), Failed> {
    let found = with(Control::Leds)?;
    let led = found.led(led)?;
    let mut ec = found.ec.lock();
    match brightness {
        Some(brightness) => ec.set_led(led, brightness),
        None => ec.auto_led(led),
    }
    .map_err(Failed::Ec)
}

/// Gives the addressable LEDs from `first` on the colors `colors`.
pub fn set_rgb(first: u8, colors: &[[u8; 3]]) -> Result<(), Failed> {
    let found = with(Control::Rgb)?;
    found.ec.lock().set_rgb(first, colors).map_err(Failed::Ec)
}
