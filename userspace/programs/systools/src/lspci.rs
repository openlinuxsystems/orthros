// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! lspci(8): the PCI functions the kernel found, from
//! `/kernel/bus/pci/devices` (radix-sysfs(5)), named from the PCI ID
//! database in `/system/resources/hwdata/pci.ids` as pciutils' lspci
//! names them (ADR 0144), with its common options.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::args::{Arg, Args};
use crate::util::{Fail, fail, usage};

/// Where the kernel lists the functions.
const DEVICES: &str = "/kernel/bus/pci/devices";

/// The PCI ID database, where hwdata puts it on Linux, under Dukovo's
/// name for /usr/share.
pub const IDS: &str = "/system/resources/hwdata/pci.ids";

/// A PCI function, as the kernel shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Function {
    domain: u16,
    bus: u8,
    slot: u8,
    func: u8,
    vendor: u16,
    device: u16,
    /// Class, subclass and programming interface.
    class: u32,
    revision: u8,
    subsystem: Option<(u16, u16)>,
}

impl Function {
    fn base_class(&self) -> u8 {
        (self.class >> 16) as u8
    }

    fn subclass(&self) -> u8 {
        (self.class >> 8) as u8
    }

    fn interface(&self) -> u8 {
        self.class as u8
    }
}

/// A number from a file of the kernel's: hexadecimal, with `0x`.
fn hex(text: &str) -> Option<u32> {
    let text = text.trim();
    u32::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16).ok()
}

/// `domain:bus:slot.func`, as the directories are named.
fn parse_address(name: &str) -> Option<(u16, u8, u8, u8)> {
    let (rest, func) = name.rsplit_once('.')?;
    let mut parts = rest.split(':');
    let domain = u16::from_str_radix(parts.next()?, 16).ok()?;
    let bus = u8::from_str_radix(parts.next()?, 16).ok()?;
    let slot = u8::from_str_radix(parts.next()?, 16).ok()?;
    let func = func.parse().ok()?;
    (parts.next().is_none() && slot < 32 && func < 8).then_some((domain, bus, slot, func))
}

/// The functions under `dir`, in the order of their addresses.
fn functions(dir: &Path) -> io::Result<Vec<Function>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some((domain, bus, slot, func)) = name.to_str().and_then(parse_address) else {
            continue;
        };
        let path = entry.path();
        let read = |file: &str| fs::read_to_string(path.join(file)).ok().and_then(|t| hex(&t));
        let (Some(vendor), Some(device), Some(class)) =
            (read("vendor"), read("device"), read("class"))
        else {
            continue;
        };
        let subsystem = match (read("subsystem_vendor"), read("subsystem_device")) {
            (Some(vendor), Some(device)) if vendor != 0 && vendor != 0xffff => {
                Some((vendor as u16, device as u16))
            }
            _ => None,
        };
        found.push(Function {
            domain,
            bus,
            slot,
            func,
            vendor: vendor as u16,
            device: device as u16,
            class,
            revision: read("revision").unwrap_or(0) as u8,
            subsystem,
        });
    }
    found.sort_by_key(|f| (f.domain, f.bus, f.slot, f.func));
    Ok(found)
}

/// The names the database gives the functions asked about.
#[derive(Default)]
struct Names {
    vendors: HashMap<u16, String>,
    devices: HashMap<(u16, u16), String>,
    subsystems: HashMap<(u16, u16, u16, u16), String>,
    classes: HashMap<u8, String>,
    subclasses: HashMap<(u8, u8), String>,
    interfaces: HashMap<(u8, u8, u8), String>,
}

/// The four hexadecimal digits at the start of `text`, and what follows
/// them.
fn id(text: &str) -> Option<(u16, &str)> {
    let digits = text.get(..4)?;
    let rest = &text[4..];
    Some((u16::from_str_radix(digits, 16).ok()?, rest.trim_start()))
}

/// Two hexadecimal digits at the start of `text`, and what follows.
fn class_id(text: &str) -> Option<(u8, &str)> {
    let digits = text.get(..2)?;
    Some((u8::from_str_radix(digits, 16).ok()?, text[2..].trim_start()))
}

