// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! GPIO controllers for the Radix kernel (Phase 11e, ADR 0233): AMD's,
//! in the FCH of Zen machines, and that of Intel's Sunrise Point-LP PCH,
//! in the 100 Series and the 6th to 8th generation Core U and Y
//! processors' packages.
//!
//! A GPIO controller has pins that the firmware sets up: some are
//! inputs a device signals on, as a touchpad tells that it has a report.
//! The ACPI namespace names the controller (`AMDI0030`, `INT344B`), its
//! registers and its one interrupt, and each device names the pins it
//! uses (a `GpioInt` resource) and how they signal. The kernel asks the
//! controller to raise its interrupt for such a pin
//! ([`Controller::set_interrupt`], then [`Controller::unmask`]); when it
//! does, [`Controller::pending`] says which pins it was for and clears
//! them, and [`Controller::finish`] lets it raise the next.
//!
//! - **AMD's** has a 32-bit register a pin, four bytes apart: its level,
//!   how it triggers, whether it interrupts, and its interrupt's status.
//!   Two summary registers say which group of four pins has an interrupt,
//!   and an end-of-interrupt bit in a master register re-arms it ([`amd`]).
//! - **Intel's** is split in communities, each with its own registers,
//!   which group 24 pads at a time: a pad's configuration registers say
//!   what it does, and a group's status and enable registers which pads
//!   interrupt ([`intel`]). The firmware may keep a pad, for the ACPI
//!   firmware's own events or for the Management Engine, and may lock its
//!   configuration; such a pad is not used.
//!
//! Level-triggered pins are masked by the caller while the device that
//! signals on them is served, and unmasked after. The crate has no
//! `unsafe` code and does no locking: the kernel maps the registers and
//! serialises the calls. Its tests run it against simulated controllers.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod amd;
pub mod intel;
#[cfg(test)]
mod tests;

use core::fmt;

use radix_hw::Mmio;

/// How a pin signals an interrupt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// While it is high.
    LevelHigh,
    /// While it is low.
    LevelLow,
    /// When it goes high.
    RisingEdge,
    /// When it goes low.
    FallingEdge,
    /// When it changes.
    BothEdges,
}

impl Trigger {
    /// Whether it is level-triggered.
    pub fn is_level(self) -> bool {
        matches!(self, Trigger::LevelHigh | Trigger::LevelLow)
    }
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Trigger::LevelHigh => "level, high",
            Trigger::LevelLow => "level, low",
            Trigger::RisingEdge => "rising edge",
            Trigger::FallingEdge => "falling edge",
            Trigger::BothEdges => "both edges",
        })
    }
}

/// Why a pin cannot interrupt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The controller has no such pin.
    NoPin,
    /// The firmware keeps the pad: for the ACPI firmware's events, or for
    /// the Management Engine.
    Firmware,
    /// The pad is set up for another function than GPIO.
    Function,
    /// Its configuration is locked, otherwise than asked.
    Locked,
    /// The controller cannot trigger so.
    Trigger(Trigger),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoPin => f.write_str("the controller has no such pin"),
            Error::Firmware => f.write_str("the firmware keeps the pin"),
            Error::Function => f.write_str("the pin is set up for another function"),
            Error::Locked => f.write_str("the pin's configuration is locked"),
            Error::Trigger(trigger) => write!(f, "the pin cannot trigger on {trigger}"),
        }
    }
}

/// Why a controller is not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unusable {
    /// Its registers read all ones: there is none at the address.
    Absent,
    /// The namespace gives too few register ranges, or too small ones.
    Registers,
    /// The registers do not look as the driver knows them.
    Layout,
}

impl fmt::Display for Unusable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Unusable::Absent => "its registers read all ones",
            Unusable::Registers => "the firmware gives too few or too small register ranges",
            Unusable::Layout => "its registers are not laid out as the driver knows them",
        })
    }
}

/// A GPIO controller.
pub enum Controller {
    /// AMD's.
    Amd(amd::Amd),
    /// Intel's.
    Intel(intel::Intel),
}

impl Controller {
    /// AMD's, at `registers`: what its `_CRS` gives.
    pub fn amd(registers: Mmio) -> Result<Controller, Unusable> {
        amd::Amd::new(registers).map(Controller::Amd)
    }

    /// Intel Sunrise Point-LP's, its communities' registers in the order
    /// its `_CRS` gives them.
    pub fn sunrise_point_lp(communities: &[Mmio]) -> Result<Controller, Unusable> {
        intel::Intel::new(&intel::SUNRISE_POINT_LP, communities).map(Controller::Intel)
    }

    /// Its name, for the log.
    pub fn name(&self) -> &'static str {
        match self {
            Controller::Amd(_) => "AMD's",
            Controller::Intel(intel) => intel.name(),
        }
    }

    /// How many pins it numbers, from 0; some may be missing.
    pub fn pins(&self) -> u16 {
        match self {
            Controller::Amd(_) => amd::PINS,
            Controller::Intel(intel) => intel.pins(),
        }
    }

    /// Turns every pin's interrupt off and clears what is pending, as the
    /// kernel takes the controller over.
    pub fn quiet(&self) {
        match self {
            Controller::Amd(amd) => amd.quiet(),
            Controller::Intel(intel) => intel.quiet(),
        }
    }

    /// Sets `pin` up as an input that interrupts on `trigger`; it stays
    /// masked until [`Controller::unmask`].
    pub fn set_interrupt(&self, pin: u16, trigger: Trigger) -> Result<(), Error> {
        match self {
            Controller::Amd(amd) => amd.set_interrupt(pin, trigger),
            Controller::Intel(intel) => intel.set_interrupt(pin, trigger),
        }
    }

    /// Lets `pin` interrupt; a level-triggered pin that is still active
    /// interrupts again.
    pub fn unmask(&self, pin: u16) {
        match self {
            Controller::Amd(amd) => amd.set_masked(pin, false),
            Controller::Intel(intel) => intel.set_masked(pin, false),
        }
    }

    /// Stops `pin` interrupting.
    pub fn mask(&self, pin: u16) {
        match self {
            Controller::Amd(amd) => amd.set_masked(pin, true),
            Controller::Intel(intel) => intel.set_masked(pin, true),
        }
    }

    /// Calls `pin` with each unmasked pin whose interrupt is pending,
    /// and clears it. From the controller's interrupt.
    pub fn pending(&self, pin: &mut dyn FnMut(u16)) {
        match self {
            Controller::Amd(amd) => amd.pending(pin),
            Controller::Intel(intel) => intel.pending(pin),
        }
    }

    /// Lets the controller raise its interrupt again, after
    /// [`Controller::pending`].
    pub fn finish(&self) {
        if let Controller::Amd(amd) = self {
            amd.finish();
        }
    }

    /// Whether `pin`'s input is high.
    pub fn level(&self, pin: u16) -> Result<bool, Error> {
        match self {
            Controller::Amd(amd) => amd.level(pin),
            Controller::Intel(intel) => intel.level(pin),
        }
    }
}
