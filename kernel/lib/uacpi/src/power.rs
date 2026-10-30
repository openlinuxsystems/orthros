// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What ACPI's battery (`PNP0C0A`) says of itself, read as Linux reads it
//! (`drivers/acpi/battery.c`) for its `power_supply` class (Phase 11c,
//! ADR 0201).
//!
//! `_BIX` (ACPI 4.0 and later) or else `_BIF` describe the battery: its
//! unit (milliwatts or milliamperes), its design and last full capacity,
//! voltage, cycle count and names; `_BST` says what it does now: charging
//! or discharging, at what rate, with how much left. A value of
//! 0xFFFFFFFF is one the battery does not know. The kernel evaluates them
//! (`Node::package`) and hands the packages here.

use core::fmt;

/// What a battery does not know.
const UNKNOWN: u64 = 0xFFFF_FFFF;

/// The elements of a package AML returned.
pub trait Fields {
    /// How many there are.
    fn count(&self) -> usize;
    /// Element `index`, if it is an integer.
    fn integer(&self, index: usize) -> Option<u64>;
    /// Element `index`, if it is a string or a buffer: its bytes.
    fn text(&self, index: usize) -> Option<&[u8]>;
}

/// A short text of the battery's, as its model or maker: printable ASCII
/// only, up to 32 bytes.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Name {
    bytes: [u8; 32],
    len: u8,
}

impl Name {
    fn from(fields: &dyn Fields, index: usize) -> Self {
        let mut name = Self::default();
        let text = fields.text(index).unwrap_or_default();
        let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
        for &byte in text[..end].trim_ascii() {
            if usize::from(name.len) == name.bytes.len() {
                break;
            }
            if byte.is_ascii_graphic() || byte == b' ' {
                name.bytes[usize::from(name.len)] = byte;
                name.len += 1;
            }
        }
        name
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        // Only ASCII is kept.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// A battery's description, from `_BIX` or `_BIF`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Info {
    /// Capacities in milliampere-hours and rates in milliamperes, not
    /// milliwatt-hours and milliwatts.
    pub amperes: bool,
    /// The capacity it was made with.
    pub design_capacity: Option<u32>,
    /// The capacity of its last full charge.
    pub full_capacity: Option<u32>,
    /// Whether it can be charged.
    pub rechargeable: bool,
    /// The voltage it was made for, in millivolts.
    pub design_voltage: Option<u32>,
    /// The capacity at which the firmware warns.
    pub warning_capacity: Option<u32>,
    /// The capacity at which it is low.
    pub low_capacity: Option<u32>,
    /// How many times it has been charged, from `_BIX` only.
    pub cycle_count: Option<u32>,
    /// Its model.
    pub model: Name,
    /// Its serial number.
    pub serial: Name,
    /// Its chemistry, as "LION".
    pub kind: Name,
    /// Its maker.
    pub maker: Name,
}

fn known(value: Option<u64>) -> Option<u32> {
    match value {
        Some(UNKNOWN) | None => None,
        Some(value) => u32::try_from(value).ok(),
    }
}

impl Info {
    /// From `_BIX`: revision, unit, capacities, technology, voltage,
    /// warning and low capacities, cycle count, measurement details, then
    /// model, serial number, type and maker.
    pub fn from_bix(fields: &dyn Fields) -> Option<Self> {
        if fields.count() < 20 {
            return None;
        }
        let at = |index| known(fields.integer(index));
        Some(Self {
            amperes: fields.integer(1)? == 1,
            design_capacity: at(2),
            full_capacity: at(3),
            rechargeable: fields.integer(4) == Some(1),
            design_voltage: at(5),
            warning_capacity: at(6),
            low_capacity: at(7),
            cycle_count: at(8),
            model: Name::from(fields, 16),
            serial: Name::from(fields, 17),
            kind: Name::from(fields, 18),
            maker: Name::from(fields, 19),
        })
    }

    /// From `_BIF`: `_BIX` without its revision, cycle count and
    /// measurement details.
    pub fn from_bif(fields: &dyn Fields) -> Option<Self> {
        if fields.count() < 13 {
            return None;
        }
        let at = |index| known(fields.integer(index));
        Some(Self {
            amperes: fields.integer(0)? == 1,
            design_capacity: at(1),
            full_capacity: at(2),
            rechargeable: fields.integer(3) == Some(1),
            design_voltage: at(4),
            warning_capacity: at(5),
            low_capacity: at(6),
            cycle_count: None,
            model: Name::from(fields, 9),
            serial: Name::from(fields, 10),
            kind: Name::from(fields, 11),
            maker: Name::from(fields, 12),
        })
    }

