// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The chipset's hardware watchdog (Phase 11, ADR 0143): QEMU's i6300esb,
//! or the TCO timer of an Intel ICH9 or ICH10.
//!
//! Each counts down in two stages and restarts the machine when the second
//! runs out; a pet starts the first again, and each stage is half the
//! timeout. [`find`] looks on bus 0 of PCI segment 0, where chipsets put
//! both:
//!
//! - The 6300ESB's watchdog function (8086:25ab; the Intel 6300ESB I/O
//!   Controller Hub datasheet gives its registers) counts in its 1 kHz mode
//!   (the 33 MHz PCI clock over 2^15) from a 20-bit preload value for each
//!   stage. Its preload and reload registers take a write only after the
//!   unlock sequence, 0x80 then 0x86 written to the reload register.
//!   WDT_TIMEOUT there says it restarted the machine; it stays set across
//!   the restart until written back.
//! - The TCO timer of the LPC bridge at 00:1f.0 (the Intel I/O Controller
//!   Hub 9 Family datasheet) sits at the bridge's ACPI base + 0x60. It
//!   counts 0.6 s ticks from TCO_TMR (10 bits, at least 4) and restarts
//!   the machine if it runs out with TIMEOUT still set from the first
//!   time; a pet reloads it and clears TIMEOUT. NO_REBOOT, in the General
//!   Control and Status register of the root complex registers, must be
//!   clear for it to restart the machine. SECOND_TO_STS says it did.
//!
//! A watchdog the firmware locked, or left unable to restart the machine,
//! is not used. The i6300esb is preferred: QEMU has one only where it was
//! asked for, and its q35 machine always has the TCO timer too.

use radix_hal::Watchdog;
use radix_hw::Mmio;
use radix_hw::pci::{self, Address, Bar, Function};
use radix_sync::Once;

use crate::io::{inw, outw};
use crate::{acpi, devices};

/// The 6300ESB's watchdog function: vendor and device.
const ESB: (u16, u16) = (0x8086, 0x25ab);
/// Its configuration register (16 bits).
const ESB_CONFIG: usize = 0x60;
/// WDT_INT_TYPE at 3: no interrupt when the first stage runs out. With
/// WDT_PRE_SEL (bit 2) clear the 1 kHz mode, with WDT_OUTPUT (bit 5)
/// clear the restart.
const ESB_NO_INTERRUPT: u16 = 0b11;
/// Its lock register (8 bits).
const ESB_LOCK: usize = 0x68;
/// WDT_LOCK: the lock register keeps its value until the machine restarts.
const ESB_LOCKED: u8 = 1 << 0;
/// WDT_ENABLE: it counts.
const ESB_ENABLE: u8 = 1 << 1;
/// Its registers, in BAR 0: the two preload values and the reload
/// register.
const ESB_PRELOAD1: usize = 0x00;
const ESB_PRELOAD2: usize = 0x04;
const ESB_RELOAD: usize = 0x0c;
/// WDT_RELOAD: the first stage starts again.
const ESB_KICK: u16 = 1 << 8;
/// WDT_TIMEOUT: it restarted the machine.
const ESB_TIMEOUT: u16 = 1 << 9;
/// Its counts in a second in the 1 kHz mode: 33 MHz over 2^15.
const ESB_HZ: u32 = 1007;
/// The largest preload value.
const ESB_MAX_PRELOAD: u32 = 0xf_ffff;

/// The device IDs of the ICH9's and ICH10's LPC bridges (QEMU's q35 has
/// 2918, an 82801IB's).
const ICH: [u16; 10] =
    [0x2912, 0x2914, 0x2916, 0x2917, 0x2918, 0x2919, 0x3a14, 0x3a16, 0x3a18, 0x3a1a];
