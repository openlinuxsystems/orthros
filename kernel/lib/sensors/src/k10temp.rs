// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! AMD's Zen temperature sensors, as Linux's `k10temp` shows them.
//!
//! Zen's System Management Unit keeps the processor's temperatures in its
//! thermal block, at SMN address 0x59800 on, which the kernel reads
//! through the root complex (00:00.0): the SMN address is written to its
//! configuration register 0x60, then the value read from 0x64. The data
//! fabric's function 3 (00:18.3) names the processor, as it is what
//! Linux's driver binds to.
//!
//! - The control temperature, Tctl (0x59800): bits 31:21 in eighths of a
//!   degree, 49 degrees less when bit 19 (range select) is set or both of
//!   bits 17:16 are. It is what the processor's cooling acts on, and on a
//!   few early Ryzens sits a fixed offset above the die's temperature.
//! - Each core complex die's temperature, Tccd (from an offset that
//!   changes with the model, 4 bytes a die): bits 10:0 in eighths of a
//!   degree above -49 degrees, while bit 11 says the die is there.
//!
//! Which dies a model has registers for is as Linux lists them; the rest,
//! the Framework Desktop's Strix Halo among them, show Tctl alone.

use radix_hw::Mmio;
use radix_hw::pci::{self, Address};

/// AMD's PCI vendor ID.
pub const AMD: u16 = 0x1022;

/// The root complex, whose configuration registers reach the SMN.
const ROOT: (u8, u8) = (0, 0);
/// The data fabric's function 3, its miscellaneous registers.
const DATA_FABRIC_MISC: (u8, u8) = (0x18, 3);
/// The root complex's SMN index and data registers.
const SMN_INDEX: usize = 0x60;
const SMN_DATA: usize = 0x64;

/// The thermal block's reported temperature control register: Tctl.
pub const REPORTED_TEMPERATURE: u32 = 0x0005_9800;
/// Tctl's temperature, from this bit on.
const CURRENT_SHIFT: u32 = 21;
/// Tctl counts from -49 degrees.
const RANGE_SELECT: u32 = 1 << 19;
/// Tctl counts from -49 degrees when both are set.
const TJ_SELECT: u32 = 0b11 << 16;
/// A die's register: the die is there.
const DIE_VALID: u32 = 1 << 11;
/// A die's register: its temperature.
const DIE_TEMPERATURE: u32 = 0x7ff;
/// The 49 degrees Tctl and the dies' temperatures may count from below 0.
const OFFSET: i32 = 49_000;

/// The data fabric's function 3 of each Zen processor, by device ID, as
/// Linux's `k10temp` binds to them: families 17h, 19h and 1Ah.
const DATA_FABRICS: &[u16] = &[
    0x1463, 0x15eb, 0x1493, 0x13f3, 0x144b, 0x1443, 0x1663, 0x1727, // 17h
    0x1653, 0x14b0, 0x167c, 0x166d, 0x14e3, 0x14f3, 0x12fb, // 19h
    0x12c3, 0x16fb, 0x12cb, 0x124b, 0x12bb, 0x1243, 0x127b, // 1Ah
];

/// The most dies a model has registers for.
pub const MAX_DIES: usize = 16;

/// The dies' labels, as Linux's.
const DIE_LABELS: [&str; MAX_DIES] = [
    "Tccd1", "Tccd2", "Tccd3", "Tccd4", "Tccd5", "Tccd6", "Tccd7", "Tccd8", "Tccd9", "Tccd10",
    "Tccd11", "Tccd12", "Tccd13", "Tccd14", "Tccd15", "Tccd16",
];

/// One of the sensors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sensor {
    /// Tctl, the control temperature.
    Control,
    /// Tccd, a core complex die's, from 0.
    Die(u8),
}

