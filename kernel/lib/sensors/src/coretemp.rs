// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Intel's digital thermal sensors, as Linux's `coretemp` shows them.
//!
//! Where CPUID leaf 6's EAX bit 0 (DTS) says so, each core counts its
//! temperature in IA32_THERM_STATUS (MSR 19Ch): bits 22:16 are the
//! digital readout, in degrees below TjMax, while bit 31 says the reading
//! is valid; bit 5 logs that the critical temperature was reached. Where
//! bit 6 (PTM) says so, IA32_PACKAGE_THERM_STATUS (1B1h) counts the
//! package's the same way. MSR_TEMPERATURE_TARGET (1A2h) gives TjMax, the
//! temperature the processor's thermal control starts at, in bits 23:16,
//! and in bits 15:8 how far below it the control aims to keep the
//! processor (Linux's `temp<n>_max`). A processor that gives no TjMax is
//! taken to have one of 100 degrees, as Linux takes most.
//!
//! Each CPU reads its own registers ([`sample`]); the threads of a core
//! read the same core sensor.

/// CPUID leaf 6, EAX: each core has a digital thermal sensor.
pub const LEAF6_DTS: u32 = 1 << 0;
/// CPUID leaf 6, EAX: the package has one.
pub const LEAF6_PTM: u32 = 1 << 6;

/// IA32_THERM_STATUS: the core's sensor.
pub const THERM_STATUS: u32 = 0x19c;
/// MSR_TEMPERATURE_TARGET: TjMax and the control's target below it.
pub const TEMPERATURE_TARGET: u32 = 0x1a2;
/// IA32_PACKAGE_THERM_STATUS: the package's sensor.
pub const PACKAGE_THERM_STATUS: u32 = 0x1b1;

/// A sensor's reading is valid.
const READING_VALID: u64 = 1 << 31;
/// The critical temperature was reached.
const CRITICAL_LOG: u64 = 1 << 5;
/// TjMax where the processor gives none.
pub const DEFAULT_TJMAX: i32 = 100_000;

/// The temperatures a CPU's sensors count from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// TjMax: where the processor's thermal control starts
    /// (`temp<n>_crit`).
    pub tjmax: i32,
    /// The control's target below it (`temp<n>_max`), where the processor
    /// gives it.
    pub max: Option<i32>,
}

impl Target {
    /// From MSR_TEMPERATURE_TARGET, `None` if the processor has none.
    pub fn from_register(register: Option<u64>) -> Target {
        let tjmax = register.map_or(0, |value| ((value >> 16) & 0xff) as i32);
        if tjmax == 0 {
            return Target { tjmax: DEFAULT_TJMAX, max: None };
        }
        let below = register.map_or(0, |value| ((value >> 8) & 0xff) as i32);
        Target { tjmax: tjmax * 1000, max: Some((tjmax - below) * 1000) }
    }
}

/// What one sensor says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// Its temperature, while its reading is valid (some processors clear
    /// the bit while too cold to measure).
    pub temperature: Option<i32>,
    /// It reached the critical temperature (`temp<n>_crit_alarm`).
    pub alarm: bool,
}

impl Reading {
    /// From IA32_THERM_STATUS or IA32_PACKAGE_THERM_STATUS, counting from
    /// `tjmax`.
    pub fn from_register(status: u64, tjmax: i32) -> Reading {
        let below = ((status >> 16) & 0x7f) as i32;
        Reading {
            temperature: (status & READING_VALID != 0).then_some(tjmax - below * 1000),
            alarm: status & CRITICAL_LOG != 0,
        }
    }
}

/// What a CPU's sensors say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// What they count from.
    pub target: Target,
    /// Its core's.
    pub core: Reading,
    /// Its package's, if it has one.
    pub package: Option<Reading>,
}

/// The running CPU's model-specific registers.
pub trait Msrs {
    /// Reads `msr`; `None` if the processor has no such register (the
    /// read faulted).
    fn read(&mut self, msr: u32) -> Option<u64>;
}

/// What the running CPU's sensors say, for an Intel processor whose CPUID
/// leaf 6 has `leaf6_eax`; `None` if it has no digital thermal sensor, or
/// one whose register cannot be read (as under some hypervisors).
pub fn sample(leaf6_eax: u32, msrs: &mut dyn Msrs) -> Option<Sample> {
    if leaf6_eax & LEAF6_DTS == 0 {
        return None;
    }
    let target = Target::from_register(msrs.read(TEMPERATURE_TARGET));
    let core = Reading::from_register(msrs.read(THERM_STATUS)?, target.tjmax);
    let package = if leaf6_eax & LEAF6_PTM != 0 {
        msrs.read(PACKAGE_THERM_STATUS).map(|status| Reading::from_register(status, target.tjmax))
    } else {
        None
    };
    Some(Sample { target, core, package })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::collections::HashMap;

    use super::*;

    struct Registers(HashMap<u32, u64>);

    impl Msrs for Registers {
        fn read(&mut self, msr: u32) -> Option<u64> {
            self.0.get(&msr).copied()
        }
    }

    /// A status register reading `below` degrees under TjMax.
    fn status(below: u64) -> u64 {
        READING_VALID | below << 16
    }

    #[test]
    fn cores_and_packages_count_below_tjmax() {
        // A Kaby Lake, the Dell's: TjMax 100 degrees, its target 3 below.
        let mut msrs = Registers(HashMap::from([
            (TEMPERATURE_TARGET, 0x0064_0300),
            (THERM_STATUS, status(55)),
            (PACKAGE_THERM_STATUS, status(52) | CRITICAL_LOG),
        ]));
        let sample = sample(LEAF6_DTS | LEAF6_PTM, &mut msrs).unwrap();
        assert_eq!(sample.target, Target { tjmax: 100_000, max: Some(97_000) });
        assert_eq!(sample.core, Reading { temperature: Some(45_000), alarm: false });
        assert_eq!(sample.package, Some(Reading { temperature: Some(48_000), alarm: true }));
    }

    #[test]
    fn without_a_target_tjmax_is_100_degrees() {
        let mut msrs = Registers(HashMap::from([(THERM_STATUS, status(70))]));
        let sample = sample(LEAF6_DTS, &mut msrs).unwrap();
        assert_eq!(sample.target, Target { tjmax: DEFAULT_TJMAX, max: None });
        assert_eq!(sample.core.temperature, Some(30_000));
        // No package sensor without PTM.
        assert_eq!(sample.package, None);
        // A target register that reads 0 gives no TjMax either.
        assert_eq!(Target::from_register(Some(0)), Target { tjmax: DEFAULT_TJMAX, max: None });
    }

    #[test]
    fn invalid_readings_and_missing_sensors() {
        let reading = Reading::from_register(status(20) & !READING_VALID, 100_000);
        assert_eq!(reading, Reading { temperature: None, alarm: false });
        // No DTS, or a status register that faults: no sensor.
        let mut msrs = Registers(HashMap::from([(THERM_STATUS, status(10))]));
        assert_eq!(sample(LEAF6_PTM, &mut msrs), None);
        let mut msrs = Registers(HashMap::new());
        assert_eq!(sample(LEAF6_DTS | LEAF6_PTM, &mut msrs), None);
    }
}
