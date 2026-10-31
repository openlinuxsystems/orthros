// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What `/kernel/bus/usb/devices` shows (ADR 0230): each controller's
//! root hub, and each device its thread set up, with what its
//! descriptors and strings say, the interfaces of its configuration and
//! the drivers that took them, as Linux's usbcore shows them in
//! `/sys/bus/usb/devices`.
//!
//! A device is numbered on its bus as Linux numbers it: the root hub is
//! 1, and each device set up takes the next number free after the last
//! one given, from 2 to 127, then from 2 again.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU8, Ordering};

use radix_hw::pci::Address;
use radix_klog::TextBuf;
use radix_sync::{Once, SpinLock};
use radix_usb::{DeviceDescriptor, InterfaceDescriptor, Speed};

use super::MAX_CONTROLLERS;
use super::controller::Path;

/// The interfaces of a device shown, at most.
pub const MAX_INTERFACES: usize = 8;
/// The devices shown, at most, over every bus.
const MAX_SHOWN: usize = 64;
/// The highest number a device on a bus takes, as Linux's.
const MAX_NUMBER: u8 = 127;

/// One interface of a device's configuration, its first setting.
#[derive(Clone, Copy, Debug)]
pub struct Interface {
    pub descriptor: InterfaceDescriptor,
    /// The driver that took it, as Linux names it: `hub`, `usbhid`,
    /// `usb-storage` or `uas`.
    pub driver: Option<&'static str>,
}

impl Interface {
    /// None yet.
    pub const NONE: Interface = Interface {
        descriptor: InterfaceDescriptor {
            number: 0,
            alternate: 0,
            endpoints: 0,
            class: 0,
            subclass: 0,
            protocol: 0,
            string: 0,
        },
        driver: None,
    };
}

/// A device, as the sysfs shows it.
#[derive(Clone, Debug)]
pub struct Shown {
    /// Its bus, from 1.
    pub bus: u8,
    /// Its number on the bus.
    pub number: u8,
    pub path: Path,
    pub speed: Speed,
    pub device: DeviceDescriptor,
    /// Its configuration's bConfigurationValue, bNumInterfaces,
    /// bmAttributes, and bMaxPower in mA.
    pub configuration: u8,
    pub interface_count: u8,
    pub attributes: u8,
    pub max_power_ma: u16,
    /// The first `listed` of its interfaces.
    pub interfaces: [Interface; MAX_INTERFACES],
    pub listed: u8,
    /// A hub's ports; 0 for any other device.
    pub hub_ports: u8,
    /// Its strings, empty where it has none.
    pub manufacturer: TextBuf<64>,
    pub product: TextBuf<64>,
    pub serial: TextBuf<64>,
}

impl Shown {
    /// Its directory's name, as Linux's: `1-3.2`.
    pub fn name(&self) -> TextBuf<32> {
        let mut name = TextBuf::new();
        let _ = write!(name, "{}-", self.bus);
        let _ = self.write_devpath(&mut name);
        name
    }

    /// Its `devpath`: the ports from its root port down, `3.2`.
    pub fn write_devpath(&self, out: &mut dyn Write) -> fmt::Result {
        write!(out, "{}", self.path.root_port)?;
        for level in 0..self.path.depth() {
            write!(out, ".{}", (self.path.route >> (4 * level)) & 0xf)?;
        }
        Ok(())
    }

    /// Its interfaces listed.
    pub fn interfaces(&self) -> &[Interface] {
        &self.interfaces[..usize::from(self.listed).min(MAX_INTERFACES)]
    }
}

/// A controller's root hub.
#[derive(Clone, Copy, Debug)]
pub struct RootHub {
    /// Its root ports.
    pub ports: u8,
    /// Whether some of them are USB 3's.
    pub usb3: bool,
    /// The controller's PCI address, its serial number as Linux gives it.
    pub address: Address,
}

