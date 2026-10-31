// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! CPUs that pick their own speed (Phase 11c, ADR 0202): Intel's hardware
//! P-states (HWP) and AMD's Collaborative Processor Performance Control
//! (CPPC) through its registers.
//!
//! Either CPU is given a range of performance levels and an energy
//! preference, and picks its speed within them itself, far more often
//! than an operating system could. A level is a number from 0 to 255, the
//! CPU's own measure of its speed. [`Driver::start`] turns the control on
//! for the running CPU and reads its [`Levels`]: the lowest, the most
//! efficient, the nominal (Intel's guaranteed) and the highest. A
//! [`Request`] is what the CPU is asked for: its lowest and highest level,
//! a desired one (0, the CPU's choice), and an energy preference from 0
//! (all for performance) to 255 (all for saving power), whose usual values
//! Linux names ([`Preference`]).
//!
//! Frequencies are for people, a level times a [`Scale`]: Intel's levels
//! are 100 MHz apart, but on hybrid CPUs; AMD's count fractions of the
//! nominal frequency. [`running_khz`] works out the frequency a CPU ran
//! at from its APERF and MPERF counters.
//!
//! References: Intel's SDM, volume 3, section 15.4 (Hardware-Controlled
//! Performance States); AMD's Processor Programming Reference for family
//! 19h (MSRC001_02B0 to MSRC001_02B3); Linux's `intel_pstate.c` and
//! `amd-pstate.c`, as the reference for their use (no code taken).
//!
//! The architecture reads and writes the registers; this crate only works
//! out their values, so its tests run on the host.

#![cfg_attr(not(test), no_std)]

use core::fmt;

/// IA32_PM_ENABLE: bit 0 turns HWP on, until the CPU is reset.
pub const PM_ENABLE: u32 = 0x770;
/// IA32_HWP_CAPABILITIES: the CPU's levels.
pub const HWP_CAPABILITIES: u32 = 0x771;
/// IA32_HWP_REQUEST: what the CPU is asked for.
pub const HWP_REQUEST: u32 = 0x774;

/// MSRC001_02B0, CPPC's first capability register: the CPU's levels.
pub const CPPC_CAPABILITY: u32 = 0xc001_02b0;
/// MSRC001_02B1: bit 0 turns CPPC on, until the CPU is reset.
pub const CPPC_ENABLE: u32 = 0xc001_02b1;
/// MSRC001_02B3: what the CPU is asked for.
pub const CPPC_REQUEST: u32 = 0xc001_02b3;

/// IA32_MPERF: counts at the TSC's frequency while the CPU runs.
pub const MPERF: u32 = 0xe7;
/// IA32_APERF: counts at the CPU's own frequency while it runs.
pub const APERF: u32 = 0xe8;

/// CPUID leaf 6, EAX: HWP.
pub const LEAF6_HWP: u32 = 1 << 7;
/// CPUID leaf 6, EAX: HWP's energy preference.
pub const LEAF6_EPP: u32 = 1 << 10;
/// CPUID leaf 6, ECX: APERF and MPERF.
pub const LEAF6_APERF_MPERF: u32 = 1 << 0;
/// CPUID leaf 8000_0008h, EBX: CPPC through registers.
pub const EXT8_CPPC: u32 = 1 << 27;

/// The highest level of AMD's CPUs that rank their cores, as Linux has
/// it: the level the highest frequency is worked out from, since the
/// capability register gives each core's rank instead.
pub const RANKED_HIGHEST: u8 = 166;
/// The same for AMD's family 19h models 70h to 7Fh (Phoenix).
pub const RANKED_HIGHEST_PHOENIX: u8 = 196;

/// The model-specific registers of the running CPU.
pub trait Msrs {
    /// Reads model-specific register `msr`.
    fn read(&mut self, msr: u32) -> u64;
    /// Writes it.
    fn write(&mut self, msr: u32, value: u64);
}