/// Where the LPC bridge is.
const LPC: Address = Address { segment: 0, bus: 0, device: 31, function: 0 };
/// The bridge's ACPI base address (bits 7 to 15), its ACPI control
/// register, whose ACPI_EN has it decode that base, and the base of the
/// root complex registers (bits 14 to 31, with bit 0 to say it is set).
const PMBASE: usize = 0x40;
const ACPI_CNTL: usize = 0x44;
const ACPI_EN: u8 = 1 << 7;
const RCBA: usize = 0xf0;
/// The General Control and Status register, in the root complex
/// registers, and its NO_REBOOT bit.
const GCS: u64 = 0x3410;
const NO_REBOOT: u32 = 1 << 5;
/// Where the TCO registers are from the ACPI base, and the ones used: the
/// reload register, the two status registers, the first control register
/// and the timer's value.
const TCO_OFFSET: u16 = 0x60;
const TCO_RLD: u16 = 0x00;
const TCO1_STS: u16 = 0x04;
const TCO2_STS: u16 = 0x06;
const TCO1_CNT: u16 = 0x08;
const TCO_TMR: u16 = 0x12;
/// TCO1_STS: the first stage ran out.
const TIMEOUT: u16 = 1 << 3;
/// TCO2_STS: the second ran out, and the machine restarted.
const SECOND_TO_STS: u16 = 1 << 1;
/// TCO1_CNT: the timer is stopped.
const TMR_HLT: u16 = 1 << 11;
/// TCO_TMR's bits, and the fewest ticks it takes.
const TCO_TICKS: u16 = 0x3ff;
const TCO_MIN_TICKS: u32 = 4;

/// The watchdog found.
enum Device {
    Esb { config: Function, regs: Mmio },
    Tco(Tco),
}

static DEVICE: Once<Device> = Once::new();

/// Finds the watchdog and stops it.
pub fn find() -> Option<Watchdog> {
    let mcfg = acpi::get()?.mcfg.as_ref()?;
    let window = radix_acpi::mcfg_entries(mcfg).find(|w| w.segment == 0 && w.start_bus == 0)?;
    let bus = devices::map(window.base, pci::BUS_SIZE)?;
    let (device, found) = find_esb(&bus).or_else(|| find_tco(&bus))?;
    let _ = DEVICE.set(device);
    Some(found)
}

/// The 6300ESB's watchdog function, if bus 0 has one.
fn find_esb(bus: &Mmio) -> Option<(Device, Watchdog)> {
    let config = (0..32).find_map(|device| {
        let address = Address { segment: 0, bus: 0, device, function: 0 };
        let config = bus.window(address.offset_in_bus(), pci::CONFIG_SIZE)?;
        let ids = (config.read16(0x00), config.read16(0x02));
        (ids == ESB).then(|| Function::probe(address, config)).flatten()
    })?;
    let Some(Bar::Memory { address, size, .. }) = config.bar(0) else { return None };
    let regs = devices::map(address, usize::try_from(size).ok()?.max(16))?;
    if config.read8(ESB_LOCK) & ESB_LOCKED != 0 {
        radix_klog::warn!("Watchdog: the firmware locked the i6300esb; it is not used");
        return None;
    }
    config.set_command(config.command() | pci::command::MEMORY);
    config.write8(ESB_LOCK, 0);
    config.write16(ESB_CONFIG, ESB_NO_INTERRUPT);
    let restarted = regs.read16(ESB_RELOAD) & ESB_TIMEOUT != 0;
    if restarted {
        esb_write(&regs, ESB_RELOAD, u32::from(ESB_TIMEOUT));
    }
    let found = Watchdog {
        name: "the Intel 6300ESB watchdog",
        min_seconds: 1,
        max_seconds: 2 * ESB_MAX_PRELOAD / ESB_HZ,
        restarted,
    };
    Some((Device::Esb { config, regs }, found))
}

/// Writes the i6300esb's register at `offset`, after the unlock sequence.
fn esb_write(regs: &Mmio, offset: usize, value: u32) {
    regs.write16(ESB_RELOAD, 0x80);
    regs.write16(ESB_RELOAD, 0x86);
    if offset == ESB_RELOAD {
        regs.write16(offset, value as u16);
    } else {
        regs.write32(offset, value);
    }
}

