// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! lsusb(8): the USB devices the kernel set up, from
//! `/kernel/bus/usb/devices` (radix-sysfs(5)), named from the USB ID
//! database in `/system/resources/hwdata/usb.ids` as usbutils' lsusb
//! names them (ADR 0230): a line for each device, or with `-t` the tree
//! of each bus's ports, the interfaces of the devices on them and their
//! drivers.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::args::{Arg, Args};
use crate::util::{Fail, fail, usage};

/// Where the kernel lists the devices.
const DEVICES: &str = "/kernel/bus/usb/devices";

/// The USB ID database, where hwdata puts it on Linux, under Dukovo's
/// name for /usr/share.
const IDS: &str = "/system/resources/hwdata/usb.ids";

/// The driver of every root hub's controller, as Linux names it: the
/// kernel drives xHCI controllers only.
const ROOT_DRIVER: &str = "xhci_hcd";

/// A controller's root hub.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RootHub {
    bus: u8,
    ports: u8,
    /// In Mb/s, as the kernel gives it: `5000`.
    speed: String,
}

/// An interface of a device's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Interface {
    number: u8,
    class: u8,
    /// The driver that took it: `usbhid`.
    driver: Option<String>,
}

/// A device the kernel set up.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Device {
    bus: u8,
    number: u8,
    /// The ports from its root port down: `[3, 2]` for `1-3.2`.
    ports: Vec<u8>,
    vendor: u16,
    product: u16,
    speed: String,
    /// A hub's ports.
    hub_ports: u8,
    /// Its own strings.
    manufacturer: Option<String>,
    name: Option<String>,
    /// In the order of their numbers.
    interfaces: Vec<Interface>,
}

/// What a file of a directory says, without its newline.
fn read(dir: &Path, file: &str) -> Option<String> {
    let text = fs::read_to_string(dir.join(file)).ok()?;
    Some(text.trim_end_matches('\n').to_string())
}

/// A file's number, hexadecimal.
fn read_hex(dir: &Path, file: &str) -> Option<u16> {
    u16::from_str_radix(read(dir, file)?.trim(), 16).ok()
}

/// A file's number, decimal.
fn read_decimal(dir: &Path, file: &str) -> Option<u8> {
    read(dir, file)?.trim().parse().ok()
}

/// `1-3.2`: the bus and the ports.
fn parse_name(name: &str) -> Option<(u8, Vec<u8>)> {
    let (bus, ports) = name.split_once('-')?;
    let bus = bus.parse().ok()?;
    let ports = ports.split('.').map(|port| port.parse().ok()).collect::<Option<Vec<u8>>>()?;
    Some((bus, ports))
}

/// The root hubs and devices under `dir`, each in the order of its bus
/// and number.
fn read_devices(dir: &Path) -> io::Result<(Vec<RootHub>, Vec<Device>)> {
    let mut hubs = Vec::new();
    let mut devices = Vec::new();
    let mut interfaces: HashMap<String, Vec<Interface>> = HashMap::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
        let path = entry.path();
        if let Some(bus) = name.strip_prefix("usb").and_then(|bus| bus.parse().ok()) {
            hubs.push(RootHub {
                bus,
                ports: read_decimal(&path, "maxchild").unwrap_or(0),
                speed: read(&path, "speed").unwrap_or_default(),
            });
        } else if let Some((device, _)) = name.split_once(':') {
            let (Some(number), Some(class)) =
                (read_hex(&path, "bInterfaceNumber"), read_hex(&path, "bInterfaceClass"))
            else {
                continue;
            };
            let driver = fs::read_link(path.join("driver"))
                .ok()
                .and_then(|link| link.file_name()?.to_str().map(str::to_string));
            interfaces.entry(device.to_string()).or_default().push(Interface {
                number: number as u8,
                class: class as u8,
                driver,
            });
        } else if let Some((bus, ports)) = parse_name(&name) {
            let (Some(number), Some(vendor), Some(product)) = (
                read_decimal(&path, "devnum"),
                read_hex(&path, "idVendor"),
                read_hex(&path, "idProduct"),
            ) else {
                continue;
            };
            devices.push((
                name,
                Device {
                    bus,
                    number,
                    ports,
                    vendor,
                    product,
                    speed: read(&path, "speed").unwrap_or_default(),
                    hub_ports: read_decimal(&path, "maxchild").unwrap_or(0),
                    manufacturer: read(&path, "manufacturer"),
                    name: read(&path, "product"),
                    interfaces: Vec::new(),
                },
            ));
        }
    }
    let mut devices: Vec<Device> = devices
        .into_iter()
        .map(|(name, mut device)| {
            device.interfaces = interfaces.remove(&name).unwrap_or_default();
            device.interfaces.sort_by_key(|i| i.number);
            device
        })
        .collect();
    hubs.sort_by_key(|hub| hub.bus);
    devices.sort_by_key(|d| (d.bus, d.number));
    Ok((hubs, devices))
}