/// What CPUID says, for [`Driver::find`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Cpuid {
    /// The vendor is Intel.
    pub intel: bool,
    /// The vendor is AMD.
    pub amd: bool,
    /// Leaf 6's EAX.
    pub leaf6_eax: u32,
    /// Leaf 8000_0008h's EBX.
    pub ext8_ebx: u32,
}

/// The control a CPU has of its speed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    /// Intel's HWP, with or without the energy preference.
    Hwp {
        /// The request carries an energy preference.
        epp: bool,
    },
    /// AMD's CPPC through registers, whose request always carries one.
    Cppc,
}

impl Driver {
    /// The control CPUID says the CPU has, if any.
    pub fn find(cpuid: &Cpuid) -> Option<Driver> {
        if cpuid.intel && cpuid.leaf6_eax & LEAF6_HWP != 0 {
            Some(Driver::Hwp { epp: cpuid.leaf6_eax & LEAF6_EPP != 0 })
        } else if cpuid.amd && cpuid.ext8_ebx & EXT8_CPPC != 0 {
            Some(Driver::Cppc)
        } else {
            None
        }
    }

    /// The name of Linux's driver for it, which `scaling_driver` gives.
    pub fn linux_name(self) -> &'static str {
        match self {
            Driver::Hwp { .. } => "intel_pstate",
            Driver::Cppc => "amd-pstate-epp",
        }
    }

    /// Whether a request carries an energy preference.
    pub fn has_preference(self) -> bool {
        !matches!(self, Driver::Hwp { epp: false })
    }

    /// Turns the control on for the running CPU, and reads its levels and
    /// the request the firmware left.
    pub fn start(self, msrs: &mut impl Msrs) -> (Levels, Request) {
        match self {
            Driver::Hwp { .. } => msrs.write(PM_ENABLE, 1),
            Driver::Cppc => msrs.write(CPPC_ENABLE, 1),
        }
        (self.levels(msrs), self.request(msrs))
    }

    /// The running CPU's levels.
    pub fn levels(self, msrs: &mut impl Msrs) -> Levels {
        match self {
            Driver::Hwp { .. } => {
                let [highest, nominal, efficient, lowest, ..] =
                    msrs.read(HWP_CAPABILITIES).to_le_bytes();
                Levels { lowest, efficient, nominal, highest }
            }
            Driver::Cppc => {
                let [lowest, efficient, nominal, highest, ..] =
                    msrs.read(CPPC_CAPABILITY).to_le_bytes();
                Levels { lowest, efficient, nominal, highest }
            }
        }
    }

    /// What the running CPU is asked for.
    pub fn request(self, msrs: &mut impl Msrs) -> Request {
        match self {
            Driver::Hwp { .. } => {
                let [min, max, desired, preference, ..] = msrs.read(HWP_REQUEST).to_le_bytes();
                Request { min, max, desired, preference }
            }
            Driver::Cppc => {
                let [max, min, desired, preference, ..] = msrs.read(CPPC_REQUEST).to_le_bytes();
                Request { min, max, desired, preference }
            }
        }
    }

    /// Asks the running CPU for `request`. The register's other fields
    /// stay as they are, and so does the preference where the request
    /// has none.
    pub fn set_request(self, msrs: &mut impl Msrs, request: Request) {
        let Request { min, max, desired, preference } = request;
        let (msr, fields) = match self {
            Driver::Hwp { .. } => (HWP_REQUEST, [min, max, desired, preference]),
            Driver::Cppc => (CPPC_REQUEST, [max, min, desired, preference]),
        };
        let kept = if self.has_preference() { !0xffff_ffff } else { !0x00ff_ffff };
        let value = msrs.read(msr) & kept | u64::from(u32::from_le_bytes(fields)) & !kept;
        msrs.write(msr, value);
    }
}

impl fmt::Display for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Driver::Hwp { .. } => "Intel's HWP",
            Driver::Cppc => "AMD's CPPC",
        })
    }
}