/// The TCO timer of an ICH9 or ICH10, if bus 0 has one's LPC bridge.
fn find_tco(bus: &Mmio) -> Option<(Device, Watchdog)> {
    let config = bus.window(LPC.offset_in_bus(), pci::CONFIG_SIZE)?;
    if config.read16(0x00) != 0x8086 || !ICH.contains(&config.read16(0x02)) {
        return None;
    }
    let pmbase = config.read32(PMBASE) & 0xff80;
    let rcba = config.read32(RCBA);
    if config.read8(ACPI_CNTL) & ACPI_EN == 0 || pmbase == 0 || rcba & 1 == 0 {
        radix_klog::warn!("Watchdog: the firmware left the chipset's TCO timer unreachable");
        return None;
    }
    let gcs = devices::map(u64::from(rcba & !0x3fff) + GCS, 4)?;
    let control = gcs.read32(0);
    if control & NO_REBOOT != 0 {
        gcs.write32(0, control & !NO_REBOOT);
        if gcs.read32(0) & NO_REBOOT != 0 {
            radix_klog::warn!("Watchdog: the chipset's TCO timer cannot restart the machine");
            return None;
        }
    }
    let tco = Tco(pmbase as u16 + TCO_OFFSET);
    tco.write(TCO1_CNT, tco.read(TCO1_CNT) | TMR_HLT);
    let restarted = tco.read(TCO2_STS) & SECOND_TO_STS != 0;
    tco.write(TCO2_STS, SECOND_TO_STS);
    tco.write(TCO1_STS, TIMEOUT);
    let found = Watchdog {
        name: "the Intel ICH9 TCO timer",
        min_seconds: tco_seconds(TCO_MIN_TICKS),
        max_seconds: tco_seconds(u32::from(TCO_TICKS)),
        restarted,
    };
    Some((Device::Tco(tco), found))
}

/// The timeout of `ticks` ticks a stage: two stages of 0.6 s ticks.
fn tco_seconds(ticks: u32) -> u32 {
    ticks * 12 / 10
}

/// The TCO registers, in I/O space from this port.
#[derive(Clone, Copy)]
struct Tco(u16);

impl Tco {
    fn read(self, offset: u16) -> u16 {
        // SAFETY: the LPC bridge decodes its ACPI base, where its TCO
        // registers are; reading them has no side effects.
        unsafe { inw(self.0 + offset) }
    }

    fn write(self, offset: u16, value: u16) {
        // SAFETY: as above; writing the TCO registers only reloads, stops
        // or starts the timer, or clears its status bits.
        unsafe { outw(self.0 + offset, value) }
    }
}

/// Starts the watchdog to restart the machine unless petted within
/// `seconds`, and returns the timeout set.
pub fn start(seconds: u32) -> u32 {
    match DEVICE.get() {
        Some(Device::Esb { config, regs }) => {
            let seconds = seconds.clamp(1, 2 * ESB_MAX_PRELOAD / ESB_HZ);
            let preload = (seconds * ESB_HZ / 2).max(1);
            esb_write(regs, ESB_PRELOAD1, preload);
            esb_write(regs, ESB_PRELOAD2, preload);
            esb_write(regs, ESB_RELOAD, u32::from(ESB_KICK));
            config.write8(ESB_LOCK, ESB_ENABLE);
            preload * 2 / ESB_HZ
        }
        Some(Device::Tco(tco)) => {
            let ticks = (seconds * 10 / 12).clamp(TCO_MIN_TICKS, u32::from(TCO_TICKS));
            tco.write(TCO_TMR, (tco.read(TCO_TMR) & !TCO_TICKS) | ticks as u16);
            pet();
            tco.write(TCO1_CNT, tco.read(TCO1_CNT) & !TMR_HLT);
            tco_seconds(ticks)
        }
        None => seconds,
    }
}

/// Pets the watchdog.
pub fn pet() {
    match DEVICE.get() {
        Some(Device::Esb { regs, .. }) => esb_write(regs, ESB_RELOAD, u32::from(ESB_KICK)),
        Some(Device::Tco(tco)) => {
            tco.write(TCO_RLD, 1);
            tco.write(TCO1_STS, TIMEOUT);
        }
        None => {}
    }
}

/// Stops the watchdog.
pub fn stop() {
    match DEVICE.get() {
        Some(Device::Esb { config, .. }) => config.write8(ESB_LOCK, 0),
        Some(Device::Tco(tco)) => tco.write(TCO1_CNT, tco.read(TCO1_CNT) | TMR_HLT),
        None => {}
    }
}
