// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The hardware monitors (ADR 0205): the temperature sensors that
//! Linux's hwmon class shows, for `/kernel/class/hwmon`, numbered in this
//! order:
//!
//! - `acpitz`, the firmware's thermal zones (ADR 0203): a temperature
//!   for each, with its critical trip point;
//! - `k10temp`, the sensors of AMD's processors (Tctl, and each core
//!   complex die's where the model has them);
//! - `coretemp`, one for each of Intel's packages: its package's sensor,
//!   then each core's, numbered as Linux numbers them (`temp1` for the
//!   package, `temp<core + 2>` for each core).
//!
//! [`find`] looks for them once the CPUs run. Intel's sensors are read on
//! each CPU, which a reader has all of them do at most once a second, as
//! Linux's `coretemp` does.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU64, Ordering};

use radix_hal::{Cpu, CpuLocal, MAX_CPUS, Smp, Timer};
use radix_klog::{TextBuf, info};
use radix_sensors::coretemp::{Reading, Sample};
use radix_sensors::k10temp::{MAX_DIES, Sensor};
use radix_sync::{Mutex, Once, SpinLock};

use crate::acpi::thermal;
use crate::{Arch, work};

/// How long a reading of Intel's sensors lasts, as Linux's.
const SAMPLE_LIFE_NS: u64 = 1_000_000_000;

/// AMD's sensors.
static CHIPSET: Once<Chipset> = Once::new();
/// Intel's sensors: the CPUs that read them.
static CORES: Once<Cores> = Once::new();
/// What each CPU's sensors last said.
static SAMPLES: [SpinLock<Option<Sample>>; MAX_CPUS] = [const { SpinLock::new(None) }; MAX_CPUS];
/// When they last did; 0 before.
static SAMPLED_NS: AtomicU64 = AtomicU64::new(0);
/// One reader has the CPUs read their sensors at a time.
static SAMPLING: Mutex<()> = Mutex::new(());

/// AMD's sensors, as the chipset reaches them.
struct Chipset {
    sensors: [Sensor; 1 + MAX_DIES],
    count: usize,
}

/// One of Intel's core sensors, and the CPU that reads it: the first
/// thread of its core.
#[derive(Clone, Copy, Default)]
struct Core {
    cpu: usize,
    package: u32,
    core: u32,
    /// The CPU reads its package's sensor too: the first of its package
    /// with one.
    reads_package: bool,
}

struct Cores {
    cores: [Core; MAX_CPUS],
    count: usize,
}

impl Cores {
    fn iter(&self) -> impl Iterator<Item = Core> + '_ {
        self.cores[..self.count].iter().copied()
    }
}

/// One `hwmon<n>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    /// `acpitz`.
    Zones,
    /// `k10temp`.
    Chipset,
    /// `coretemp`, of this package.
    Package(u32),
}

impl Device {
    /// Its `name`.
    pub fn name(self) -> &'static str {
        match self {
            Device::Zones => "acpitz",
            Device::Chipset => "k10temp",
            Device::Package(_) => "coretemp",
        }
    }
}

/// One of a device's temperatures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// A thermal zone, by its index.
    Zone(usize),
    /// One of AMD's.
    Chipset(Sensor),
    /// An Intel package's, read by `cpu`.
    Package { cpu: usize, package: u32 },
    /// An Intel core's, read by `cpu`.
    Core { cpu: usize, core: u32 },
}

/// A channel's file: `temp<n>_<suffix>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum File {
    Input,
    Label,
    Crit,
    Max,
    CritAlarm,
}

impl File {
    /// What follows `temp<n>_`.
    pub fn suffix(self) -> &'static str {
        match self {
            File::Input => "input",
            File::Label => "label",
            File::Crit => "crit",
            File::Max => "max",
            File::CritAlarm => "crit_alarm",
        }
    }
}