/// A CPU's performance levels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Levels {
    /// The slowest.
    pub lowest: u8,
    /// The one that does the most work for its energy (AMD's lowest
    /// nonlinear level).
    pub efficient: u8,
    /// The one it can keep up (Intel's guaranteed level), at the base
    /// frequency.
    pub nominal: u8,
    /// The fastest, for a while, boosted.
    pub highest: u8,
}

impl Levels {
    /// Whether the levels make sense: firmware that gives nonsense keeps
    /// its own control of the CPU's speed.
    pub fn valid(self) -> bool {
        0 < self.lowest && self.lowest <= self.nominal && self.nominal <= self.highest
    }
}

/// What a CPU is asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Request {
    /// The lowest level it may pick.
    pub min: u8,
    /// The highest.
    pub max: u8,
    /// The level wanted, or 0 for the CPU's own choice.
    pub desired: u8,
    /// The energy preference.
    pub preference: u8,
}

impl Request {
    /// The request at boot, as Linux's: every level from the lowest to the
    /// highest, at the CPU's choice, with `preference` if given, else the
    /// firmware's.
    pub fn at_boot(levels: Levels, firmware: Request, preference: Option<u8>) -> Request {
        Request {
            min: levels.lowest,
            max: levels.highest,
            desired: 0,
            preference: preference.unwrap_or(firmware.preference),
        }
    }

    /// The request with its highest level at most `level`, but not below
    /// its lowest.
    pub fn capped(self, level: u8) -> Request {
        Request { max: self.max.min(level).max(self.min), ..self }
    }
}

/// The energy preferences Linux names, from all for performance to all
/// for saving power.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preference {
    /// 0.
    Performance,
    /// 128, the usual default.
    BalancePerformance,
    /// 192 on Intel, 191 on AMD.
    BalancePower,
    /// 255.
    Power,
}

impl Preference {
    /// Every one, in Linux's order.
    pub const ALL: [Preference; 4] = [
        Preference::Performance,
        Preference::BalancePerformance,
        Preference::BalancePower,
        Preference::Power,
    ];

    /// Its name, Linux's.
    pub fn name(self) -> &'static str {
        match self {
            Preference::Performance => "performance",
            Preference::BalancePerformance => "balance_performance",
            Preference::BalancePower => "balance_power",
            Preference::Power => "power",
        }
    }

    /// The preference named `name`.
    pub fn parse(name: &str) -> Option<Preference> {
        Preference::ALL.into_iter().find(|preference| preference.name() == name)
    }

    /// Its value for `driver`'s requests, Linux's.
    pub fn value(self, driver: Driver) -> u8 {
        match (self, driver) {
            (Preference::Performance, _) => 0,
            (Preference::BalancePerformance, _) => 0x80,
            (Preference::BalancePower, Driver::Hwp { .. }) => 0xc0,
            (Preference::BalancePower, Driver::Cppc) => 0xbf,
            (Preference::Power, _) => 0xff,
        }
    }

    /// The preference whose value for `driver` is `value`, if one is.
    pub fn of(driver: Driver, value: u8) -> Option<Preference> {
        Preference::ALL.into_iter().find(|preference| preference.value(driver) == value)
    }
}

/// Kilohertz per level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    numerator: u64,
    denominator: u64,
}

impl Scale {
    /// Intel's levels: 100 MHz apart, as Linux has them, but on a hybrid
    /// CPU, whose levels are worked out from its base frequency (CPUID
    /// leaf 16h's, `hybrid_base_mhz`) at the nominal level.
    pub fn intel(levels: Levels, hybrid_base_mhz: Option<u32>) -> Scale {
        match hybrid_base_mhz {
            Some(mhz) if mhz > 0 && levels.nominal > 0 => {
                Scale { numerator: u64::from(mhz) * 1000, denominator: u64::from(levels.nominal) }
            }
            _ => Scale { numerator: 100_000, denominator: 1 },
        }
    }