impl Sensor {
    /// Its label, as Linux's: `Tctl`, `Tccd1`...
    pub fn label(self) -> &'static str {
        match self {
            Sensor::Control => "Tctl",
            Sensor::Die(die) => DIE_LABELS[usize::from(die) % MAX_DIES],
        }
    }

    /// Its number among hwmon's temperatures, as Linux's: 1 for Tctl
    /// (`temp1_input`), and from 3 for the dies, 2 being Linux's Tdie.
    pub fn number(self) -> u8 {
        match self {
            Sensor::Control => 1,
            Sensor::Die(die) => die + 3,
        }
    }
}

/// Where a model's dies' registers start, from [`REPORTED_TEMPERATURE`],
/// and how many there may be, as Linux has them; `None` for a model whose
/// dies' registers it does not know.
fn dies(family: u32, model: u32) -> Option<(u32, usize)> {
    match (family, model) {
        (0x17, 0x01 | 0x08 | 0x11 | 0x18) => Some((0x154, 4)),
        (0x17, 0x31 | 0x47 | 0x60 | 0x68 | 0x71) => Some((0x154, 8)),
        (0x17, 0xa0..=0xaf) => Some((0x300, 8)),
        (0x19, 0x00..=0x01 | 0x08 | 0x21 | 0x50..=0x5f) => Some((0x154, 8)),
        (0x19, 0x40..=0x4f) => Some((0x300, 8)),
        (0x19, 0x60..=0x7f) => Some((0x308, 8)),
        (0x19, 0x10..=0x1f | 0xa0..=0xaf) => Some((0x300, 12)),
        (0x1a, 0x00..=0x1f) => Some((0x1f0, 16)),
        (0x1a, 0x40..=0x4f) => Some((0x308, 8)),
        _ => None,
    }
}

/// Tctl from its register, at least 0 as Linux has it.
pub fn control(register: u32) -> i32 {
    let mut temperature = (register >> CURRENT_SHIFT) as i32 * 125;
    if register & RANGE_SELECT != 0 || register & TJ_SELECT == TJ_SELECT {
        temperature -= OFFSET;
    }
    temperature.max(0)
}

/// A die's temperature from its register, if the die is there.
pub fn die(register: u32) -> Option<i32> {
    (register & DIE_VALID != 0 && register != u32::MAX)
        .then(|| (register & DIE_TEMPERATURE) as i32 * 125 - OFFSET)
}

/// The sensors of a Zen processor.
pub struct K10temp {
    /// The root complex's configuration space.
    root: Mmio,
    /// Where the dies' registers start, from [`REPORTED_TEMPERATURE`].
    dies_at: u32,
    /// The dies there, a bit each.
    dies: u16,
}

/// The configuration space of `device`.`function` on bus 0, if the
/// function is there, and its vendor and device IDs.
fn function(bus: &Mmio, (device, function): (u8, u8)) -> Option<(Mmio, u16, u16)> {
    let address = Address { segment: 0, bus: 0, device, function };
    let config = bus.window(address.offset_in_bus(), pci::CONFIG_SIZE)?;
    let (vendor, device) = (config.read16(0x00), config.read16(0x02));
    (vendor != 0xffff).then_some((config, vendor, device))
}

/// Finds the sensors of the running processor, of CPUID's `family` and
/// `model`, in `bus`, the configuration space of bus 0 of segment 0:
/// `None` unless it is a Zen whose data fabric and root complex are
/// there.
pub fn find(bus: &Mmio, family: u32, model: u32) -> Option<K10temp> {
    if !matches!(family, 0x17 | 0x19 | 0x1a) {
        return None;
    }
    let (_, vendor, device) = function(bus, DATA_FABRIC_MISC)?;
    if vendor != AMD || !DATA_FABRICS.contains(&device) {
        return None;
    }
    // A host bridge, class 06h subclass 00h.
    let (root, vendor, _) = function(bus, ROOT)?;
    if vendor != AMD || (root.read8(0x0b), root.read8(0x0a)) != (0x06, 0x00) {
        return None;
    }
    let mut found = K10temp { root, dies_at: 0, dies: 0 };
    if let Some((at, count)) = dies(family, model) {
        found.dies_at = at;
        for index in 0..count {
            if found.smn(found.die_register(index as u8)).and_then(die).is_some() {
                found.dies |= 1 << index;
            }
        }
    }
    Some(found)
}