impl Channel {
    /// Its number, the `<n>` of `temp<n>_input`.
    pub fn number(self) -> u32 {
        match self {
            Channel::Zone(index) => index as u32 + 1,
            Channel::Chipset(sensor) => u32::from(sensor.number()),
            Channel::Package { .. } => 1,
            Channel::Core { core, .. } => core + 2,
        }
    }

    /// Its files, as Linux's drivers have them.
    pub fn files(self) -> &'static [File] {
        match self {
            Channel::Zone(index) => {
                let critical =
                    thermal::zones().nth(index).is_some_and(|z| z.trips().critical.is_some());
                if critical { &[File::Input, File::Crit] } else { &[File::Input] }
            }
            Channel::Chipset(_) => &[File::Input, File::Label],
            Channel::Package { cpu, .. } | Channel::Core { cpu, .. } => {
                let max = SAMPLES[cpu].lock().is_some_and(|sample| sample.target.max.is_some());
                if max {
                    &[File::Label, File::Input, File::Crit, File::Max, File::CritAlarm]
                } else {
                    &[File::Label, File::Input, File::Crit, File::CritAlarm]
                }
            }
        }
    }
}

/// Finds the sensors, and logs what was found.
pub fn find() {
    let mut chipset = Chipset { sensors: [Sensor::Control; 1 + MAX_DIES], count: 0 };
    Arch::chipset_temperatures(&mut |sensor| {
        if let Some(slot) = chipset.sensors.get_mut(chipset.count) {
            *slot = sensor;
            chipset.count += 1;
        }
    });
    if chipset.count > 0 {
        let mut line = TextBuf::<160>::new();
        for (index, sensor) in chipset.sensors[..chipset.count].iter().enumerate() {
            let _ = match index {
                0 => write!(line, "{}", sensor.label()),
                _ if index + 1 == chipset.count => write!(line, " and {}", sensor.label()),
                _ => write!(line, ", {}", sensor.label()),
            };
        }
        info!("Sensors: AMD's processor sensors (k10temp) read {}", line.as_str());
        let _ = CHIPSET.set(chipset);
    }
    // Only Intel's CPUs have them, all or none.
    if Arch::core_temperature().is_some() {
        sample_all();
    }
    let mut cores = Cores { cores: [Core::default(); MAX_CPUS], count: 0 };
    let mut packages = 0;
    for (cpu, slot) in SAMPLES.iter().enumerate().take(Arch::cpu_count()) {
        let Some(sample) = *slot.lock() else { continue };
        let place = Arch::cpu_place(cpu).unwrap_or_default();
        let listed = &cores.cores[..cores.count];
        if listed.iter().any(|c| (c.package, c.core) == (place.package, place.core)) {
            continue;
        }
        let package_read = listed.iter().any(|c| c.package == place.package && c.reads_package);
        if !listed.iter().any(|c| c.package == place.package) {
            packages += 1;
        }
        cores.cores[cores.count] = Core {
            cpu,
            package: place.package,
            core: place.core,
            reads_package: !package_read && sample.package.is_some(),
        };
        cores.count += 1;
    }
    if cores.count > 0 {
        let tjmax = SAMPLES[cores.cores[0].cpu].lock().map_or(0, |sample| sample.target.tjmax);
        info!(
            "Sensors: Intel's digital thermal sensors (coretemp) read {packages} package{} and {} \
             core{}, up to {} °C",
            if packages == 1 { "" } else { "s" },
            cores.count,
            if cores.count == 1 { "" } else { "s" },
            tjmax / 1000
        );
        let _ = CORES.set(cores);
    }
    if CHIPSET.get().is_none() && CORES.get().is_none() {
        info!("Sensors: the CPUs have no temperature sensor the kernel reads");
    }
}

/// The devices, in the order they are numbered.
pub fn devices(found: &mut dyn FnMut(Device)) {
    if thermal::zones().next().is_some() {
        found(Device::Zones);
    }
    if CHIPSET.get().is_some() {
        found(Device::Chipset);
    }
    let Some(cores) = CORES.get() else { return };
    for (index, core) in cores.iter().enumerate() {
        if !cores.iter().take(index).any(|c| c.package == core.package) {
            found(Device::Package(core.package));
        }
    }
}