    /// AMD's levels: `nominal_khz`, the TSC's frequency (the base one, at
    /// the nominal level), over the nominal level.
    pub fn amd(levels: Levels, nominal_khz: u64) -> Scale {
        Scale { numerator: nominal_khz, denominator: u64::from(levels.nominal.max(1)) }
    }

    /// The frequency of `level`, in kilohertz.
    pub fn khz(self, level: u8) -> u64 {
        u64::from(level) * self.numerator / self.denominator
    }
}

/// What each step of passive cooling takes off the highest level, in
/// percent, as Linux's processor cooling does with frequencies (ADR
/// 0203).
const COOLING_STEP_PERCENT: u32 = 20;

/// The highest level a CPU may be asked for at passive cooling's `step`:
/// a fifth of its highest level off a step, but never below its lowest.
pub fn cooled(levels: Levels, step: u8) -> u8 {
    let percent = 100u32.saturating_sub(COOLING_STEP_PERCENT * u32::from(step));
    let level = u32::from(levels.highest) * percent / 100;
    u8::try_from(level).unwrap_or(u8::MAX).max(levels.lowest)
}

/// The level AMD's highest frequency is worked out from, as Linux has it:
/// the highest level, but on CPUs that rank their cores (whose highest
/// levels differ, `ranked`), whose highest level is a rank.
pub fn amd_highest(levels: Levels, ranked: bool, family: u32, model: u32) -> u8 {
    if !ranked {
        levels.highest
    } else if family == 0x19 && (0x70..=0x7f).contains(&model) {
        RANKED_HIGHEST_PHOENIX
    } else {
        RANKED_HIGHEST
    }
}