    /// The capacity a full battery has: of its last full charge, or the
    /// design's.
    pub fn capacity(&self) -> Option<u32> {
        self.full_capacity.filter(|&c| c != 0).or(self.design_capacity.filter(|&c| c != 0))
    }

    /// Its technology, as Linux names it from the type.
    pub fn technology(&self) -> &'static str {
        let kind = self.kind.as_str();
        if kind.eq_ignore_ascii_case("NiCd") {
            "NiCd"
        } else if kind.eq_ignore_ascii_case("NiMH") {
            "NiMH"
        } else if kind.eq_ignore_ascii_case("LION")
            || kind.get(..6).is_some_and(|start| start.eq_ignore_ascii_case("LI-ION"))
        {
            "Li-ion"
        } else if kind.eq_ignore_ascii_case("LiP") {
            "Li-poly"
        } else {
            "Unknown"
        }
    }
}

/// What a battery does now, from `_BST`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// It is discharging.
    pub discharging: bool,
    /// It is charging.
    pub charging: bool,
    /// It is critically low.
    pub critical: bool,
    /// Its charge is held below full (ACPI 6.0's charge limiting).
    pub limited: bool,
    /// The rate it charges or discharges at.
    pub rate: Option<u32>,
    /// What is left.
    pub remaining: Option<u32>,
    /// Its voltage now, in millivolts.
    pub voltage: Option<u32>,
}

impl State {
    /// From `_BST`: state bits, rate, remaining capacity, voltage.
    pub fn from_bst(fields: &dyn Fields) -> Option<Self> {
        if fields.count() < 4 {
            return None;
        }
        let bits = fields.integer(0)?;
        let at = |index| known(fields.integer(index));
        Some(Self {
            discharging: bits & 1 != 0,
            charging: bits & 2 != 0,
            critical: bits & 4 != 0,
            limited: bits & 8 != 0,
            rate: at(1),
            remaining: at(2),
            voltage: at(3),
        })
    }

    /// Whether it is full, as Linux's `acpi_battery_is_charged` decides:
    /// neither charging nor discharging, with its last full charge or at
    /// least its design capacity left.
    pub fn is_full(&self, info: &Info) -> bool {
        if self.discharging || self.charging || self.critical || self.limited {
            return false;
        }
        let Some(remaining) = self.remaining.filter(|&r| r != 0) else { return false };
        info.full_capacity == Some(remaining)
            || info.design_capacity.is_some_and(|d| d <= remaining)
    }