/// The names the database gives the devices asked about, and the
/// classes.
#[derive(Default)]
struct Names {
    vendors: HashMap<u16, String>,
    products: HashMap<(u16, u16), String>,
    classes: HashMap<u8, String>,
}

/// The four hexadecimal digits at the start of `text`, and what follows
/// them.
fn id(text: &str) -> Option<(u16, &str)> {
    let digits = text.get(..4)?;
    Some((u16::from_str_radix(digits, 16).ok()?, text[4..].trim_start()))
}

/// Two hexadecimal digits at the start of `text`, and what follows.
fn class_id(text: &str) -> Option<(u8, &str)> {
    let digits = text.get(..2)?;
    Some((u8::from_str_radix(digits, 16).ok()?, text[2..].trim_start()))
}

impl Names {
    /// The names in database `text` of `devices`' vendors and products,
    /// and of every class: the database names some 25,000 products.
    fn read(text: &str, devices: &[Device]) -> Names {
        let vendors: HashSet<u16> = devices.iter().map(|d| d.vendor).collect();
        let products: HashSet<(u16, u16)> = devices.iter().map(|d| (d.vendor, d.product)).collect();
        let mut names = Names::default();
        // The vendor whose products follow, in the vendors' section.
        let mut vendor = None;
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') || line.starts_with("\t\t") {
                continue;
            }
            if let Some(rest) = line.strip_prefix('\t') {
                if let Some(v) = vendor
                    && let Some((p, name)) = id(rest)
                    && products.contains(&(v, p))
                {
                    names.products.insert((v, p), name.to_string());
                }
            } else if let Some(rest) = line.strip_prefix("C ") {
                vendor = None;
                if let Some((class, name)) = class_id(rest) {
                    names.classes.insert(class, name.to_string());
                }
            } else if let Some((v, name)) =
                id(line).filter(|_| line.as_bytes().get(4) == Some(&b' '))
            {
                vendor = Some(v);
                if vendors.contains(&v) {
                    names.vendors.insert(v, name.to_string());
                }
            } else {
                // Another section: `AT`, `HID`, `L`...
                vendor = None;
            }
        }
        names
    }
}

/// The lines lsusb prints for `devices`: `Bus 001 Device 002: ID
/// 046d:c52b Logitech, Inc. Unifying Receiver`, named from the database
/// and else from the device's own strings.
fn lines(devices: &[Device], names: &Names) -> Vec<String> {
    devices
        .iter()
        .map(|d| {
            let vendor = names.vendors.get(&d.vendor).or(d.manufacturer.as_ref());
            let product = names.products.get(&(d.vendor, d.product)).or(d.name.as_ref());
            format!(
                "Bus {:03} Device {:03}: ID {:04x}:{:04x} {} {}",
                d.bus,
                d.number,
                d.vendor,
                d.product,
                vendor.map_or("[unknown]", String::as_str),
                product.map_or("[unknown]", String::as_str)
            )
        })
        .collect()
}

/// The lines of `lsusb -t`: each root hub, then each interface of each
/// device beneath it, a level deeper for each hub.
fn tree(hubs: &[RootHub], devices: &[Device], names: &Names) -> Vec<String> {
    let mut out = Vec::new();
    for hub in hubs {
        out.push(format!(
            "/:  Bus {:03}.Port 001: Dev 001, Class=root_hub, Driver={ROOT_DRIVER}/{}p, {}M",
            hub.bus, hub.ports, hub.speed
        ));
        branch(&mut out, devices, names, hub.bus, &[]);
    }
    out
}