/// The frequency a CPU ran at while APERF counted `aperf` and MPERF
/// `mperf`, in kilohertz: `reference_khz` (MPERF's, the TSC's) times
/// their ratio. `None` if MPERF did not count, the CPU having slept
/// throughout.
pub fn running_khz(reference_khz: u64, aperf: u64, mperf: u64) -> Option<u64> {
    if mperf == 0 {
        return None;
    }
    u64::try_from(u128::from(reference_khz) * u128::from(aperf) / u128::from(mperf)).ok()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// A CPU's registers.
    #[derive(Default)]
    struct Cpu {
        registers: HashMap<u32, u64>,
        writes: Vec<(u32, u64)>,
    }

    impl Msrs for Cpu {
        fn read(&mut self, msr: u32) -> u64 {
            *self.registers.get(&msr).unwrap_or_else(|| panic!("read of MSR {msr:#x}"))
        }

        fn write(&mut self, msr: u32, value: u64) {
            self.registers.insert(msr, value);
            self.writes.push((msr, value));
        }
    }

    fn cpu(registers: &[(u32, u64)]) -> Cpu {
        Cpu { registers: registers.iter().copied().collect(), writes: Vec::new() }
    }

    /// A register from its fields' bytes, the lowest first.
    fn fields(bytes: &[u8]) -> u64 {
        bytes.iter().rev().fold(0, |value, &byte| value << 8 | u64::from(byte))
    }

    #[test]
    fn cpuid_says_which_control_a_cpu_has() {
        let intel = Cpuid { intel: true, leaf6_eax: LEAF6_HWP | LEAF6_EPP, ..Cpuid::default() };
        assert_eq!(Driver::find(&intel), Some(Driver::Hwp { epp: true }));
        let old = Cpuid { leaf6_eax: LEAF6_HWP, ..intel };
        assert_eq!(Driver::find(&old), Some(Driver::Hwp { epp: false }));
        assert_eq!(Driver::find(&Cpuid { leaf6_eax: LEAF6_EPP, ..intel }), None);

        let amd = Cpuid { amd: true, ext8_ebx: EXT8_CPPC, ..Cpuid::default() };
        assert_eq!(Driver::find(&amd), Some(Driver::Cppc));
        assert_eq!(Driver::find(&Cpuid { ext8_ebx: 0, ..amd }), None);
        // Each vendor's bit counts only on its own CPUs.
        assert_eq!(
            Driver::find(&Cpuid { amd: true, leaf6_eax: LEAF6_HWP, ..Cpuid::default() }),
            None
        );
        assert_eq!(
            Driver::find(&Cpuid { intel: true, ext8_ebx: EXT8_CPPC, ..Cpuid::default() }),
            None
        );

        assert_eq!(Driver::Hwp { epp: true }.linux_name(), "intel_pstate");
        assert_eq!(Driver::Cppc.linux_name(), "amd-pstate-epp");
        assert_eq!(Driver::Cppc.to_string(), "AMD's CPPC");
    }

    #[test]
    fn hwp_is_turned_on_and_asked_for_every_level() {
        let driver = Driver::Hwp { epp: true };
        // Lowest 4, most efficient 8, guaranteed 18, highest 34; the
        // firmware asked for 8 to 18 with an activity window, at 0x80.
        let caps = fields(&[34, 18, 8, 4]);
        let firmware = fields(&[8, 18, 0, 0x80, 1]);
        let mut cpu = cpu(&[(HWP_CAPABILITIES, caps), (HWP_REQUEST, firmware)]);

        let (levels, request) = driver.start(&mut cpu);
        assert_eq!(cpu.writes, [(PM_ENABLE, 1)]);
        assert_eq!(levels, Levels { lowest: 4, efficient: 8, nominal: 18, highest: 34 });
        assert!(levels.valid());
        assert_eq!(request, Request { min: 8, max: 18, desired: 0, preference: 0x80 });

        let boot = Request::at_boot(levels, request, None);
        assert_eq!(boot, Request { min: 4, max: 34, desired: 0, preference: 0x80 });
        driver.set_request(&mut cpu, boot);
        assert_eq!(cpu.registers[&HWP_REQUEST], fields(&[4, 34, 0, 0x80, 1]));

        let power = Request::at_boot(levels, request, Some(Preference::Power.value(driver)));
        driver.set_request(&mut cpu, power.capped(20));
        assert_eq!(cpu.registers[&HWP_REQUEST], fields(&[4, 20, 0, 0xff, 1]));
        assert_eq!(
            driver.request(&mut cpu),
            Request { min: 4, max: 20, desired: 0, preference: 0xff }
        );
        // A cap never goes below the lowest level asked for.
        assert_eq!(power.capped(1).max, 4);
    }

    #[test]
    fn hwp_without_a_preference_keeps_that_field() {
        let driver = Driver::Hwp { epp: false };
        assert!(!driver.has_preference());
        let mut cpu = cpu(&[(HWP_REQUEST, 0x0000_1004)]);
        driver.set_request(&mut cpu, Request { min: 4, max: 34, desired: 0, preference: 0x80 });
        assert_eq!(cpu.registers[&HWP_REQUEST], 0x0000_2204);
    }

    #[test]
    fn cppc_is_turned_on_and_asked_for_every_level() {
        let driver = Driver::Cppc;
        // Lowest 18, lowest nonlinear 50, nominal 120, highest 196.
        let caps = fields(&[18, 50, 120, 196]);
        // The firmware's request, maximum first: 196, 18, desired 0, 0x80.
        let firmware = fields(&[196, 18, 0, 0x80]);
        let mut cpu = cpu(&[(CPPC_CAPABILITY, caps), (CPPC_REQUEST, firmware)]);

        let (levels, request) = driver.start(&mut cpu);
        assert_eq!(cpu.writes, [(CPPC_ENABLE, 1)]);
        assert_eq!(levels, Levels { lowest: 18, efficient: 50, nominal: 120, highest: 196 });
        assert_eq!(request, Request { min: 18, max: 196, desired: 0, preference: 0x80 });

        let saving =
            Request::at_boot(levels, request, Some(Preference::BalancePower.value(driver)));
        driver.set_request(&mut cpu, saving.capped(150));
        assert_eq!(cpu.registers[&CPPC_REQUEST], fields(&[150, 18, 0, 0xbf]));
    }

    #[test]
    fn nonsense_levels_are_not_used() {
        assert!(!Levels::default().valid());
        assert!(!Levels { lowest: 40, efficient: 40, nominal: 20, highest: 30 }.valid());
        assert!(!Levels { lowest: 4, efficient: 8, nominal: 40, highest: 34 }.valid());
        assert!(Levels { lowest: 4, efficient: 4, nominal: 4, highest: 4 }.valid());
    }

    #[test]
    fn preferences_have_linux_names_and_values() {
        let names: Vec<_> = Preference::ALL.into_iter().map(Preference::name).collect();
        assert_eq!(names, ["performance", "balance_performance", "balance_power", "power"]);
        assert_eq!(Preference::parse("balance_power"), Some(Preference::BalancePower));
        assert_eq!(Preference::parse("default"), None);

        let intel = Driver::Hwp { epp: true };
        let values: Vec<_> = Preference::ALL.into_iter().map(|p| p.value(intel)).collect();
        assert_eq!(values, [0, 0x80, 0xc0, 0xff]);
        assert_eq!(Preference::BalancePower.value(Driver::Cppc), 0xbf);
        assert_eq!(Preference::of(Driver::Cppc, 0xbf), Some(Preference::BalancePower));
        assert_eq!(Preference::of(intel, 0xbf), None);
        assert_eq!(Preference::of(intel, 0x80), Some(Preference::BalancePerformance));
    }

    #[test]
    fn levels_become_frequencies() {
        let kaby_lake = Levels { lowest: 4, efficient: 8, nominal: 18, highest: 34 };
        let scale = Scale::intel(kaby_lake, None);
        assert_eq!((scale.khz(4), scale.khz(34)), (400_000, 3_400_000));

        // A hybrid CPU's performance core: base 2300 MHz at level 29.
        let hybrid = Levels { lowest: 1, efficient: 10, nominal: 29, highest: 61 };
        let scale = Scale::intel(hybrid, Some(2300));
        assert_eq!(scale.khz(29), 2_300_000);
        assert_eq!(scale.khz(61), 4_837_931);

        let zen = Levels { lowest: 18, efficient: 50, nominal: 120, highest: 255 };
        let scale = Scale::amd(zen, 3_000_000);
        assert_eq!((scale.khz(18), scale.khz(120)), (450_000, 3_000_000));
        // The ranked cores' highest level is a rank: Linux's level stands in.
        assert_eq!(amd_highest(zen, false, 0x1a, 0x70), 255);
        assert_eq!(amd_highest(zen, true, 0x1a, 0x70), 166);
        assert_eq!(amd_highest(zen, true, 0x19, 0x74), 196);
        assert_eq!(scale.khz(amd_highest(zen, true, 0x1a, 0x70)), 4_150_000);
        // A nominal level of 0 divides by 1.
        assert_eq!(Scale::amd(Levels::default(), 3_000_000).khz(1), 3_000_000);
    }

    #[test]
    fn cooling_takes_a_fifth_of_the_highest_level_off_a_step() {
        let levels = Levels { lowest: 4, efficient: 8, nominal: 18, highest: 34 };
        let steps: Vec<_> = (0..=3).map(|step| cooled(levels, step)).collect();
        assert_eq!(steps, [34, 27, 20, 13]);
        // Never below the lowest level.
        assert_eq!(cooled(Levels { lowest: 20, ..levels }, 3), 20);
        assert_eq!(cooled(levels, 9), 4);
    }

    #[test]
    fn the_running_frequency_comes_from_aperf_and_mperf() {
        assert_eq!(running_khz(1_800_000, 150, 100), Some(2_700_000));
        assert_eq!(running_khz(1_800_000, 50, 100), Some(900_000));
        assert_eq!(running_khz(1_800_000, 1, 0), None);
        assert_eq!(running_khz(4_000_000, u64::MAX, u64::MAX / 2), Some(8_000_000));
    }
}
