// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The firmware's power devices, through uACPI (Phase 11c, ADR 0201):
//! batteries (`PNP0C0A`), AC adapters (`ACPI0003`), lids (`PNP0C0D`) and
//! power buttons of the namespace (`PNP0C0C`), found once the namespace is
//! initialized.
//!
//! A battery's description (`_BIX`, else `_BIF`) is read when it is found
//! and again after the firmware notifies 0x81; its state (`_BST`) when one
//! of its files is read, at most once a second as Linux's `cache_time`
//! has it, or at once after a notify of 0x80. An adapter's `_PSR` and a
//! lid's `_LID` are read at each look. `/kernel/class/power_supply` shows
//! the batteries and adapters in Linux's layout (`fs::sysfs`), and
//! `/processes/acpi/button/lid` the lids (`fs::procfs`). AML is never run
//! under a lock: on a laptop it reads the embedded controller, which
//! takes milliseconds.

use core::ffi::CStr;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use radix_hal::Timer;
use radix_klog::{info, warn};
use radix_sync::{Once, SpinLock};
use radix_uacpi::power::{Info, State};
use radix_uacpi::{Node, NodeName};

use crate::Arch;

const MAX_BATTERIES: usize = 4;
const MAX_ADAPTERS: usize = 2;
pub const MAX_LIDS: usize = 2;
const MAX_BUTTONS: usize = 2;

/// How long a battery's state is kept before a look reads it again.
const STATE_LIFE_NS: u64 = 1_000_000_000;

/// `_STA`: a battery is in its bay.
const BATTERY_PRESENT: u64 = 1 << 4;

/// Notify values: a battery's or adapter's state changed; a battery's
/// description changed (another battery, or a new full capacity).
const STATUS_CHANGED: u64 = 0x80;
const INFO_CHANGED: u64 = 0x81;

/// A battery.
pub struct Battery {
    node: Node,
    name: NodeName,
    info_stale: AtomicBool,
    state_stale: AtomicBool,
    last: SpinLock<Reading>,
}

/// What a battery said when last read.
#[derive(Clone, Copy, Default)]
pub struct Reading {
    /// Whether it is in its bay.
    pub present: bool,
    pub info: Option<Info>,
    pub state: Option<State>,
    /// When the state was read.
    read_ns: u64,
}

/// An AC adapter.
pub struct Adapter {
    node: Node,
    name: NodeName,
}

/// A lid.
pub struct Lid {
    node: Node,
    name: NodeName,
}

static BATTERIES: [Once<Battery>; MAX_BATTERIES] = [const { Once::new() }; MAX_BATTERIES];
static ADAPTERS: [Once<Adapter>; MAX_ADAPTERS] = [const { Once::new() }; MAX_ADAPTERS];
static LIDS: [Once<Lid>; MAX_LIDS] = [const { Once::new() }; MAX_LIDS];
static BUTTONS: [Once<Node>; MAX_BUTTONS] = [const { Once::new() }; MAX_BUTTONS];

/// What a press of a power button of the namespace calls.
static PRESSED: Once<fn()> = Once::new();

/// The batteries found.
pub fn batteries() -> impl Iterator<Item = &'static Battery> {
    BATTERIES.iter().map_while(Once::get)
}

/// The AC adapters found.
pub fn adapters() -> impl Iterator<Item = &'static Adapter> {
    ADAPTERS.iter().map_while(Once::get)
}

/// The lids found.
pub fn lids() -> impl Iterator<Item = &'static Lid> {
    LIDS.iter().map_while(Once::get)
}

/// Puts the devices with `hid` into `slots`, in the order uACPI finds
/// them.
fn collect<T>(hid: &CStr, what: &str, slots: &'static [Once<T>], make: impl Fn(Node) -> T) {
    let mut count = 0;
    radix_uacpi::find_devices(hid, &mut |node| {
        if let Some(slot) = slots.get(count) {
            let _ = slot.set(make(node));
        }
        count += 1;
    });
    if count > slots.len() {
        warn!("ACPI: the firmware has {count} {what}; the kernel shows the first {}", slots.len());
    }
}

/// Finds the devices, hears their notifies and logs what they are and
/// say. Called once, once uACPI has started.
pub fn find() {
    collect(c"PNP0C0A", "batteries", &BATTERIES, |node| Battery {
        node,
        name: node.name_text(),
        info_stale: AtomicBool::new(true),
        state_stale: AtomicBool::new(true),
        last: SpinLock::new(Reading::default()),
    });
    collect(c"ACPI0003", "AC adapters", &ADAPTERS, |node| Adapter { node, name: node.name_text() });
    collect(c"PNP0C0D", "lids", &LIDS, |node| Lid { node, name: node.name_text() });
    collect(c"PNP0C0C", "power buttons", &BUTTONS, |node| node);

    for battery in batteries() {
        listen(battery.node, battery.name, battery_notified);
        let mut line = Line::new();
        let _ = battery.read().describe(&mut line);
        info!("Power: {}: {}", battery.name, line.as_str());
    }
    for adapter in adapters() {
        listen(adapter.node, adapter.name, adapter_notified);
        let state = match adapter.online() {
            Some(true) => "plugged in",
            Some(false) => "unplugged",
            None => "that does not say whether it is plugged in",
        };
        info!("Power: {}: an AC adapter, {state}", adapter.name);
    }
    for lid in lids() {
        listen(lid.node, lid.name, lid_notified);
        match lid.open() {
            Some(open) => info!("Lid: {} is {}", lid.name, if open { "open" } else { "closed" }),
            None => info!("Lid: {} does not say whether it is open", lid.name),
        }
    }
}