impl Names {
    /// The names in database `text` of what `functions` are. Only those
    /// are kept: the database names some 40,000 devices.
    fn read(text: &str, functions: &[Function]) -> Names {
        let mut vendors = HashSet::new();
        for f in functions {
            vendors.insert(f.vendor);
            if let Some((vendor, _)) = f.subsystem {
                vendors.insert(vendor);
            }
        }
        let devices: HashSet<(u16, u16)> = functions.iter().map(|f| (f.vendor, f.device)).collect();
        let mut names = Names::default();
        let (mut vendor, mut device, mut class, mut subclass) = (None, None, None, None);
        // Whether the lines are in the class section, `C`.
        let mut classes = false;
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("\t\t") {
                if classes {
                    if let (Some(c), Some(s), Some((p, name))) = (class, subclass, class_id(rest)) {
                        names.interfaces.insert((c, s, p), name.to_string());
                    }
                } else if let (Some(v), Some(d)) = (vendor, device)
                    && devices.contains(&(v, d))
                    && let Some((sv, rest)) = id(rest)
                    && let Some((sd, name)) = id(rest)
                {
                    names.subsystems.insert((v, d, sv, sd), name.to_string());
                }
            } else if let Some(rest) = line.strip_prefix('\t') {
                if classes {
                    subclass = class_id(rest).map(|(s, name)| {
                        if let Some(c) = class {
                            names.subclasses.insert((c, s), name.to_string());
                        }
                        s
                    });
                } else {
                    device = id(rest).map(|(d, name)| {
                        if let Some(v) = vendor
                            && devices.contains(&(v, d))
                        {
                            names.devices.insert((v, d), name.to_string());
                        }
                        d
                    });
                }
            } else if let Some(rest) = line.strip_prefix("C ") {
                classes = true;
                class = class_id(rest).map(|(c, name)| {
                    names.classes.insert(c, name.to_string());
                    c
                });
                subclass = None;
            } else if let Some((v, name)) =
                id(line).filter(|_| line.as_bytes().get(4) == Some(&b' '))
            {
                classes = false;
                vendor = Some(v);
                device = None;
                if vendors.contains(&v) {
                    names.vendors.insert(v, name.to_string());
                }
            } else {
                // Another section (usb.ids-style), which pci.ids has none of.
                classes = false;
                vendor = None;
                device = None;
            }
        }
        names
    }

    /// The class of `f` without numbers: its subclass's name, else its
    /// class's.
    fn class(&self, f: &Function) -> Option<&str> {
        let (c, s) = (f.base_class(), f.subclass());
        self.subclasses.get(&(c, s)).or_else(|| self.classes.get(&c)).map(String::as_str)
    }

    /// The name of the subsystem `sv`:`sd` of `f`: the database's, or
    /// the device's own when the subsystem is the device itself.
    fn subsystem(&self, f: &Function, (sv, sd): (u16, u16)) -> Option<&str> {
        let own =
            || self.devices.get(&(f.vendor, f.device)).filter(|_| (sv, sd) == (f.vendor, f.device));
        self.subsystems.get(&(f.vendor, f.device, sv, sd)).or_else(own).map(String::as_str)
    }

    fn interface(&self, f: &Function) -> Option<&str> {
        let key = (f.base_class(), f.subclass(), f.interface());
        self.interfaces.get(&key).map(String::as_str)
    }
}

/// How numbers and names mix (`-n`, `-nn`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ids {
    Names,
    Numbers,
    Both,
}

/// What `-s` and `-d` let through; `None` matches anything.
#[derive(Default)]
struct Filter {
    domain: Option<u16>,
    bus: Option<u8>,
    slot: Option<u8>,
    func: Option<u8>,
    vendor: Option<u16>,
    device: Option<u16>,
    class: Option<u16>,
}

