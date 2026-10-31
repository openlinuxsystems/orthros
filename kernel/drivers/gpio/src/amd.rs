// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! AMD's GPIO controller, in the FCH of Zen machines (ACPI `AMDI0030`,
//! `AMD0030` and `AMDI0031`), at the registers its `_CRS` gives, usually
//! 0xfed81500 (AMD's Processor Programming References, "GPIO"; Linux's
//! `pinctrl-amd` as the reference for what it is used for).
//!
//! Pin `n`'s register is at `4 * n`, for pins 0 to 183; the place of
//! pin 63 holds the master register instead, whose bit 29 ends an
//! interrupt. In a pin's register:
//!
//! - bit 8 makes it level-triggered, rather than edge-triggered, and
//!   bits 10:9 pick the level or the edge (0 high or rising, 1 low or
//!   falling, 2 both edges);
//! - bit 11 lets it interrupt and bit 12 lets the interrupt through (its
//!   mask), bits 15:13 let it wake the machine;
//! - bit 16 is its input's level;
//! - bits 28 and 29 say an interrupt or a wake is pending, and are
//!   cleared by writing them as 1.
//!
//! The summary registers at 0x2f8 and 0x2fc have a bit for each four
//! pins with an interrupt pending, 46 bits in all. Writing a pin's
//! register back as it was read would clear its pending status, so the
//! driver writes those two bits as 0 except to clear them.

use radix_hw::Mmio;

use crate::{Error, Trigger, Unusable};

/// The pins there may be.
pub const PINS: u16 = 184;
/// The pin whose place the master register takes.
const MASTER_PIN: u16 = 63;
/// How many bytes of registers the controller needs.
const SIZE: usize = 0x300;

const MASTER: usize = 0xfc;
const END_OF_INTERRUPT: u32 = 1 << 29;
const SUMMARY: [usize; 2] = [0x2f8, 0x2fc];

const LEVEL_TRIGGERED: u32 = 1 << 8;
const ACTIVE_SHIFT: u32 = 9;
const ACTIVE_MASK: u32 = 3 << ACTIVE_SHIFT;
const INTERRUPT_ENABLE: u32 = 1 << 11;
const INTERRUPT_MASK: u32 = 1 << 12;
const WAKE: u32 = 7 << 13;
const PIN_STATUS: u32 = 1 << 16;
const INTERRUPT_STATUS: u32 = 1 << 28;
const WAKE_STATUS: u32 = 1 << 29;
const PENDING: u32 = INTERRUPT_STATUS | WAKE_STATUS;

/// AMD's GPIO controller.
pub struct Amd {
    registers: Mmio,
}

impl Amd {
    /// The controller at `registers`, if they look like its.
    pub(crate) fn new(registers: Mmio) -> Result<Amd, Unusable> {
        if registers.len() < SIZE {
            return Err(Unusable::Registers);
        }
        // Pin 0's register: a controller always has it, and its bits 31:30
        // and 27:26 are reserved.
        let first = registers.read32(0);
        if first == u32::MAX {
            return Err(Unusable::Absent);
        }
        Ok(Amd { registers })
    }

    fn offset(pin: u16) -> Result<usize, Error> {
        if pin >= PINS || pin == MASTER_PIN {
            return Err(Error::NoPin);
        }
        Ok(usize::from(pin) * 4)
    }

    /// Changes `pin`'s register by `change`, leaving its pending bits.
    fn update(&self, offset: usize, change: impl FnOnce(u32) -> u32) {
        let value = self.registers.read32(offset);
        self.registers.write32(offset, change(value) & !PENDING);
    }

    pub(crate) fn quiet(&self) {
        for pin in (0..PINS).filter(|&pin| pin != MASTER_PIN) {
            let offset = usize::from(pin) * 4;
            let value = self.registers.read32(offset);
            if value & (INTERRUPT_ENABLE | INTERRUPT_MASK | WAKE | PENDING) != 0 {
                self.registers.write32(offset, value & !(INTERRUPT_ENABLE | INTERRUPT_MASK | WAKE));
            }
        }
        self.finish();
    }

    pub(crate) fn set_interrupt(&self, pin: u16, trigger: Trigger) -> Result<(), Error> {
        let offset = Amd::offset(pin)?;
        let (level, active) = match trigger {
            Trigger::RisingEdge => (0, 0),
            Trigger::FallingEdge => (0, 1),
            Trigger::BothEdges => (0, 2),
            Trigger::LevelHigh => (LEVEL_TRIGGERED, 0),
            Trigger::LevelLow => (LEVEL_TRIGGERED, 1),
        };
        self.update(offset, |value| {
            let value = value & !(LEVEL_TRIGGERED | ACTIVE_MASK | INTERRUPT_MASK);
            value | level | (active << ACTIVE_SHIFT) | INTERRUPT_ENABLE
        });
        // An edge seen before the pin was set up is not this one's.
        let value = self.registers.read32(offset);
        if value & PENDING != 0 {
            self.registers.write32(offset, value);
        }
        Ok(())
    }

    pub(crate) fn set_masked(&self, pin: u16, masked: bool) {
        let Ok(offset) = Amd::offset(pin) else { return };
        self.update(
            offset,
            |value| {
                if masked { value & !INTERRUPT_MASK } else { value | INTERRUPT_MASK }
            },
        );
    }

    pub(crate) fn pending(&self, found: &mut dyn FnMut(u16)) {
        let summary = u64::from(self.registers.read32(SUMMARY[0]))
            | u64::from(self.registers.read32(SUMMARY[1])) << 32;
        let groups = summary & ((1 << (PINS / 4)) - 1);
        for group in (0..PINS / 4).filter(|group| groups & (1 << group) != 0) {
            for pin in (group * 4..group * 4 + 4).filter(|&pin| pin != MASTER_PIN) {
                let offset = usize::from(pin) * 4;
                // A masked pin keeps what it has pending until unmasked.
                let value = self.registers.read32(offset);
                if value & PENDING == 0 || value & INTERRUPT_MASK == 0 {
                    continue;
                }
                // Cleared before the pin's device is served, so that a
                // level that is still active interrupts again.
                self.registers.write32(offset, value);
                found(pin);
            }
        }
    }

    pub(crate) fn finish(&self) {
        let value = self.registers.read32(MASTER);
        self.registers.write32(MASTER, value | END_OF_INTERRUPT);
    }

    pub(crate) fn level(&self, pin: u16) -> Result<bool, Error> {
        Ok(self.registers.read32(Amd::offset(pin)?) & PIN_STATUS != 0)
    }
}