fn listen(node: Node, name: NodeName, handler: fn(Node, u64)) {
    if let Err(err) = node.on_notify(handler) {
        warn!("ACPI: {name}'s changes cannot be heard: {err}");
    }
}

/// Hears the power buttons of the namespace: each press calls `pressed`,
/// in the notify thread. Returns their names, as `PWRB`, joined by "and".
pub fn power_buttons(pressed: fn()) -> Option<Line> {
    let _ = PRESSED.set(pressed);
    let mut names = Line::new();
    for button in BUTTONS.iter().map_while(Once::get) {
        if button.on_notify(button_notified).is_ok() {
            let and = if names.as_str().is_empty() { "" } else { " and " };
            let _ = write!(names, "{and}{}", button.name_text());
        }
    }
    (!names.as_str().is_empty()).then_some(names)
}

fn button_notified(_: Node, value: u64) {
    if value == STATUS_CHANGED
        && let Some(pressed) = PRESSED.get()
    {
        pressed();
    }
}

fn battery_notified(node: Node, value: u64) {
    let Some(battery) = batteries().find(|battery| battery.node == node) else { return };
    if value == INFO_CHANGED {
        battery.info_stale.store(true, Ordering::Release);
    }
    if value == STATUS_CHANGED || value == INFO_CHANGED {
        battery.state_stale.store(true, Ordering::Release);
    }
}

fn adapter_notified(node: Node, value: u64) {
    let Some(adapter) = adapters().find(|adapter| adapter.node == node) else { return };
    if value == STATUS_CHANGED
        && let Some(online) = adapter.online()
    {
        info!("Power: {} is {}", adapter.name, if online { "plugged in" } else { "unplugged" });
    }
}

fn lid_notified(node: Node, value: u64) {
    let Some(lid) = lids().find(|lid| lid.node == node) else { return };
    if value == STATUS_CHANGED
        && let Some(open) = lid.open()
    {
        info!("Lid: {} {}", lid.name, if open { "opened" } else { "closed" });
    }
}

/// The description: `_BIX`, or `_BIF` on firmware older than ACPI 4.0.
fn read_info(node: Node) -> Option<Info> {
    match node.package(c"_BIX", |fields| Info::from_bix(fields)) {
        Ok(Some(info)) => Some(info),
        _ => node.package(c"_BIF", |fields| Info::from_bif(fields)).ok().flatten(),
    }
}

impl Battery {
    /// Its name, as `BAT0`.
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// What it says now: read again if a second old or notified of.
    pub fn read(&self) -> Reading {
        let now = Arch::now_ns();
        let last = *self.last.lock();
        let old = last.read_ns == 0 || now.saturating_sub(last.read_ns) >= STATE_LIFE_NS;
        let state_stale = self.state_stale.swap(false, Ordering::AcqRel);
        let info_stale = self.info_stale.swap(false, Ordering::AcqRel);
        if !old && !state_stale && !info_stale {
            return last;
        }
        // A battery without `_STA` is always there.
        let present =
            !matches!(self.node.integer(c"_STA"), Ok(status) if status & BATTERY_PRESENT == 0);
        let mut fresh = Reading { present, read_ns: now.max(1), ..Reading::default() };
        if present {
            fresh.info =
                if info_stale || last.info.is_none() { read_info(self.node) } else { last.info };
            fresh.state =
                self.node.package(c"_BST", |fields| State::from_bst(fields)).ok().flatten();
        }
        if fresh.info.is_none() && present {
            // Try again at the next look.
            self.info_stale.store(true, Ordering::Release);
        }
        *self.last.lock() = fresh;
        fresh
    }
}

impl Reading {
    /// One line on what it is and does, for the log.
    fn describe(&self, f: &mut dyn Write) -> fmt::Result {
        if !self.present {
            return f.write_str("an empty battery bay");
        }
        let Some(info) = self.info else {
            return f.write_str("a battery that does not say what it is");
        };
        let unit = if info.amperes { "mAh" } else { "mWh" };
        write!(f, "a {} battery", info.technology())?;
        if let Some(full) = info.capacity() {
            write!(f, " of {full} {unit}")?;
            if let Some(design) = info.design_capacity.filter(|&d| d != full) {
                write!(f, " ({design} {unit} new)")?;
            }
        }
        if let Some(state) = self.state {
            if let Some(percent) = state.percent(&info) {
                write!(f, ", {percent} % charged")?;
            }
            let status = match state.status(&info) {
                "Discharging" => "discharging",
                "Charging" => "charging",
                "Full" => "full",
                _ => "not charging",
            };
            write!(f, ", {status}")?;
        }
        Ok(())
    }
}

