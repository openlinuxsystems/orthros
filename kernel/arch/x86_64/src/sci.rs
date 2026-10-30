// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ACPI's System Control Interrupt, and the power button behind it (Phase
//! 11, ADR 0142).
//!
//! Where ACPI makes the power button a fixed feature, as QEMU's machines
//! and many PCs do, pressing it sets PWRBTN_STS in a PM1 status register,
//! which raises the SCI while PWRBTN_EN is set in the enable register
//! beside it. [`start`] takes the machine into ACPI
//! mode if the firmware left it out (the firmware then handles the button
//! no more), clears every fixed event and turns all but the button's off,
//! clears and turns off every general-purpose event, as nothing here can
//! run their AML methods until an interpreter does (uACPI, Phase 11c),
//! and routes the SCI to a device vector through the I/O APIC,
//! level-triggered and active low unless the MADT says otherwise.
//!
//! The interrupt handler clears every status bit it finds set, so that the
//! level-triggered SCI falls, turns off any general-purpose event that
//! raised it, and calls the kernel's function for each press. A button the
//! ACPI namespace describes (the FADT's PWR_BUTTON flag), whose presses
//! only AML tells of, and a hardware-reduced machine's, which has no fixed
//! events, are left to the firmware: [`start`] returns `None` for them.

use radix_acpi::RegisterBlock;
use radix_sync::Once;

use crate::power::{self, Control};
use crate::{acpi, apic, devices, smp};

/// PM1 status: the power button was pressed.
const PWRBTN_STS: u16 = 1 << 8;
/// PM1 status: every fixed event's bit (TMR_STS, BM_STS, GBL_STS,
/// PWRBTN_STS, SLPBTN_STS, RTC_STS, PCIEXP_WAKE_STS and WAK_STS); the
/// others are reserved, and written as 0.
const FIXED_STATUS: u16 = 0xc731;
/// PM1 enable: the power button raises the SCI.
const PWRBTN_EN: u16 = 1 << 8;
/// PM1 enable: every fixed event's bit (TMR_EN, GBL_EN, PWRBTN_EN,
/// SLPBTN_EN and RTC_EN); the others are kept as they are.
const FIXED_ENABLE: u16 = 0x0721;

/// A PM1 event block's status and enable registers.
#[derive(Clone, Copy)]
struct Pm1 {
    status: Control,
    enable: Control,
}

/// A general-purpose event block: its status and enable registers, `half`
/// bytes each.
#[derive(Clone, Copy)]
struct Gpe {
    status: Control,
    enable: Control,
    half: usize,
}

/// The registers the handler reads, and the kernel's function.
struct Events {
    pm1: [Option<Pm1>; 2],
    gpe: [Option<Gpe>; 2],
    pressed: fn(),
}

static EVENTS: Once<Events> = Once::new();

impl Pm1 {
    fn find(block: RegisterBlock) -> Option<Self> {
        let half = usize::from(block.length / 2);
        let status = Control::block(block.address, usize::from(block.length))?;
        Some(Self { status, enable: status.offset(half) })
    }
}

impl Gpe {
    fn find(block: RegisterBlock) -> Option<Self> {
        let half = usize::from(block.length / 2);
        let status = Control::block(block.address, usize::from(block.length))?;
        Some(Self { status, enable: status.offset(half), half })
    }

    /// Turns every event off and clears it.
    fn clear(self) {
        for i in 0..self.half {
            self.enable.offset(i).write8(0);
            self.status.offset(i).write8(0xff);
        }
    }

    /// Clears each event that is set, and turns it off: nothing here
    /// handles them yet.
    fn quiet(self) {
        for i in 0..self.half {
            let set = self.status.offset(i).read8();
            if set != 0 {
                let enable = self.enable.offset(i);
                enable.write8(enable.read8() & !set);
                self.status.offset(i).write8(set);
            }
        }
    }
}

/// Hears the machine's power button: `pressed` is called, in an interrupt
/// handler with interrupts masked, each time it is pressed. Returns what
/// the button is, for the log, or `None` (and changes nothing) if the
/// kernel cannot hear it.
pub fn start(pressed: fn()) -> Option<&'static str> {
    let acpi = acpi::get()?;
    let fadt = acpi.fadt?;
    if fadt.hardware_reduced || !fadt.fixed_power_button {
        return None;
    }
    let pm1a = Pm1::find(fadt.pm1a_event?)?;
    let pm1b = fadt.pm1b_event.and_then(Pm1::find);
    let control = Control::find(fadt.pm1a_control?)?;
    let irq = u8::try_from(fadt.sci_interrupt).ok().filter(|&irq| irq < 16)?;
    if !power::enter_acpi_mode(&fadt, control) {
        radix_klog::warn!("Power: the firmware did not hand ACPI's events over");
        return None;
    }
    let gpe = fadt.gpe.map(|block| block.and_then(Gpe::find));
    for block in gpe.iter().flatten() {
        block.clear();
    }
    for pm1 in [Some(pm1a), pm1b].iter().flatten() {
        pm1.enable.write(pm1.enable.read() & !FIXED_ENABLE);
        pm1.status.write(FIXED_STATUS);
    }
    EVENTS.call_once(|| Events { pm1: [Some(pm1a), pm1b], gpe, pressed });
    let Some((vector, _)) = devices::allocate(None, 0, interrupt, 0) else {
        radix_klog::warn!("Power: no interrupt vector is free for ACPI's SCI");
        return None;
    };
    let destination = smp::info(0).map_or(0, |cpu| cpu.hardware_id);
    if apic::route_sci(acpi.madt.as_ref(), irq, vector as u8, destination).is_none() {
        radix_klog::warn!("Power: ACPI's SCI, ISA interrupt {irq}, could not be routed");
        devices::free(vector);
        return None;
    }
    // A machine with two PM1 blocks splits each register between them,
    // and the same value goes to both, as Linux writes them.
    for pm1 in [Some(pm1a), pm1b].iter().flatten() {
        pm1.enable.write(pm1.enable.read() | PWRBTN_EN);
    }
    Some("the ACPI power button")
}

/// The SCI: clears what raised it, and passes a press of the button on.
fn interrupt(_: usize) {
    let Some(events) = EVENTS.get() else { return };
    let mut pressed = false;
    for pm1 in events.pm1.iter().flatten() {
        let set = pm1.status.read() & FIXED_STATUS;
        if set != 0 {
            pm1.status.write(set);
        }
        pressed |= set & PWRBTN_STS != 0;
    }
    for block in events.gpe.iter().flatten() {
        block.quiet();
    }
    if pressed {
        (events.pressed)();
    }
}
