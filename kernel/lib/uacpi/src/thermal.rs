// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Thermal zones (Phase 11c, ADR 0203): what a zone's objects say, and
//! the passive cooling they call for, as Linux's ACPI thermal driver and
//! its step-wise governor have them.
//!
//! A zone's `_TMP` is its temperature in tenths of a kelvin. Its trip
//! points are `_CRT`, at which the machine powers off, `_HOT`, at which
//! it would hibernate, and `_PSV`, from which the CPUs slow down. Passive
//! cooling needs ACPI's constants too: `_TC1` and `_TC2` weigh how fast
//! the temperature changes and how far it is past the trip point, and
//! `_TSP` is how often, in tenths of a second, to look again while it
//! cools ([`Cooling`]). A trip point at or below 0 °C or above 200 °C
//! is nonsense some firmware gives, and is left out.

use core::fmt;

/// 0 °C, in tenths of a kelvin, as Linux rounds it.
pub const ZERO_CELSIUS: u64 = 2732;
/// The hottest trip point that makes sense: 200 °C.
const HOTTEST_TRIP: u64 = ZERO_CELSIUS + 2000;

/// The most steps of passive cooling, as Linux's processor cooling has
/// them: each takes a fifth of the CPUs' highest level off.
pub const STEPS: u8 = 3;

/// A temperature, in tenths of a kelvin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Temperature(pub u64);

impl Temperature {
    /// In thousandths of a degree Celsius, Linux's unit.
    pub fn millicelsius(self) -> i64 {
        (i64::try_from(self.0).unwrap_or(i64::MAX) - ZERO_CELSIUS as i64) * 100
    }

    /// A trip point's value, unless it is nonsense.
    fn trip(value: Option<u64>) -> Option<Temperature> {
        value.filter(|&value| ZERO_CELSIUS < value && value <= HOTTEST_TRIP).map(Temperature)
    }
}

impl fmt::Display for Temperature {
    /// In degrees Celsius, with a tenth where there is one: `61.5 °C`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tenths = self.millicelsius() / 100;
        let sign = if tenths < 0 { "-" } else { "" };
        let (whole, tenth) = (tenths.unsigned_abs() / 10, tenths.unsigned_abs() % 10);
        if tenth == 0 {
            write!(f, "{sign}{whole} °C")
        } else {
            write!(f, "{sign}{whole}.{tenth} °C")
        }
    }
}

/// A passive trip point and its constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Passive {
    /// `_PSV`.
    pub at: Temperature,
    /// `_TC1`, the weight of the change since the last look.
    pub tc1: u64,
    /// `_TC2`, the weight of how far past the trip point it is.
    pub tc2: u64,
    /// `_TSP`, how often to look while cooling, in tenths of a second.
    pub period_ds: u64,
}

/// A zone's trip points.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trips {
    /// `_CRT`.
    pub critical: Option<Temperature>,
    /// `_HOT`.
    pub hot: Option<Temperature>,
    /// `_PSV`, with what passive cooling needs.
    pub passive: Option<Passive>,
}

impl Trips {
    /// The trip points from the zone's objects, each `None` where the zone
    /// lacks it. As in Linux, a passive trip point needs `_TC1`, `_TC2`
    /// and a `_TSP` other than 0.
    pub fn new(
        critical: Option<u64>,
        hot: Option<u64>,
        passive: Option<u64>,
        constants: [Option<u64>; 3],
    ) -> Trips {
        let passive = match (Temperature::trip(passive), constants) {
            (Some(at), [Some(tc1), Some(tc2), Some(period_ds)]) if period_ds > 0 => {
                Some(Passive { at, tc1, tc2, period_ds })
            }
            _ => None,
        };
        Trips { critical: Temperature::trip(critical), hot: Temperature::trip(hot), passive }
    }

    /// The trip points in Linux's order and with its names, as
    /// `trip_point_<n>_type` gives them: critical, hot, passive.
    pub fn listed(&self) -> impl Iterator<Item = (&'static str, Temperature)> {
        [
            self.critical.map(|at| ("critical", at)),
            self.hot.map(|at| ("hot", at)),
            self.passive.map(|passive| ("passive", passive.at)),
        ]
        .into_iter()
        .flatten()
    }
}

/// A zone's passive cooling: how many steps the CPUs are slowed by, which
/// Linux's step-wise governor decides over ACPI's trend (the ACPI
/// specification, section 11.1.5): `_TC1 × (Tn − Tn−1) + _TC2 × (Tn − Tt)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cooling {
    step: u8,
    last: Option<Temperature>,
}

