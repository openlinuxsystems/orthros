// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/kernel/bus/usb` (ADR 0230): the USB devices, as Linux's usbcore
//! shows them in `/sys/bus/usb`, which lsusb(8) reads.
//!
//! `devices` has a directory for each controller's root hub (`usb1`),
//! each device (`1-3.2`, its bus and ports) and each interface of its
//! configuration (`1-3.2:1.0`, the configuration's value and the
//! interface's number), with Linux's files: the device descriptor's
//! fields in hexadecimal (`idVendor`, `bDeviceClass`...), its strings
//! (`manufacturer`, `product`, `serial`), its `busnum`, `devnum`,
//! `devpath`, `speed` (in Mb/s) and, for a hub, `maxchild`; an
//! interface's descriptor, and its `driver`, a link to `drivers/hub`,
//! `drivers/usbhid`, `drivers/usb-storage` or `drivers/uas`.
//!
//! A file is read from what the controller's thread keeps of the device
//! ([`crate::usb::shown`]), so a device unplugged fails to read.

use core::fmt::{self, Write};

use radix_klog::TextBuf;
use radix_vfs::synthetic::{FixedDir, Link, ListedDir, Listing};
use radix_vfs::{Error, Kind, NodeRef, Result, Text, mem, next_id};

use super::value;
use crate::usb::shown::{self, Shown};

/// The drivers an interface may have, as Linux names them.
const DRIVERS: [&str; 4] = ["hub", "usbhid", "usb-storage", "uas"];

/// Adds `usb` to `bus`.
pub(super) fn add(bus: &FixedDir) -> Result<()> {
    let usb = bus.add_dir("usb", next_id())?;
    let devices: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(Devices)?;
    usb.add("devices", ListedDir::make(next_id(), devices)?)?;
    let drivers = usb.add_dir("drivers", next_id())?;
    for driver in DRIVERS {
        drivers.add_dir(driver, next_id())?;
    }
    Ok(())
}

/// `devices`.
struct Devices;

/// The most buses.
const BUSES: usize = 8;

impl Listing for Devices {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        for bus in 1..=BUSES {
            if shown::root_hub(bus).is_some() {
                let mut name = TextBuf::<8>::new();
                let _ = write!(name, "usb{bus}");
                if !f(name.as_str(), Kind::Directory) {
                    return;
                }
            }
        }
        for at in 0..shown::PLACES {
            let Some(device) = shown::get(at) else { continue };
            let name = device.name();
            if !f(name.as_str(), Kind::Directory) {
                return;
            }
            for interface in device.interfaces() {
                let name = interface_name(&device, interface.descriptor.number);
                if !f(name.as_str(), Kind::Directory) {
                    return;
                }
            }
        }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        if let Some(bus) = name.strip_prefix("usb") {
            return root_hub(bus);
        }
        match name.split_once(':') {
            Some((device, interface)) => interface_dir(device, interface),
            None => device_dir(name),
        }
    }
}

/// An interface's directory's name: `1-3.2:1.0`.
fn interface_name(device: &Shown, number: u8) -> TextBuf<40> {
    let mut name = TextBuf::new();
    let _ = write!(name, "{}:{}.{number}", device.name(), device.configuration);
    name
}

/// `usb<bus>`.
fn root_hub(bus: &str) -> Result<NodeRef> {
    // "1" and not "01": one name for each.
    if bus.starts_with('0') {
        return Err(Error::NotFound);
    }
    let bus: usize = bus.parse().map_err(|_| Error::NotFound)?;
    let hub = shown::root_hub(bus).ok_or(Error::NotFound)?;
    let dir = FixedDir::new(next_id())?;
    let (speed, version, product) = (hub.speed, hub.version, hub.product);
    let ports = hub.ports;
    let address = hub.address;
    dir.add("busnum", value(move |text| writeln!(text, "{bus}"))?)?;
    dir.add("devnum", value(|text| writeln!(text, "1"))?)?;
    dir.add("devpath", value(|text| writeln!(text, "0"))?)?;
    dir.add("speed", value(move |text| writeln!(text, "{speed}"))?)?;
    dir.add("version", value(move |text| writeln!(text, "{version}"))?)?;
    dir.add("maxchild", value(move |text| writeln!(text, "{ports}"))?)?;
    dir.add("bDeviceClass", value(|text| writeln!(text, "09"))?)?;
    dir.add("product", value(move |text| writeln!(text, "{product}"))?)?;
    dir.add("serial", value(move |text| writeln!(text, "{address}"))?)?;
    Ok(dir)
}

/// What one of a device's files says.
type Field = fn(&Shown, &mut Text) -> fmt::Result;