/// The devices on the ports of the hub at `ports` on bus `bus`, in the
/// order of their ports, each followed by its own.
fn branch(out: &mut Vec<String>, devices: &[Device], names: &Names, bus: u8, ports: &[u8]) {
    let mut below: Vec<&Device> = devices
        .iter()
        .filter(|d| d.bus == bus && d.ports.len() == ports.len() + 1 && d.ports.starts_with(ports))
        .collect();
    below.sort_by_key(|d| d.ports.last().copied());
    let indent = 4 * (ports.len() + 1);
    for d in below {
        let port = d.ports.last().copied().unwrap_or(0);
        for i in &d.interfaces {
            let class = names.classes.get(&i.class).map_or("[unknown]", String::as_str);
            let mut driver = i.driver.clone().unwrap_or_else(|| "[none]".to_string());
            if i.class == 9 {
                driver += &format!("/{}p", d.hub_ports);
            }
            out.push(format!(
                "{:indent$}|__ Port {port:03}: Dev {:03}, If {}, Class={class}, Driver={driver}, \
                 {}M",
                "", d.number, i.number, d.speed
            ));
        }
        branch(out, devices, names, bus, &d.ports);
    }
}

/// What `-s` and `-d` let through; `None` matches anything.
#[derive(Default)]
struct Filter {
    bus: Option<u8>,
    number: Option<u8>,
    vendor: Option<u16>,
    product: Option<u16>,
}

impl Filter {
    /// `-s [[bus]:][devnum]`, in decimal.
    fn place(&mut self, text: &str) -> Result<(), Fail> {
        let (bus, number) = text.split_once(':').unwrap_or(("", text));
        let field = |part: &str| -> Result<Option<u8>, Fail> {
            if part.is_empty() {
                return Ok(None);
            }
            part.parse().map(Some).map_err(|_| usage(format!("-s: invalid bus or device '{text}'")))
        };
        self.bus = field(bus)?;
        self.number = field(number)?;
        Ok(())
    }

    /// `-d [vendor]:[product]`, in hexadecimal.
    fn ids(&mut self, text: &str) -> Result<(), Fail> {
        let bad = || usage(format!("-d: invalid device ID '{text}'"));
        let (vendor, product) = text.split_once(':').ok_or_else(bad)?;
        let field = |part: &str| -> Result<Option<u16>, Fail> {
            if part.is_empty() {
                return Ok(None);
            }
            u16::from_str_radix(part, 16).map(Some).map_err(|_| bad())
        };
        self.vendor = field(vendor)?;
        self.product = field(product)?;
        Ok(())
    }

    fn matches(&self, d: &Device) -> bool {
        self.bus.is_none_or(|b| b == d.bus)
            && self.number.is_none_or(|n| n == d.number)
            && self.vendor.is_none_or(|v| v == d.vendor)
            && self.product.is_none_or(|p| p == d.product)
    }
}

/// Prints what `dir` shows, named from database `ids`, as lsusb does:
/// the tree for `tree`, else the devices `filter` lets through. 1 when
/// that is no device, as usbutils' lsusb.
fn run(
    dir: &Path,
    ids: &Path,
    tree_view: bool,
    filter: &Filter,
    out: &mut dyn Write,
) -> Result<i32, Fail> {
    let (hubs, mut devices) = read_devices(dir).map_err(|error| {
        fail(format!("{}: {}", dir.display(), crate::util::strip_os_error(&error)))
    })?;
    if !tree_view {
        devices.retain(|d| filter.matches(d));
    }
    // As usbutils, without its database: the devices' own strings. Not
    // all of it is UTF-8: a key among its HID usages is in Latin-1.
    let names = fs::read(ids)
        .map_or_default(|bytes| Names::read(&String::from_utf8_lossy(&bytes), &devices));
    if tree_view {
        for line in tree(&hubs, &devices, &names) {
            writeln!(out, "{line}")?;
        }
        return Ok(0);
    }
    for line in lines(&devices, &names) {
        writeln!(out, "{line}")?;
    }
    Ok(if devices.is_empty() { 1 } else { 0 })
}

