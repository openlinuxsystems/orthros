// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The CPUs' control of their own speed (ADR 0202): CPUID says whether
//! the running CPU has Intel's HWP or AMD's CPPC, and its registers are
//! read and written here, for [`radix_pstate`], which works out their
//! values.

use radix_pstate::{Cpuid, Driver, LEAF6_APERF_MPERF, Levels, Msrs, Request, Scale};

use crate::cpu::{self, cpuid, rdmsr, wrmsr};
use crate::time;

/// CPUID leaf 7, EDX: the CPU has cores of two kinds.
const LEAF7_HYBRID: u32 = 1 << 15;

/// The running CPU's registers of the control [`driver`] found, which
/// is all [`radix_pstate`] reads and writes.
struct Registers;

impl Msrs for Registers {
    fn read(&mut self, msr: u32) -> u64 {
        // SAFETY: a `Registers` is made only once CPUID said that the
        // running CPU has the control whose registers `radix_pstate`
        // reads, which reading does not change.
        unsafe { rdmsr(msr) }
    }

    fn write(&mut self, msr: u32, value: u64) {
        // SAFETY: as in `read`; the values turn the control on or ask the
        // CPU for a speed, which touches no memory.
        unsafe { wrmsr(msr, value) }
    }
}

/// The control CPUID says the running CPU has.
pub fn driver() -> Option<Driver> {
    let max = cpu::max_leaf();
    let leaf6 = (max >= 6).then(|| cpuid(6, 0));
    let ext8_ebx =
        if cpu::max_extended_leaf() >= 0x8000_0008 { cpuid(0x8000_0008, 0).ebx } else { 0 };
    Driver::find(&Cpuid {
        intel: cpu::is_intel(),
        amd: cpu::is_amd(),
        leaf6_eax: leaf6.map_or(0, |leaf| leaf.eax),
        ext8_ebx,
    })
}

/// Turns the running CPU's control on, if it has one; returns its levels
/// and the request the firmware left.
pub fn start() -> Option<(Levels, Request)> {
    Some(driver()?.start(&mut Registers))
}

/// Asks the running CPU for `request`, if it has a control.
pub fn request(request: Request) {
    if let Some(driver) = driver() {
        driver.set_request(&mut Registers, request);
    }
}

/// The running CPU's APERF and MPERF counts, if it has the counters.
pub fn counts() -> Option<(u64, u64)> {
    if cpu::max_leaf() < 6 || cpuid(6, 0).ecx & LEAF6_APERF_MPERF == 0 {
        return None;
    }
    // SAFETY: CPUID says the CPU has both counters, which reading does
    // not change.
    Some(unsafe { (rdmsr(radix_pstate::APERF), rdmsr(radix_pstate::MPERF)) })
}

/// The frequencies of the running CPU's levels, and the level its highest
/// frequency is that of where the CPUs rank their cores.
pub fn scale(levels: Levels) -> Option<(Scale, u8)> {
    match driver()? {
        Driver::Hwp { .. } => {
            let max = cpu::max_leaf();
            let hybrid = max >= 7 && cpuid(7, 0).edx & LEAF7_HYBRID != 0;
            let base_mhz = (hybrid && max >= 0x16).then(|| cpuid(0x16, 0).eax & 0xffff);
            Some((Scale::intel(levels, base_mhz), levels.highest))
        }
        Driver::Cppc => {
            let khz = time::calibration()?.tsc_hz / 1000;
            let (family, model, _) = cpu::family_model_stepping();
            Some((Scale::amd(levels, khz), radix_pstate::amd_highest(levels, true, family, model)))
        }
    }
}