/// A device's files, as Linux's.
const DEVICE_FILES: [(&str, Field); 18] = [
    ("busnum", |d, t| writeln!(t, "{}", d.bus)),
    ("devnum", |d, t| writeln!(t, "{}", d.number)),
    ("devpath", |d, t| {
        d.write_devpath(t)?;
        writeln!(t)
    }),
    ("idVendor", |d, t| writeln!(t, "{:04x}", d.device.vendor)),
    ("idProduct", |d, t| writeln!(t, "{:04x}", d.device.product)),
    ("bcdDevice", |d, t| writeln!(t, "{:04x}", d.device.release)),
    ("version", |d, t| writeln!(t, "{:2x}.{:02x}", d.device.usb >> 8, d.device.usb & 0xff)),
    ("bDeviceClass", |d, t| writeln!(t, "{:02x}", d.device.class)),
    ("bDeviceSubClass", |d, t| writeln!(t, "{:02x}", d.device.subclass)),
    ("bDeviceProtocol", |d, t| writeln!(t, "{:02x}", d.device.protocol)),
    ("bMaxPacketSize0", |d, t| writeln!(t, "{}", d.device.max_packet0_raw)),
    ("bNumConfigurations", |d, t| writeln!(t, "{}", d.device.configurations)),
    ("bConfigurationValue", |d, t| writeln!(t, "{}", d.configuration)),
    ("bNumInterfaces", |d, t| writeln!(t, "{:2}", d.interface_count)),
    ("bmAttributes", |d, t| writeln!(t, "{:2x}", d.attributes)),
    ("bMaxPower", |d, t| writeln!(t, "{}mA", d.max_power_ma)),
    ("speed", |d, t| writeln!(t, "{}", shown::speed_text(d.speed))),
    ("maxchild", |d, t| writeln!(t, "{}", d.hub_ports)),
];

/// A file of the device `name` that says what `field` does.
fn device_file(name: TextBuf<32>, field: Field) -> Result<NodeRef> {
    value(move |text| {
        let device = shown::find(name.as_str()).ok_or(fmt::Error)?;
        field(&device, text)
    })
}

/// `<bus>-<ports>`.
fn device_dir(name: &str) -> Result<NodeRef> {
    let device = shown::find(name).ok_or(Error::NotFound)?;
    let name = device.name();
    let dir = FixedDir::new(next_id())?;
    for (file, field) in DEVICE_FILES {
        dir.add(file, device_file(name.clone(), field)?)?;
    }
    let strings: [(&str, &TextBuf<64>, Field); 3] = [
        ("manufacturer", &device.manufacturer, |d, t| writeln!(t, "{}", d.manufacturer.as_str())),
        ("product", &device.product, |d, t| writeln!(t, "{}", d.product.as_str())),
        ("serial", &device.serial, |d, t| writeln!(t, "{}", d.serial.as_str())),
    ];
    for (file, string, field) in strings {
        if !string.is_empty() {
            dir.add(file, device_file(name.clone(), field)?)?;
        }
    }
    Ok(dir)
}

/// What one of an interface's files says, of its descriptor.
type InterfaceField = fn(&radix_usb::InterfaceDescriptor, &mut Text) -> fmt::Result;

/// An interface's files, as Linux's.
const INTERFACE_FILES: [(&str, InterfaceField); 6] = [
    ("bInterfaceNumber", |i, t| writeln!(t, "{:02x}", i.number)),
    ("bAlternateSetting", |i, t| writeln!(t, "{:2}", i.alternate)),
    ("bNumEndpoints", |i, t| writeln!(t, "{:02x}", i.endpoints)),
    ("bInterfaceClass", |i, t| writeln!(t, "{:02x}", i.class)),
    ("bInterfaceSubClass", |i, t| writeln!(t, "{:02x}", i.subclass)),
    ("bInterfaceProtocol", |i, t| writeln!(t, "{:02x}", i.protocol)),
];

/// `<bus>-<ports>:<configuration>.<interface>`.
fn interface_dir(device: &str, interface: &str) -> Result<NodeRef> {
    let shown = shown::find(device).ok_or(Error::NotFound)?;
    let (configuration, number) = interface.split_once('.').ok_or(Error::NotFound)?;
    let number: u8 = number.parse().map_err(|_| Error::NotFound)?;
    let found = shown
        .interfaces()
        .iter()
        .find(|i| i.descriptor.number == number)
        .filter(|_| configuration.parse() == Ok(shown.configuration))
        .copied()
        .ok_or(Error::NotFound)?;
    // Its name as the listing gives it, and no other spelling.
    let mut asked = TextBuf::<40>::new();
    let _ = write!(asked, "{device}:{interface}");
    if interface_name(&shown, number).as_str() != asked.as_str() {
        return Err(Error::NotFound);
    }
    let dir = FixedDir::new(next_id())?;
    for (file, field) in INTERFACE_FILES {
        let descriptor = found.descriptor;
        dir.add(file, value(move |text| field(&descriptor, text))?)?;
    }
    if let Some(driver) = found.driver {
        let mut target = TextBuf::<32>::new();
        let _ = write!(target, "../../drivers/{driver}");
        dir.add("driver", Link::make(next_id(), target.as_str())?)?;
    }
    Ok(dir)
}