/// lsusb(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut tree_view = false;
    let mut filter = Filter::default();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('t') => tree_view = true,
            Arg::Long(ref name) if name == "tree" => tree_view = true,
            Arg::Short('s') => filter.place(&args.text()?)?,
            Arg::Short('d') => filter.ids(&args.text()?)?,
            Arg::Short('h') => {
                return Err(usage(
                    "Usage: lsusb [-t] [-s [[bus]:][devnum]] [-d [vendor]:[product]]\n  -t, --tree  \
                     the buses, ports, interfaces and drivers, as a tree\n  -s          only the \
                     devices on this bus, or of this number\n  -d          only the devices of \
                     this vendor and product"
                        .to_string(),
                ));
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let mut out = io::stdout().lock();
    run(Path::new(DEVICES), Path::new(IDS), tree_view, &filter, &mut out)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A piece of usb.ids, as the database writes it.
    const USB_IDS: &str = "\
#\tList of USB ID's
# Version: 2026.06.26
046d  Logitech, Inc.
\tc52b  Unifying Receiver
\tc534  Nano Receiver
0bda  Realtek Semiconductor Corp.
\t5411  RTS5411 Hub
\t\t00  Not a product
32ac  Framework Computer Inc
\t0012  Laptop Expansion Card
0781  SanDisk Corp.

# List of known device classes, subclasses and protocols
C 00  (Defined at Interface level)
C 03  Human Interface Device
\t01  Boot Interface Subclass
\t\t02  Mouse
C 08  Mass Storage
C 09  Hub
\t00  Unused
AT 0001  Not a vendor
\t0002  Not a product
HID 00  Undefined
";

    /// A machine's devices, as the kernel shows them: a hub on root
    /// port 1 with a receiver and a card on it, and a stick on port 3.
    fn sysfs() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lsusb-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let write = |name: &str, files: &[(&str, &str)]| {
            let path = dir.join(name);
            fs::create_dir_all(&path).unwrap();
            for (file, value) in files {
                fs::write(path.join(file), format!("{value}\n")).unwrap();
            }
        };
        write("usb1", &[("busnum", "1"), ("devnum", "1"), ("speed", "5000"), ("maxchild", "4")]);
        let device = |name: &str, number: &str, vendor: &str, product: &str, speed: &str| {
            write(
                name,
                &[
                    ("devnum", number),
                    ("idVendor", vendor),
                    ("idProduct", product),
                    ("speed", speed),
                ],
            );
        };
        device("1-1", "2", "0bda", "5411", "480");
        write("1-1", &[("maxchild", "4")]);
        device("1-1.2", "3", "046d", "c52b", "12");
        device("1-1.4", "5", "32ac", "9999", "480");
        write("1-1.4", &[("manufacturer", "Framework"), ("product", "HDMI Expansion Card")]);
        device("1-3", "4", "0781", "5581", "5000");
        let interface = |name: &str, number: &str, class: &str, driver: Option<&str>| {
            write(name, &[("bInterfaceNumber", number), ("bInterfaceClass", class)]);
            if let Some(driver) = driver {
                let target = format!("../../drivers/{driver}");
                std::os::unix::fs::symlink(target, dir.join(name).join("driver")).unwrap();
            }
        };
        interface("1-1:1.0", "00", "09", Some("hub"));
        interface("1-1.2:1.1", "01", "03", Some("usbhid"));
        interface("1-1.2:1.0", "00", "03", Some("usbhid"));
        interface("1-1.4:1.0", "00", "11", None);
        interface("1-3:1.0", "00", "08", Some("uas"));
        // Not a device.
        fs::create_dir_all(dir.join("stray")).unwrap();
        dir
    }

    #[test]
    fn names_and_trees_as_usbutils() {
        let dir = sysfs();
        let ids = dir.with_extension("ids");
        // With the line of usb.ids that is not UTF-8.
        fs::write(&ids, [USB_IDS.as_bytes(), b"HUT 07  Keyboard\n\t034  \xb4 and \"\n"].concat())
            .unwrap();
        let run_with = |tree_view: bool, filter: &Filter| {
            let mut out = Vec::new();
            let status = run(&dir, &ids, tree_view, filter, &mut out).unwrap();
            (status, String::from_utf8(out).unwrap())
        };
        let (status, plain) = run_with(false, &Filter::default());
        assert_eq!(status, 0);
        assert_eq!(
            plain,
            "Bus 001 Device 002: ID 0bda:5411 Realtek Semiconductor Corp. RTS5411 Hub\n\
             Bus 001 Device 003: ID 046d:c52b Logitech, Inc. Unifying Receiver\n\
             Bus 001 Device 004: ID 0781:5581 SanDisk Corp. [unknown]\n\
             Bus 001 Device 005: ID 32ac:9999 Framework Computer Inc HDMI Expansion Card\n"
        );
        let (status, tree) = run_with(true, &Filter::default());
        assert_eq!(status, 0);
        assert_eq!(
            tree,
            "/:  Bus 001.Port 001: Dev 001, Class=root_hub, Driver=xhci_hcd/4p, 5000M\n    \
             |__ Port 001: Dev 002, If 0, Class=Hub, Driver=hub/4p, 480M\n        \
             |__ Port 002: Dev 003, If 0, Class=Human Interface Device, Driver=usbhid, 12M\n        \
             |__ Port 002: Dev 003, If 1, Class=Human Interface Device, Driver=usbhid, 12M\n        \
             |__ Port 004: Dev 005, If 0, Class=[unknown], Driver=[none], 480M\n    \
             |__ Port 003: Dev 004, If 0, Class=Mass Storage, Driver=uas, 5000M\n"
        );
        // Without the database: the devices' own strings.
        let mut out = Vec::new();
        run(&dir, &dir.join("none"), false, &Filter::default(), &mut out).unwrap();
        let plain = String::from_utf8(out).unwrap();
        assert!(plain.contains("ID 32ac:9999 Framework HDMI Expansion Card\n"), "{plain}");
        assert!(plain.contains("ID 046d:c52b [unknown] [unknown]\n"), "{plain}");
        let picked = |place: Option<&str>, ids: Option<&str>| {
            let mut filter = Filter::default();
            if let Some(place) = place {
                filter.place(place).unwrap();
            }
            if let Some(ids) = ids {
                filter.ids(ids).unwrap();
            }
            run_with(false, &filter)
        };
        assert_eq!(picked(Some("1:3"), None).1.lines().count(), 1);
        assert!(picked(Some("4"), None).1.contains("0781:5581"));
        assert_eq!(picked(Some("1:"), None).1.lines().count(), 4);
        assert_eq!(picked(Some("2:"), None), (1, String::new()));
        assert_eq!(picked(None, Some("046d:")).1.lines().count(), 1);
        assert!(picked(None, Some(":5411")).1.contains("Device 002"));
        assert_eq!(picked(None, Some("046d:c534")).0, 1);
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_file(&ids).unwrap();
    }

    #[test]
    fn reads_the_database_and_the_options() {
        let device = |vendor, product| Device {
            bus: 1,
            number: 2,
            ports: vec![1],
            vendor,
            product,
            speed: "12".into(),
            hub_ports: 0,
            manufacturer: None,
            name: None,
            interfaces: Vec::new(),
        };
        let names = Names::read(USB_IDS, &[device(0x046d, 0xc52b), device(0x0001, 0x0002)]);
        // Only what was asked about is kept, and only from the vendors.
        assert_eq!(names.vendors.len(), 1);
        assert_eq!(names.products.len(), 1);
        assert!(!names.products.contains_key(&(0x046d, 0xc534)));
        assert_eq!(names.classes[&0x09], "Hub");
        assert_eq!(names.classes.len(), 4);
        assert_eq!(parse_name("1-3.2"), Some((1, vec![3, 2])));
        assert_eq!(parse_name("12-1"), Some((12, vec![1])));
        assert_eq!(parse_name("1-"), None);
        assert_eq!(parse_name("usb1"), None);
        assert!(Filter::default().place("x:1").is_err());
        assert!(Filter::default().place("1:300").is_err());
        assert!(Filter::default().ids("046d").is_err());
        assert!(Filter::default().ids("zz:1").is_err());
    }
}