impl Adapter {
    /// Its name, as `ADP1`.
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Whether it is plugged in (`_PSR`).
    pub fn online(&self) -> Option<bool> {
        self.node.integer(c"_PSR").ok().map(|online| online != 0)
    }
}

impl Lid {
    /// Its name, as `LID0`.
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Whether it is open (`_LID`).
    pub fn open(&self) -> Option<bool> {
        self.node.integer(c"_LID").ok().map(|open| open != 0)
    }
}

/// The files of a battery's directory in `/kernel/class/power_supply`,
/// as Linux's ACPI battery has them: energies and powers for a battery
/// that counts in milliwatts, charges and currents for one in
/// milliamperes.
pub fn battery_files(battery: &Battery) -> &'static [(&'static str, BatteryFile)] {
    use BatteryFile::*;
    const WATTS: &[(&str, BatteryFile)] = &[
        ("type", Type),
        ("present", Present),
        ("status", Status),
        ("capacity", Capacity),
        ("capacity_level", CapacityLevel),
        ("technology", Technology),
        ("cycle_count", CycleCount),
        ("voltage_min_design", VoltageMinDesign),
        ("voltage_now", VoltageNow),
        ("energy_full_design", FullDesign),
        ("energy_full", Full),
        ("energy_now", Now),
        ("power_now", Rate),
        ("model_name", Model),
        ("manufacturer", Maker),
        ("serial_number", Serial),
    ];
    const AMPERES: &[(&str, BatteryFile)] = &[
        ("type", Type),
        ("present", Present),
        ("status", Status),
        ("capacity", Capacity),
        ("capacity_level", CapacityLevel),
        ("technology", Technology),
        ("cycle_count", CycleCount),
        ("voltage_min_design", VoltageMinDesign),
        ("voltage_now", VoltageNow),
        ("charge_full_design", FullDesign),
        ("charge_full", Full),
        ("charge_now", Now),
        ("current_now", Rate),
        ("model_name", Model),
        ("manufacturer", Maker),
        ("serial_number", Serial),
    ];
    let amperes = battery.last.lock().info.is_some_and(|info| info.amperes);
    if amperes { AMPERES } else { WATTS }
}

/// One of a battery's files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatteryFile {
    Type,
    Present,
    Status,
    Capacity,
    CapacityLevel,
    Technology,
    CycleCount,
    VoltageMinDesign,
    VoltageNow,
    FullDesign,
    Full,
    Now,
    Rate,
    Model,
    Maker,
    Serial,
}

/// Writes `file` of `battery`, a line, in Linux's units: micro-. A value
/// the battery does not know, or any but `type` and `present` while its
/// bay is empty, is an error, as Linux's ENODEV.
pub fn write_battery_file(
    battery: &Battery,
    file: BatteryFile,
    text: &mut dyn Write,
) -> fmt::Result {
    use BatteryFile::*;
    let reading = battery.read();
    match file {
        Type => return writeln!(text, "Battery"),
        Present => return writeln!(text, "{}", u8::from(reading.present)),
        _ => {}
    }
    let (Some(info), true) = (reading.info, reading.present) else { return Err(fmt::Error) };
    let micro = |value: Option<u32>| value.map(|v| u64::from(v) * 1000).ok_or(fmt::Error);
    let state = reading.state;
    match file {
        Status => writeln!(text, "{}", state.ok_or(fmt::Error)?.status(&info)),
        Capacity => writeln!(text, "{}", state.and_then(|s| s.percent(&info)).ok_or(fmt::Error)?),
        CapacityLevel => writeln!(text, "{}", state.ok_or(fmt::Error)?.level(&info)),
        Technology => writeln!(text, "{}", info.technology()),
        CycleCount => writeln!(text, "{}", info.cycle_count.ok_or(fmt::Error)?),
        VoltageMinDesign => writeln!(text, "{}", micro(info.design_voltage)?),
        VoltageNow => writeln!(text, "{}", micro(state.and_then(|s| s.voltage))?),
        FullDesign => writeln!(text, "{}", micro(info.design_capacity)?),
        Full => writeln!(text, "{}", micro(info.capacity())?),
        Now => writeln!(text, "{}", micro(state.and_then(|s| s.remaining))?),
        Rate => writeln!(text, "{}", micro(state.and_then(|s| s.rate))?),
        Model => writeln!(text, "{}", info.model.as_str()),
        Maker => writeln!(text, "{}", info.maker.as_str()),
        Serial => writeln!(text, "{}", info.serial.as_str()),
        Type | Present => Ok(()),
    }
}

/// A line of text for the log, on the stack.
pub type Line = radix_klog::TextBuf<96>;