impl Cooling {
    /// How many steps the CPUs are slowed by, from 0 to [`STEPS`].
    pub fn step(&self) -> u8 {
        self.step
    }

    /// Takes the zone's temperature `now` and returns the new step: one
    /// more while it is at or past the trip point and rising, one fewer
    /// once it is below it and falling.
    pub fn update(&mut self, now: Temperature, passive: &Passive) -> u8 {
        let last = self.last.replace(now).unwrap_or(now);
        let difference = |a: Temperature, b: Temperature| i128::from(a.0) - i128::from(b.0);
        let trend = i128::from(passive.tc1) * difference(now, last)
            + i128::from(passive.tc2) * difference(now, passive.at);
        self.step = match (now >= passive.at, trend.signum()) {
            (true, 1) => (self.step + 1).min(STEPS),
            (false, -1) => self.step.saturating_sub(1),
            _ => self.step,
        };
        self.step
    }

    /// Whether the zone is cooling: looked at every `_TSP`, then.
    pub fn active(&self, now: Temperature, passive: &Passive) -> bool {
        self.step > 0 || now >= passive.at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn celsius(degrees: u64) -> u64 {
        ZERO_CELSIUS + degrees * 10
    }

    #[test]
    fn temperatures_read_as_linux_reads_them() {
        assert_eq!(Temperature(celsius(45)).millicelsius(), 45_000);
        assert_eq!(Temperature(3187).millicelsius(), 45_500);
        assert_eq!(Temperature(2700).millicelsius(), -3_200);
        assert_eq!(Temperature(celsius(60)).to_string(), "60 °C");
        assert_eq!(Temperature(3187).to_string(), "45.5 °C");
        assert_eq!(Temperature(2700).to_string(), "-3.2 °C");
    }

    #[test]
    fn nonsense_trip_points_are_left_out() {
        let constants = [Some(2), Some(5), Some(10)];
        let trips = Trips::new(Some(celsius(80)), Some(celsius(70)), Some(celsius(60)), constants);
        let passive = Passive { at: Temperature(celsius(60)), tc1: 2, tc2: 5, period_ds: 10 };
        assert_eq!(trips.passive, Some(passive));
        let listed: Vec<_> = trips.listed().map(|(kind, at)| (kind, at.millicelsius())).collect();
        assert_eq!(listed, [("critical", 80_000), ("hot", 70_000), ("passive", 60_000)]);

        // 0 °C, 0 K and 6553.5 K are what broken firmware gives.
        let trips = Trips::new(Some(ZERO_CELSIUS), Some(0), Some(0xffff), constants);
        assert_eq!(trips, Trips::default());
        assert_eq!(trips.listed().count(), 0);
        // Passive cooling needs its constants, and a period.
        let trips = Trips::new(None, None, Some(celsius(60)), [Some(2), None, Some(10)]);
        assert_eq!(trips.passive, None);
        let trips = Trips::new(None, None, Some(celsius(60)), [Some(2), Some(5), Some(0)]);
        assert_eq!(trips.passive, None);
    }

    #[test]
    fn passive_cooling_steps_as_linux_steps() {
        let passive = Passive { at: Temperature(celsius(60)), tc1: 2, tc2: 5, period_ds: 10 };
        let mut cooling = Cooling::default();
        let mut step = |degrees: u64| cooling.update(Temperature(celsius(degrees)), &passive);
        // Below the trip point nothing happens.
        assert_eq!([step(50), step(55), step(59)], [0, 0, 0]);
        // At and past it, rising, a step more each look, up to three.
        assert_eq!([step(60), step(62), step(64), step(66), step(68)], [1, 2, 3, 3, 3]);
        // Still past it but falling: the steps stay.
        assert_eq!(step(61), 3);
        // Below it and falling: a step fewer each look.
        assert_eq!([step(58), step(57), step(56), step(55)], [2, 1, 0, 0]);
        // Back at it, then steady there: ACPI's trend is 0, and the step stays.
        assert_eq!([step(60), step(60)], [1, 1]);
        assert!(cooling.active(Temperature(celsius(59)), &passive));

        let idle = Cooling::default();
        assert!(idle.active(Temperature(celsius(60)), &passive));
        assert!(!idle.active(Temperature(celsius(59)), &passive));
    }
}
