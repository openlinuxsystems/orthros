// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The CPUs' temperature sensors (ADR 0205), through `radix-sensors`:
//! AMD's, which the root complex on bus 0 reaches over the SMN
//! (`k10temp`), and Intel's digital thermal sensors, which each CPU reads
//! in its own MSRs (`coretemp`).

use radix_hw::pci;
use radix_sensors::coretemp::{self, Sample};
use radix_sensors::k10temp::{self, K10temp, Sensor};
use radix_sync::{Once, SpinLock};

use crate::cpu::{self, cpuid, rdmsr_safe};
use crate::{acpi, devices};

/// AMD's sensors, if found; the lock keeps the SMN's index and data
/// registers to one reader.
static CHIPSET: Once<Option<SpinLock<K10temp>>> = Once::new();

/// AMD's sensors, found at the first call.
fn chipset() -> Option<&'static SpinLock<K10temp>> {
    CHIPSET
        .call_once(|| {
            if !cpu::is_amd() {
                return None;
            }
            let mcfg = acpi::get()?.mcfg.as_ref()?;
            let window =
                radix_acpi::mcfg_entries(mcfg).find(|w| w.segment == 0 && w.start_bus == 0)?;
            let bus = devices::map(window.base, pci::BUS_SIZE)?;
            let (family, model, _) = cpu::family_model_stepping();
            k10temp::find(&bus, family, model).map(SpinLock::new)
        })
        .as_ref()
}

/// Calls `found` with each of AMD's sensors.
pub fn chipset_sensors(found: &mut dyn FnMut(Sensor)) {
    if let Some(sensors) = chipset() {
        sensors.lock().sensors().for_each(found);
    }
}

/// Reads one of AMD's sensors.
pub fn read_chipset(sensor: Sensor) -> Option<i32> {
    chipset()?.lock().read(sensor)
}

/// The running CPU's registers, read only where they may not exist.
struct Registers;

impl coretemp::Msrs for Registers {
    fn read(&mut self, msr: u32) -> Option<u64> {
        // SAFETY: `coretemp` reads only its thermal status and target
        // registers, which reading does not change; one the CPU lacks
        // faults, which `rdmsr_safe` turns into `None`.
        unsafe { rdmsr_safe(msr) }
    }
}

/// What the running CPU's digital thermal sensors say, on Intel's.
pub fn core_sample() -> Option<Sample> {
    if !cpu::is_intel() || cpu::max_leaf() < 6 {
        return None;
    }
    coretemp::sample(cpuid(6, 0).eax, &mut Registers)
}