    /// Linux's `status`: Charging, Discharging, Full or Not charging.
    pub fn status(&self, info: &Info) -> &'static str {
        if self.discharging {
            "Discharging"
        } else if self.charging {
            "Charging"
        } else if self.limited || !self.is_full(info) {
            "Not charging"
        } else {
            "Full"
        }
    }

    /// Linux's `capacity`: what is left, in percent of a full charge.
    pub fn percent(&self, info: &Info) -> Option<u32> {
        let full = u64::from(info.capacity()?);
        let remaining = u64::from(self.remaining?);
        Some(((remaining * 100 + full / 2) / full).min(100) as u32)
    }

    /// Linux's `capacity_level`: Critical, Low, Full or Normal.
    pub fn level(&self, info: &Info) -> &'static str {
        if self.critical {
            "Critical"
        } else if info.low_capacity.zip(self.remaining).is_some_and(|(low, left)| left <= low) {
            "Low"
        } else if self.is_full(info) {
            "Full"
        } else {
            "Normal"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    enum Element {
        Integer(u64),
        Text(&'static [u8]),
    }

    struct Fixed<'a>(&'a [Element]);

    impl Fields for Fixed<'_> {
        fn count(&self) -> usize {
            self.0.len()
        }

        fn integer(&self, index: usize) -> Option<u64> {
            match self.0.get(index)? {
                Element::Integer(value) => Some(*value),
                Element::Text(_) => None,
            }
        }

        fn text(&self, index: usize) -> Option<&[u8]> {
            match self.0.get(index)? {
                Element::Text(text) => Some(text),
                Element::Integer(_) => None,
            }
        }
    }

    use Element::{Integer as I, Text as T};

    const BIX: Fixed<'_> = Fixed(&[
        I(1),
        I(0),
        I(50_000),
        I(48_000),
        I(1),
        I(11_400),
        I(5_000),
        I(2_000),
        I(42),
        I(95_000),
        I(UNKNOWN),
        I(UNKNOWN),
        I(UNKNOWN),
        I(UNKNOWN),
        I(10),
        I(10),
        T(b"DELL 0X1234\0"),
        T(b" 1234 "),
        T(b"LION\0"),
        T(b"SMP\x01"),
        I(1),
    ]);

    const BIF: Fixed<'_> = Fixed(&[
        I(1),
        I(4_000),
        I(UNKNOWN),
        I(1),
        I(14_800),
        I(400),
        I(100),
        I(10),
        I(10),
        T(b"Model"),
        T(b""),
        T(b"LiP"),
        T(b"Maker"),
    ]);

    fn bst(bits: u64, rate: u64, remaining: u64) -> [Element; 4] {
        [I(bits), I(rate), I(remaining), I(12_100)]
    }

    fn state(bits: u64, rate: u64, remaining: u64) -> State {
        State::from_bst(&Fixed(&bst(bits, rate, remaining))).unwrap()
    }

    #[test]
    fn a_battery_describes_itself_through_bix_or_bif() {
        let info = Info::from_bix(&BIX).unwrap();
        assert!(!info.amperes && info.rechargeable);
        assert_eq!(
            (info.design_capacity, info.full_capacity, info.design_voltage, info.cycle_count),
            (Some(50_000), Some(48_000), Some(11_400), Some(42))
        );
        assert_eq!(
            (info.model.as_str(), info.serial.as_str(), info.maker.as_str()),
            ("DELL 0X1234", "1234", "SMP")
        );
        assert_eq!(info.technology(), "Li-ion");
        assert_eq!(info.capacity(), Some(48_000));

        let info = Info::from_bif(&BIF).unwrap();
        assert!(info.amperes);
        assert_eq!((info.full_capacity, info.cycle_count), (None, None));
        // Without a last full charge, the design's capacity is full.
        assert_eq!(info.capacity(), Some(4_000));
        assert_eq!((info.technology(), info.serial.as_str()), ("Li-poly", ""));

        // Too short to be either.
        assert_eq!(Info::from_bix(&BIF), None);
        assert_eq!(Info::from_bif(&Fixed(&bst(0, 0, 0))), None);
    }

    #[test]
    fn its_state_reads_as_linux_reads_it() {
        let info = Info::from_bix(&BIX).unwrap();
        let discharging = state(1, 9_000, 36_000);
        assert_eq!(discharging.status(&info), "Discharging");
        assert_eq!(discharging.percent(&info), Some(75));
        assert_eq!(discharging.level(&info), "Normal");
        assert_eq!(discharging.voltage, Some(12_100));

        assert_eq!(state(2, 20_000, 24_000).status(&info), "Charging");
        // Neither, and full by its last full charge or its design.
        assert_eq!(state(0, 0, 48_000).status(&info), "Full");
        assert_eq!(state(0, 0, 48_000).level(&info), "Full");
        assert_eq!(state(0, 0, 50_000).percent(&info), Some(100));
        // Neither, but not full: on AC with charging held off.
        assert_eq!(state(0, 0, 30_000).status(&info), "Not charging");
        assert_eq!(state(8, 0, 48_000).status(&info), "Not charging");

        assert_eq!(state(1, 9_000, 2_000).level(&info), "Low");
        assert_eq!(state(5, 9_000, 1_000).level(&info), "Critical");
        // Half a percent rounds up, as Linux's DIV_ROUND_CLOSEST.
        assert_eq!(state(1, 0, 240).percent(&info), Some(1));
        assert_eq!(state(1, 0, 239).percent(&info), Some(0));

        let unknown = state(1, UNKNOWN, UNKNOWN);
        assert_eq!((unknown.rate, unknown.remaining, unknown.percent(&info)), (None, None, None));
        assert!(!unknown.is_full(&info));
    }
}