/// Each bus's root hub, by its bus less 1.
static ROOT_HUBS: [Once<RootHub>; MAX_CONTROLLERS] = [const { Once::new() }; MAX_CONTROLLERS];
/// The devices shown, in the order they came.
static SHOWN: SpinLock<[Option<Shown>; MAX_SHOWN]> = SpinLock::new([const { None }; MAX_SHOWN]);
/// The number last given on each bus.
static LAST: [AtomicU8; MAX_CONTROLLERS] = [const { AtomicU8::new(1) }; MAX_CONTROLLERS];

/// Shows bus `bus`'s root hub.
pub fn add_root_hub(bus: usize, hub: RootHub) {
    if let Some(slot) = bus.checked_sub(1).and_then(|at| ROOT_HUBS.get(at)) {
        let _ = slot.set(hub);
    }
}

/// Bus `bus`'s root hub, if it has one.
pub fn root_hub(bus: usize) -> Option<RootHub> {
    bus.checked_sub(1).and_then(|at| ROOT_HUBS.get(at)).and_then(Once::get).copied()
}

/// Shows `shown`, giving it the next number free on its bus; false if
/// there is no room, or no number.
pub fn add(shown: &Shown) -> bool {
    let Some(last) = usize::from(shown.bus).checked_sub(1).and_then(|at| LAST.get(at)) else {
        return false;
    };
    let mut all = SHOWN.lock();
    let Some(free) = all.iter().position(Option::is_none) else { return false };
    let taken = |number: u8| {
        all.iter().flatten().any(|other| other.bus == shown.bus && other.number == number)
    };
    // The numbers after the last one given, from 2 to 127 and round.
    let from = i32::from(last.load(Ordering::Relaxed)) - 2;
    let count = i32::from(MAX_NUMBER) - 1;
    let Some(number) = (1..=count)
        .map(|step| ((from + step).rem_euclid(count) + 2) as u8)
        .find(|&number| !taken(number))
    else {
        return false;
    };
    last.store(number, Ordering::Relaxed);
    let mut entry = shown.clone();
    entry.number = number;
    all[free] = Some(entry);
    true
}

/// Stops showing the device at `path` on bus `bus`.
pub fn remove(bus: usize, path: Path) {
    let mut all = SHOWN.lock();
    for slot in all.iter_mut() {
        if slot.as_ref().is_some_and(|s| usize::from(s.bus) == bus && s.path == path) {
            *slot = None;
        }
    }
}

/// Changes the device at `path` on bus `bus` as `change` does.
fn change(bus: usize, path: Path, change: impl FnOnce(&mut Shown)) {
    let mut all = SHOWN.lock();
    if let Some(shown) =
        all.iter_mut().flatten().find(|s| usize::from(s.bus) == bus && s.path == path)
    {
        change(shown);
    }
}

/// Says that `driver` took interface `interface` of the device at
/// `path`.
pub fn set_driver(bus: usize, path: Path, interface: u8, driver: &'static str) {
    change(bus, path, |shown| {
        let count = usize::from(shown.listed).min(MAX_INTERFACES);
        if let Some(i) =
            shown.interfaces[..count].iter_mut().find(|i| i.descriptor.number == interface)
        {
            i.driver = Some(driver);
        }
    });
}

/// Says that the hub at `path` has `ports` ports.
pub fn set_hub_ports(bus: usize, path: Path, ports: u8) {
    change(bus, path, |shown| shown.hub_ports = ports);
}

/// The device shown in place `at`, from 0, if there is one: each place
/// keeps its device while it is plugged in.
pub fn get(at: usize) -> Option<Shown> {
    SHOWN.lock().get(at).cloned().flatten()
}

/// How many places [`get`] has.
pub const PLACES: usize = MAX_SHOWN;

/// The device whose directory is `name`, `1-3.2`.
pub fn find(name: &str) -> Option<Shown> {
    SHOWN.lock().iter().flatten().find(|shown| shown.name().as_str() == name).cloned()
}

/// The text of a speed as Linux's `speed` file gives it, in Mb/s.
pub fn speed_text(speed: Speed) -> &'static str {
    match speed {
        Speed::Low => "1.5",
        Speed::Full => "12",
        Speed::High => "480",
        Speed::Super => "5000",
        Speed::SuperPlus => "10000",
    }
}