/// A field of a filter: empty or `*` for any, else hexadecimal.
fn field<T: TryFrom<u32>>(text: &str, max: u32, what: &str) -> Result<Option<T>, Fail> {
    if text.is_empty() || text == "*" {
        return Ok(None);
    }
    let bad = || usage(format!("-{what}: invalid number '{text}'"));
    let value = u32::from_str_radix(text, 16).map_err(|_| bad())?;
    if value > max {
        return Err(bad());
    }
    Ok(Some(T::try_from(value).map_err(|_| bad())?))
}

impl Filter {
    /// `-s [[[[domain]:]bus]:][slot][.[func]]`.
    fn slot(&mut self, text: &str) -> Result<(), Fail> {
        let (rest, func) = match text.rsplit_once('.') {
            Some((rest, func)) => (rest, Some(func)),
            None => (text, None),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() > 3 {
            return Err(usage(format!("-s: invalid slot '{text}'")));
        }
        let mut parts = parts.into_iter().rev();
        self.slot = field(parts.next().unwrap_or(""), 31, "s")?;
        self.bus = field(parts.next().unwrap_or(""), 0xff, "s")?;
        self.domain = field(parts.next().unwrap_or(""), 0xffff, "s")?;
        self.func = field(func.unwrap_or(""), 7, "s")?;
        Ok(())
    }

    /// `-d [vendor]:[device][:class]`.
    fn device(&mut self, text: &str) -> Result<(), Fail> {
        let parts: Vec<&str> = text.split(':').collect();
        if !(2..=3).contains(&parts.len()) {
            return Err(usage(format!("-d: invalid ID '{text}'")));
        }
        self.vendor = field(parts[0], 0xffff, "d")?;
        self.device = field(parts[1], 0xffff, "d")?;
        self.class = field(parts.get(2).copied().unwrap_or(""), 0xffff, "d")?;
        Ok(())
    }

    fn matches(&self, f: &Function) -> bool {
        self.domain.is_none_or(|d| d == f.domain)
            && self.bus.is_none_or(|b| b == f.bus)
            && self.slot.is_none_or(|s| s == f.slot)
            && self.func.is_none_or(|n| n == f.func)
            && self.vendor.is_none_or(|v| v == f.vendor)
            && self.device.is_none_or(|d| d == f.device)
            && self.class.is_none_or(|c| u32::from(c) == f.class >> 8)
    }
}

/// How lspci was asked to show them.
struct Show {
    numbers: Ids,
    verbose: bool,
    machine: bool,
    domains: bool,
}

/// `name [number]`, `number` or `name`, as `numbers` asks; without a
/// name, `fallback` and the number.
fn mixed(name: Option<&str>, number: &str, fallback: &str, numbers: Ids) -> String {
    match (numbers, name) {
        (Ids::Numbers, _) => number.to_string(),
        (Ids::Both, Some(name)) => format!("{name} [{number}]"),
        (Ids::Names, Some(name)) => name.to_string(),
        (_, None) => format!("{fallback} {number}"),
    }
}

/// The vendor and device of a function, or of its subsystem, as
/// pciutils names them: `vendor device`, `vendor Device dddd` when the
/// database knows only the vendor, else `Device vvvv:dddd`.
fn vendor_device(vendor: Option<&str>, device: Option<&str>, ids: (u16, u16), n: Ids) -> String {
    let number = format!("{:04x}:{:04x}", ids.0, ids.1);
    match (n, vendor, device) {
        (Ids::Numbers, ..) => number,
        (_, Some(v), Some(d)) if n == Ids::Both => format!("{v} {d} [{number}]"),
        (_, Some(v), Some(d)) => format!("{v} {d}"),
        (_, Some(v), None) if n == Ids::Both => format!("{v} Device [{number}]"),
        (_, Some(v), None) => format!("{v} Device {:04x}", ids.1),
        (Ids::Both, None, _) => format!("Device [{number}]"),
        (_, None, _) => format!("Device {number}"),
    }
}

/// The lines lspci prints for `functions`.
fn lines(functions: &[Function], names: &Names, show: &Show) -> Vec<String> {
    let domains = show.domains || functions.iter().any(|f| f.domain != 0);
    let mut out = Vec::new();
    for f in functions {
        let slot = if domains {
            format!("{:04x}:{:02x}:{:02x}.{}", f.domain, f.bus, f.slot, f.func)
        } else {
            format!("{:02x}:{:02x}.{}", f.bus, f.slot, f.func)
        };
        let class_number = format!("{:04x}", f.class >> 8);
        let class = mixed(names.class(f), &class_number, "Class", show.numbers);
        let vendor = names.vendors.get(&f.vendor).map(String::as_str);
        let device = names.devices.get(&(f.vendor, f.device)).map(String::as_str);
        if show.machine {
            let quoted = |text: &str| format!("\"{text}\"");
            let (vendor, device) = match show.numbers {
                Ids::Numbers => (format!("{:04x}", f.vendor), format!("{:04x}", f.device)),
                _ => (
                    vendor.map_or_else(|| format!("Vendor {:04x}", f.vendor), str::to_string),
                    device.map_or_else(|| format!("Device {:04x}", f.device), str::to_string),
                ),
            };
            let mut line =
                format!("{slot} {} {} {}", quoted(&class), quoted(&vendor), quoted(&device));
            if f.revision != 0 {
                line += &format!(" -r{:02x}", f.revision);
            }
            if f.interface() != 0 {
                line += &format!(" -p{:02x}", f.interface());
            }
            match f.subsystem {
                Some((sv, sd)) => {
                    let (sv_name, sd_name) = match show.numbers {
                        Ids::Numbers => (format!("{sv:04x}"), format!("{sd:04x}")),
                        _ => (
                            names
                                .vendors
                                .get(&sv)
                                .cloned()
                                .unwrap_or_else(|| format!("Vendor {sv:04x}")),
                            names
                                .subsystem(f, (sv, sd))
                                .map_or_else(|| format!("Device {sd:04x}"), str::to_string),
                        ),
                    };
                    line += &format!(" {} {}", quoted(&sv_name), quoted(&sd_name));
                }
                None => line += " \"\" \"\"",
            }
            out.push(line);
            continue;
        }
        let mut line = format!(
            "{slot} {class}: {}",
            vendor_device(vendor, device, (f.vendor, f.device), show.numbers)
        );
        if f.revision != 0 {
            line += &format!(" (rev {:02x})", f.revision);
        }
        if show.verbose {
            let interface = names.interface(f).filter(|_| show.numbers != Ids::Numbers);
            if f.interface() != 0 || interface.is_some() {
                line += &format!(" (prog-if {:02x}", f.interface());
                if let Some(name) = interface {
                    line += &format!(" [{name}]");
                }
                line.push(')');
            }
        }
        out.push(line);
        if show.verbose {
            if let Some((sv, sd)) = f.subsystem {
                let vendor = names.vendors.get(&sv).map(String::as_str);
                let name = names.subsystem(f, (sv, sd));
                out.push(format!(
                    "\tSubsystem: {}",
                    vendor_device(vendor, name, (sv, sd), show.numbers)
                ));
            }
            out.push(String::new());
        }
    }
    out
}

/// lspci(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut show = Show { numbers: Ids::Names, verbose: false, machine: false, domains: false };
    let mut filter = Filter::default();
    let mut ids = IDS.to_string();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('n') => {
                show.numbers = if show.numbers == Ids::Names { Ids::Numbers } else { Ids::Both };
            }
            Arg::Short('v') => show.verbose = true,
            Arg::Short('m') => show.machine = true,
            Arg::Short('D') => show.domains = true,
            Arg::Short('s') => filter.slot(&args.text()?)?,
            Arg::Short('d') => filter.device(&args.text()?)?,
            Arg::Short('i') => ids = args.text()?,
            // Every function shows its numbers already, and a tree is
            // more than this shows.
            Arg::Short('k') => {}
            other => return Err(args.unknown(&other)),
        }
    }
    let mut found = functions(Path::new(DEVICES))
        .map_err(|error| fail(format!("{DEVICES}: {}", crate::util::strip_os_error(&error))))?;
    found.retain(|f| filter.matches(f));
    let names = match (show.numbers, fs::read_to_string(&ids)) {
        (Ids::Numbers, _) => Names::default(),
        (_, Ok(text)) => Names::read(&text, &found),
        // As pciutils, without its database: numbers.
        (_, Err(_)) => Names::default(),
    };
    let mut out = io::stdout().lock();
    for line in lines(&found, &names, &show) {
        writeln!(out, "{line}")?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A piece of pci.ids, as the database writes it.
    const IDS: &str = "\
#\tList of PCI IDs
1af4  Red Hat, Inc.
\t1000  Virtio network device
8086  Intel Corporation
\t2922  82801IR/IO/IH (ICH9R/DO/DH) 6 port SATA Controller [AHCI mode]
\t29c0  82G33/G31/P35/P31 Express DRAM Controller
\t\t1028 020d  Inspiron 530
\t\t1af4 1100  QEMU Virtual Machine
\t29c1  82G33/G31/P35/P31 Express PCI Express Root Port
8087  Intel Corporation
\t29c0  Not this one

# List of known device classes, subclasses and programming interfaces
C 01  Mass storage controller
\t06  SATA controller
\t\t01  AHCI 1.0
C 06  Bridge
\t00  Host bridge
C ff  Unassigned class
";

    fn function(
        slot: u8,
        ids: (u16, u16),
        class: u32,
        revision: u8,
        sub: Option<(u16, u16)>,
    ) -> Function {
        Function {
            domain: 0,
            bus: 0,
            slot,
            func: 0,
            vendor: ids.0,
            device: ids.1,
            class,
            revision,
            subsystem: sub,
        }
    }

    fn q35() -> Vec<Function> {
        vec![
            function(0, (0x8086, 0x29c0), 0x060000, 0, Some((0x1af4, 0x1100))),
            function(2, (0x1234, 0x1111), 0x030000, 2, Some((0x1af4, 0x1100))),
            function(0x1f, (0x8086, 0x2922), 0x010601, 2, Some((0x1af4, 0x1100))),
        ]
    }

    fn show(numbers: Ids, verbose: bool, machine: bool) -> Show {
        Show { numbers, verbose, machine, domains: false }
    }

    #[test]
    fn names_what_the_database_knows() {
        let f = q35();
        let names = Names::read(IDS, &f);
        // Only what was asked about is kept.
        assert!(!names.devices.contains_key(&(0x8086, 0x29c1)));
        assert!(!names.vendors.contains_key(&0x8087));
        assert_eq!(
            lines(&f, &names, &show(Ids::Names, false, false)),
            [
                "00:00.0 Host bridge: Intel Corporation 82G33/G31/P35/P31 Express DRAM Controller",
                "00:02.0 Class 0300: Device 1234:1111 (rev 02)",
                "00:1f.0 SATA controller: Intel Corporation 82801IR/IO/IH (ICH9R/DO/DH) 6 port \
                 SATA Controller [AHCI mode] (rev 02)",
            ]
        );
        let both = lines(&f, &names, &show(Ids::Both, false, false));
        assert_eq!(
            both[0],
            "00:00.0 Host bridge [0600]: Intel Corporation 82G33/G31/P35/P31 Express DRAM \
             Controller [8086:29c0]"
        );
        assert_eq!(both[1], "00:02.0 Class 0300: Device [1234:1111] (rev 02)");
        let numbers = lines(&f, &Names::default(), &show(Ids::Numbers, false, false));
        assert_eq!(numbers[2], "00:1f.0 0106: 8086:2922 (rev 02)");
    }

    #[test]
    fn verbose_and_machine_readable() {
        let f = q35();
        let names = Names::read(IDS, &f);
        let verbose = lines(&f, &names, &show(Ids::Names, true, false));
        assert_eq!(verbose[1], "\tSubsystem: Red Hat, Inc. QEMU Virtual Machine");
        assert_eq!(verbose[2], "");
        // The vendor is known, the subsystem not.
        assert_eq!(verbose[4], "\tSubsystem: Red Hat, Inc. Device 1100");
        assert!(verbose[6].ends_with("(rev 02) (prog-if 01 [AHCI 1.0])"), "{}", verbose[6]);
        let machine = lines(&f, &names, &show(Ids::Names, false, true));
        assert_eq!(
            machine[0],
            "00:00.0 \"Host bridge\" \"Intel Corporation\" \"82G33/G31/P35/P31 Express DRAM \
             Controller\" \"Red Hat, Inc.\" \"QEMU Virtual Machine\""
        );
        assert!(
            machine[2].contains("\" -r02 -p01 \"Red Hat, Inc.\" \"Device 1100\""),
            "{}",
            machine[2]
        );
        let numbers = lines(&f, &names, &show(Ids::Numbers, false, true));
        assert_eq!(numbers[0], "00:00.0 \"0600\" \"8086\" \"29c0\" \"1af4\" \"1100\"");
    }

    #[test]
    fn filters_and_addresses() {
        assert_eq!(parse_address("0000:00:1f.2"), Some((0, 0, 0x1f, 2)));
        assert_eq!(parse_address("0001:3a:00.7"), Some((1, 0x3a, 0, 7)));
        assert_eq!(parse_address("0000:00:20.0"), None);
        assert_eq!(parse_address("0000:00:1f.8"), None);
        assert_eq!(parse_address("00:1f.2"), None);
        let f = q35();
        let picked = |slot: Option<&str>, ids: Option<&str>| {
            let mut filter = Filter::default();
            if let Some(slot) = slot {
                filter.slot(slot).unwrap();
            }
            if let Some(ids) = ids {
                filter.device(ids).unwrap();
            }
            f.iter().filter(|f| filter.matches(f)).map(|f| f.slot).collect::<Vec<_>>()
        };
        assert_eq!(picked(Some("1f"), None), [0x1f]);
        assert_eq!(picked(Some("0:0:1f.0"), None), [0x1f]);
        assert_eq!(picked(Some(".0"), None), [0, 2, 0x1f]);
        assert_eq!(picked(Some("1:"), None), [0u8; 0]);
        assert_eq!(picked(None, Some("8086:")), [0, 0x1f]);
        assert_eq!(picked(None, Some(":2922")), [0x1f]);
        assert_eq!(picked(None, Some("::0600")), [0]);
        assert!(Filter::default().slot("0:0:0:0").is_err());
        assert!(Filter::default().slot("20").is_err());
        assert!(Filter::default().device("8086").is_err());
        assert!(Filter::default().device("zz:1").is_err());
        let domains = Show { domains: true, ..show(Ids::Numbers, false, false) };
        assert!(lines(&f, &Names::default(), &domains)[0].starts_with("0000:00:00.0 0600"));
    }

    #[test]
    fn reads_what_the_kernel_shows() {
        let dir = std::env::temp_dir().join(format!("lspci-test-{}", std::process::id()));
        for (name, files) in [
            (
                "0000:00:1f.2",
                [
                    ("vendor", "0x8086"),
                    ("device", "0x2922"),
                    ("class", "0x010601"),
                    ("revision", "0x02"),
                ],
            ),
            (
                "0000:00:00.0",
                [
                    ("vendor", "0x8086"),
                    ("device", "0x29c0"),
                    ("class", "0x060000"),
                    ("revision", "0x00"),
                ],
            ),
        ] {
            let path = dir.join(name);
            fs::create_dir_all(&path).unwrap();
            for (file, value) in files {
                fs::write(path.join(file), format!("{value}\n")).unwrap();
            }
            fs::write(path.join("subsystem_vendor"), "0x1af4\n").unwrap();
            fs::write(path.join("subsystem_device"), "0x1100\n").unwrap();
        }
        // Not a function.
        fs::create_dir_all(dir.join("stray")).unwrap();
        let found = functions(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].slot, found[0].revision), (0, 0));
        assert_eq!((found[1].slot, found[1].func, found[1].revision), (0x1f, 2, 2));
        assert_eq!(found[1].class, 0x010601);
        assert_eq!(found[1].subsystem, Some((0x1af4, 0x1100)));
    }
}