/// `device`'s channels.
pub fn channels(device: Device, found: &mut dyn FnMut(Channel)) {
    match device {
        Device::Zones => {
            (0..thermal::zones().count()).for_each(|index| found(Channel::Zone(index)));
        }
        Device::Chipset => {
            let Some(chipset) = CHIPSET.get() else { return };
            chipset.sensors[..chipset.count]
                .iter()
                .for_each(|&sensor| found(Channel::Chipset(sensor)));
        }
        Device::Package(package) => {
            let Some(cores) = CORES.get() else { return };
            if let Some(core) = cores.iter().find(|c| c.package == package && c.reads_package) {
                found(Channel::Package { cpu: core.cpu, package });
            }
            for core in cores.iter().filter(|core| core.package == package) {
                found(Channel::Core { cpu: core.cpu, core: core.core });
            }
        }
    }
}

/// Writes `channel`'s `file`; fails (EIO) for a sensor that gives no
/// reading.
pub fn write_file(channel: Channel, file: File, out: &mut dyn Write) -> fmt::Result {
    match channel {
        Channel::Zone(index) => {
            let zone = thermal::zones().nth(index).ok_or(fmt::Error)?;
            let at = match file {
                File::Crit => zone.trips().critical,
                _ => zone.temperature(),
            };
            writeln!(out, "{}", at.ok_or(fmt::Error)?.millicelsius())
        }
        Channel::Chipset(sensor) => match file {
            File::Label => writeln!(out, "{}", sensor.label()),
            _ => writeln!(out, "{}", Arch::read_chipset_temperature(sensor).ok_or(fmt::Error)?),
        },
        Channel::Package { cpu, package } => {
            if file == File::Label {
                return writeln!(out, "Package id {package}");
            }
            let sample = sample(cpu).ok_or(fmt::Error)?;
            write_reading(sample, sample.package.ok_or(fmt::Error)?, file, out)
        }
        Channel::Core { cpu, core } => {
            if file == File::Label {
                return writeln!(out, "Core {core}");
            }
            let sample = sample(cpu).ok_or(fmt::Error)?;
            write_reading(sample, sample.core, file, out)
        }
    }
}

fn write_reading(sample: Sample, reading: Reading, file: File, out: &mut dyn Write) -> fmt::Result {
    match file {
        File::Input => writeln!(out, "{}", reading.temperature.ok_or(fmt::Error)?),
        File::Crit => writeln!(out, "{}", sample.target.tjmax),
        File::Max => writeln!(out, "{}", sample.target.max.ok_or(fmt::Error)?),
        File::CritAlarm => writeln!(out, "{}", u8::from(reading.alarm)),
        File::Label => Err(fmt::Error),
    }
}

/// What `cpu`'s sensors say, read again if the last reading is a second
/// old.
fn sample(cpu: usize) -> Option<Sample> {
    {
        let _one = SAMPLING.lock();
        let last = SAMPLED_NS.load(Ordering::Relaxed);
        if last == 0 || Arch::now_ns().saturating_sub(last) >= SAMPLE_LIFE_NS {
            sample_all();
        }
    }
    *SAMPLES.get(cpu)?.lock()
}

/// Has every CPU read its sensors.
fn sample_all() {
    Arch::preempt_disable();
    let me = Arch::current_cpu();
    sample_here();
    Arch::preempt_enable();
    work::on_others(me, sample_here);
    SAMPLED_NS.store(Arch::now_ns().max(1), Ordering::Relaxed);
}

/// On each CPU: its sensors are read.
fn sample_here() {
    let cpu = Arch::current_cpu();
    if let Some(slot) = SAMPLES.get(cpu) {
        *slot.lock() = Arch::core_temperature();
    }
}