impl K10temp {
    /// The sensors there: Tctl, then each die's.
    pub fn sensors(&self) -> impl Iterator<Item = Sensor> + '_ {
        let dies = (0..MAX_DIES as u8).filter(|&die| self.dies & (1 << die) != 0);
        core::iter::once(Sensor::Control).chain(dies.map(Sensor::Die))
    }

    /// Reads `sensor`'s temperature; `None` if it does not answer.
    ///
    /// It takes `&mut self` as the SMN's index and data registers are one
    /// pair: the kernel keeps them to one reader at a time.
    pub fn read(&mut self, sensor: Sensor) -> Option<i32> {
        match sensor {
            Sensor::Control => self.smn(REPORTED_TEMPERATURE).map(control),
            Sensor::Die(index) => self.smn(self.die_register(index)).and_then(die),
        }
    }

    fn die_register(&self, index: u8) -> u32 {
        REPORTED_TEMPERATURE + self.dies_at + 4 * u32::from(index)
    }

    /// Reads the SMN register at `address`; `None` if it reads all ones,
    /// as a register that does not answer does.
    fn smn(&mut self, address: u32) -> Option<u32> {
        self.root.write32(SMN_INDEX, address);
        let value = self.root.read32(SMN_DATA);
        (value != u32::MAX).then_some(value)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::vec::Vec;

    use radix_hw::pci::BUS_SIZE;
    use radix_hw::{Mmio, Registers};

    use super::*;

    /// A simulated bus 0: functions' configuration space, and the SMN
    /// registers the root complex's index and data registers reach.
    struct Bus {
        config: HashMap<(u8, u8), Vec<u8>>,
        smn: HashMap<u32, u32>,
        index: u32,
    }

    struct Config(Arc<Mutex<Bus>>);

    fn split(offset: usize) -> ((u8, u8), usize) {
        let function = offset / pci::CONFIG_SIZE;
        (((function / 8) as u8, (function % 8) as u8), offset % pci::CONFIG_SIZE)
    }

    impl Registers for Config {
        fn read(&self, offset: usize, size: usize) -> u64 {
            let bus = self.0.lock().unwrap();
            let (function, register) = split(offset);
            if function == ROOT && register == SMN_DATA {
                // A register that does not answer reads all ones.
                return u64::from(bus.smn.get(&bus.index).copied().unwrap_or(u32::MAX));
            }
            let ones = u64::MAX >> (64 - 8 * size);
            bus.config.get(&function).map_or(ones, |bytes| {
                let mut value = [0; 8];
                value[..size].copy_from_slice(&bytes[register..register + size]);
                u64::from_le_bytes(value)
            })
        }

        fn write(&self, offset: usize, _size: usize, value: u64) {
            let mut bus = self.0.lock().unwrap();
            if split(offset) == (ROOT, SMN_INDEX) {
                bus.index = value as u32;
            }
        }
    }

    /// A function's configuration space: its IDs and class.
    fn header(vendor: u16, device: u16, class: u8) -> Vec<u8> {
        let mut bytes = std::vec![0; pci::CONFIG_SIZE];
        bytes[0..2].copy_from_slice(&vendor.to_le_bytes());
        bytes[2..4].copy_from_slice(&device.to_le_bytes());
        bytes[0x0b] = class;
        bytes
    }

    /// A Zen with the data fabric `fabric`, and these SMN registers.
    fn zen(fabric: u16, smn: &[(u32, u32)]) -> Mmio {
        let mut config = HashMap::new();
        config.insert(ROOT, header(AMD, 0x14d8, 0x06));
        config.insert(DATA_FABRIC_MISC, header(AMD, fabric, 0x06));
        let bus = Bus { config, smn: smn.iter().copied().collect(), index: 0 };
        Mmio::fake(Arc::new(Config(Arc::new(Mutex::new(bus)))), BUS_SIZE)
    }

    /// Tctl's register for `millicelsius`, counted from 0.
    fn tctl(millicelsius: u32) -> u32 {
        (millicelsius / 125) << CURRENT_SHIFT
    }

    /// A die's register for `millicelsius`.
    fn tccd(millicelsius: i32) -> u32 {
        ((millicelsius + OFFSET) / 125) as u32 | DIE_VALID
    }

    #[test]
    fn zen_5_desktops_show_tctl_and_each_die() {
        let dies = REPORTED_TEMPERATURE + 0x308;
        let bus = zen(
            0x1663,
            &[
                (REPORTED_TEMPERATURE, tctl(45_500)),
                (dies, tccd(42_000)),
                (dies + 4, tccd(-1_000)),
                // A die that is not there, and one that does not answer.
                (dies + 8, 0),
            ],
        );
        let mut found = find(&bus, 0x1a, 0x44).unwrap();
        let sensors: Vec<Sensor> = found.sensors().collect();
        assert_eq!(sensors, [Sensor::Control, Sensor::Die(0), Sensor::Die(1)]);
        assert_eq!(found.read(Sensor::Control), Some(45_500));
        assert_eq!(found.read(Sensor::Die(0)), Some(42_000));
        assert_eq!(found.read(Sensor::Die(1)), Some(-1_000));
        let labels: Vec<(&str, u8)> = sensors.iter().map(|s| (s.label(), s.number())).collect();
        assert_eq!(labels, [("Tctl", 1), ("Tccd1", 3), ("Tccd2", 4)]);
    }

    #[test]
    fn models_without_known_dies_show_tctl_alone() {
        // The Framework Desktop's Strix Halo: family 1Ah, model 70h.
        let bus = zen(0x12bb, &[(REPORTED_TEMPERATURE, tctl(60_000))]);
        let mut found = find(&bus, 0x1a, 0x70).unwrap();
        assert_eq!(found.sensors().collect::<Vec<_>>(), [Sensor::Control]);
        assert_eq!(found.read(Sensor::Control), Some(60_000));
    }

    #[test]
    fn tctl_counts_from_minus_49_where_it_says_so() {
        assert_eq!(control(tctl(50_000)), 50_000);
        assert_eq!(control(tctl(99_000) | RANGE_SELECT), 50_000);
        assert_eq!(control(tctl(99_000) | TJ_SELECT), 50_000);
        // One of the two bits alone does not.
        assert_eq!(control(tctl(99_000) | 1 << 16), 99_000);
        // Below 0, Linux shows 0.
        assert_eq!(control(tctl(10_000) | RANGE_SELECT), 0);
        assert_eq!(die(tccd(30_000)), Some(30_000));
        assert_eq!(die(tccd(30_000) & !DIE_VALID), None);
        assert_eq!(die(u32::MAX), None);
    }

    #[test]
    fn other_processors_have_none() {
        let bus = zen(0x1663, &[(REPORTED_TEMPERATURE, tctl(40_000))]);
        // Before Zen, and Intel's family 6.
        assert!(find(&bus, 0x15, 0x60).is_none());
        assert!(find(&bus, 0x06, 0x8e).is_none());
        // A data fabric that is not one of Zen's.
        assert!(find(&zen(0x1234, &[]), 0x19, 0x21).is_none());
        // QEMU's q35, whose root complex is Intel's and has no 00:18.3.
        let mut config = HashMap::new();
        config.insert(ROOT, header(0x8086, 0x29c0, 0x06));
        let bus = Bus { config, smn: HashMap::new(), index: 0 };
        let bus = Mmio::fake(Arc::new(Config(Arc::new(Mutex::new(bus)))), BUS_SIZE);
        assert!(find(&bus, 0x19, 0x21).is_none());
    }
}
