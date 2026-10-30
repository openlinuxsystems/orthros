// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The chipset's hardware watchdog (Phase 11, ADRs 0143 and 0167): QEMU's
//! i6300esb, the TCO timer of Intel's chipsets from the ICH9 on, or the
//! watchdog of AMD's Fusion Controller Hub (FCH).
//!
//! Each restarts the machine unless it is petted in time. [`find`] looks
//! on bus 0 of PCI segment 0, where chipsets put them:
//!
//! - The 6300ESB's watchdog function (8086:25ab; the Intel 6300ESB I/O
//!   Controller Hub datasheet gives its registers) counts in its 1 kHz mode
//!   (the 33 MHz PCI clock over 2^15) from a 20-bit preload value for each
//!   of two stages. Its preload and reload registers take a write only
//!   after the unlock sequence, 0x80 then 0x86 written to the reload
//!   register. WDT_TIMEOUT there says it restarted the machine; it stays
//!   set across the restart until written back.
//! - Intel's TCO timer counts 0.6 s ticks from TCO_TMR (10 bits, at least
//!   4) and restarts the machine if it runs out with TIMEOUT still set from
//!   the first time, so each of its two stages is half the timeout; a pet
//!   reloads it and clears TIMEOUT. SECOND_TO_STS says it restarted the
//!   machine. Its NO_REBOOT bit must be clear for it to do so. Where the
//!   registers and that bit are changed with the chipsets (Intel numbers
//!   the layouts as TCO versions):
//!   - The ICH9 and ICH10 and the 5 to 9 Series and C600 and C610 PCHs
//!     (version 2): the registers are at the LPC bridge's (00:1f.0) ACPI
//!     base + 0x60, and NO_REBOOT is in the General Control and Status
//!     register of its root complex registers (the ICH9 datasheet).
//!   - The 100 and 200 Series and C620 PCHs (version 4): the SMBus
//!     controller (00:1f.4) decodes the registers at its TCOBASE once
//!     TCOCTL's TCO_BASE_EN is set, and NO_REBOOT is bit 1 of the SMBus's
//!     General Control register, offset 0xc of its port (0xc6) in the
//!     private configuration space the sideband bridge (P2SB, 00:1f.1)
//!     decodes at its SBREG_BAR. Firmware hides the bridge: its
//!     configuration space reads all ones until bit 0 of its byte at 0xe1
//!     is cleared. (FreeBSD's ichwd(4) gives these.)
//!   - The 300 Series PCHs and later (version 6): at the SMBus
//!     controller's TCOBASE too, with NO_REBOOT bit 0 of TCO1_CNT (Intel's
//!     patch for Cannon Lake's TCO timer in Linux says so).
//! - The FCH's watchdog (in AMD's FCHs whose SMBus function, at 00:14.0,
//!   is 1022:780b from revision 0x41 or 1022:790b from 0x49; FreeBSD's
//!   amdsbwd(4) gives the registers) counts down once from its 16-bit
//!   WatchDogCount, at the resolution its power management registers set
//!   (1 s here), and restarts the machine when it reaches 0, unless told
//!   to power it off instead. A pet (WatchDogTrigger) starts the count
//!   again, and WatchDogFired says it ran out. The power management
//!   registers say where it is decoded: at 0xfeb00000 if
//!   WatchdogTmrEn is set in DecodeEn, which on Zen also turns the
//!   watchdog on, else in the ACPI MMIO block at 0xfed80b00. Zen's FCHs
//!   (790b from revision 0x51) put the power management registers in that
//!   block too, at 0xfed80300, since they may not answer at the old index
//!   and data ports 0xcd6 and 0xcd7 (AMD's patch to Linux's sp5100_tco
//!   says so).
//!
//! A watchdog the firmware locked, or left unable to restart the machine,
//! is not used. The i6300esb is preferred: QEMU has one only where it was
//! asked for, and its q35 machine always has the TCO timer too.
//!
//! The crate has no `unsafe` code. The kernel gives it bus 0's
//! configuration space, and maps device memory and reaches I/O ports for
//! it ([`Platform`]). Its tests run it against a simulated chipset.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use core::fmt;

use radix_hw::Mmio;
use radix_hw::pci::{self, Address, Bar, Function};

/// What the watchdog needs from the kernel besides configuration space.
pub trait Platform {
    /// Maps the `len` bytes of device memory at `phys`; `None` if they
    /// cannot be, or are RAM.
    fn map(&self, phys: u64, len: usize) -> Option<Mmio>;
    /// Reads the byte at I/O port `port`.
    fn in8(&self, port: u16) -> u8;
    /// Writes the byte at I/O port `port`.
    fn out8(&self, port: u16, value: u8);
    /// Reads the 16-bit word at I/O port `port`.
    fn in16(&self, port: u16) -> u16;
    /// Writes the 16-bit word at I/O port `port`.
    fn out16(&self, port: u16, value: u16);
}

/// What [`find`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    /// What it is, for the log and `/kernel/class/watchdog`.
    pub name: &'static str,
    /// The shortest timeout it takes, in seconds.
    pub min_seconds: u32,
    /// The longest.
    pub max_seconds: u32,
    /// Whether it restarted the machine: the boot before this one ended
    /// with it running out.
    pub restarted: bool,
}

/// Why a watchdog that is there is not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unusable {
    /// The firmware locked the i6300esb.
    EsbLocked,
    /// The firmware left the TCO timer's registers, or the bit that keeps
    /// it from restarting the machine, out of reach.
    TcoUnreachable,
    /// NO_REBOOT stays set: the TCO timer cannot restart the machine.
    TcoNoReboot,
    /// The FCH decodes its watchdog nowhere.
    FchUnreachable,
    /// The firmware turned the FCH's watchdog off.
    FchDisabled,
}

impl fmt::Display for Unusable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Unusable::EsbLocked => "the firmware locked the i6300esb",
            Unusable::TcoUnreachable => "the firmware left the chipset's TCO timer unreachable",
            Unusable::TcoNoReboot => "the chipset's TCO timer cannot restart the machine",
            Unusable::FchUnreachable => "the firmware left the AMD FCH watchdog unreachable",
            Unusable::FchDisabled => "the firmware turned the AMD FCH watchdog off",
        })
    }
}

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

const INTEL: u16 = 0x8086;
const AMD: u16 = 0x1022;

/// The LPC bridges whose TCO timer is at their ACPI base (version 2), by
/// ranges of device IDs, with the name the kernel gives it. QEMU's q35
/// has an 82801IB's (2918).
const LPC_BRIDGES: [(u16, u16, &str); 16] = [
    (0x2912, 0x2912, "the Intel ICH9 TCO timer"),
    (0x2914, 0x2914, "the Intel ICH9 TCO timer"),
    (0x2916, 0x2919, "the Intel ICH9 TCO timer"),
    (0x3a14, 0x3a14, "the Intel ICH10 TCO timer"),
    (0x3a16, 0x3a16, "the Intel ICH10 TCO timer"),
    (0x3a18, 0x3a18, "the Intel ICH10 TCO timer"),
    (0x3a1a, 0x3a1a, "the Intel ICH10 TCO timer"),
    // The 5 Series and 3400, 6 Series and C200, C600, and 7 Series.
    (0x3b00, 0x3b1f, "the Intel PCH TCO timer"),
    (0x1c40, 0x1c5f, "the Intel PCH TCO timer"),
    (0x1d40, 0x1d41, "the Intel PCH TCO timer"),
    (0x1e41, 0x1e5f, "the Intel PCH TCO timer"),
    // The 8 Series and C220, its mobile parts, the 9 Series and its
    // mobile parts, and the C610.
    (0x8c40, 0x8c5f, "the Intel PCH TCO timer"),
    (0x9c40, 0x9c47, "the Intel PCH TCO timer"),
    (0x8cc1, 0x8cc6, "the Intel PCH TCO timer"),
    (0x9cc1, 0x9cc9, "the Intel PCH TCO timer"),
    (0x8d40, 0x8d4f, "the Intel PCH TCO timer"),
];

/// Where the LPC bridge is.
const LPC: (u8, u8) = (31, 0);
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
const GCS_NO_REBOOT: u32 = 1 << 5;
/// Where the TCO registers are from the ACPI base.
const TCO_OFFSET: u16 = 0x60;

/// The SMBus controllers that decode the TCO timer, by device ID: those
/// whose NO_REBOOT is behind the sideband bridge (version 4), with the
/// SMBus's port there, and those with it in TCO1_CNT (version 6).
const SMBUS_SIDEBAND: [(u16, u8); 6] = [
    (0x9d23, 0xc6), // Sunrise Point-LP: the Dell's Kaby Lake
    (0xa123, 0xc6), // 100 Series and C230
    (0xa2a3, 0xc6), // 200 Series
    (0xa1a3, 0xc6), // C620
    (0xa223, 0xc6), // C620's other SMBus
    (0x19df, 0xcf), // Atom C3000
];
const SMBUS_TCO1_CNT: [u16; 24] = [
    0x9da3, // Cannon Point-LP
    0xa323, // 300 Series and C240
    0x02a3, // 400 Series on the package
    0x06a3, // 400 Series
    0xa3a3, // B460 and H410
    0x34a3, // Ice Lake-LP
    0x18df, // Atom P5000
    0x4b23, // Elkhart Lake
    0x4da3, // Jasper Lake
    0xa0a3, // 500 Series on the package
    0x43a3, // 500 Series
    0x7aa3, // 600 Series
    0x51a3, // Alder Lake-P
    0x54a3, // Alder Lake-N
    0x7a23, // 700 Series
    0x7e22, // Meteor Lake-P
    0xae22, // Core Ultra 200 desktop processors
    0x7722, // Core Ultra 200H
    0x7f23, // 800 Series
    0xa822, // Lunar Lake-M
    0xe322, // Core Ultra series 3
    0xe422, // Core Ultra series 3
    0x1bc9, // C740
    0x5796, // Xeon 6
];

/// Where the SMBus controller is.
const SMBUS: (u8, u8) = (31, 4);
/// Its TCO base (bits 5 to 15) and TCO control registers, and the
/// control register's TCO_BASE_EN.
const TCOBASE: usize = 0x50;
const TCOCTL: usize = 0x54;
const TCO_BASE_EN: u32 = 1 << 8;
/// Where the sideband bridge is, its byte with the hide bit, and its
/// 64-bit SBREG_BAR.
const P2SB: (u8, u8) = (31, 1);
const P2SB_HIDE: usize = 0xe1;
const HIDDEN: u8 = 1 << 0;
const SBREG_BAR: usize = 0x10;
/// The SMBus's General Control register in its port, and its NO_REBOOT.
const SMB_GC: u64 = 0x0c;
const SMB_GC_NO_REBOOT: u32 = 1 << 1;

/// The TCO registers used: the reload register, the two status
/// registers, the first control register and the timer's value.
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
/// TCO1_CNT, from version 6: NO_REBOOT.
const CNT_NO_REBOOT: u16 = 1 << 0;
/// TCO_TMR's bits, and the fewest ticks it takes.
const TCO_TICKS: u16 = 0x3ff;
const TCO_MIN_TICKS: u32 = 4;

/// Where the FCH's SMBus function is, and the revisions of each from
/// which the watchdog is as here, and from which Zen's power management
/// registers are in memory.
const FCH_SMBUS: (u8, u8) = (20, 0);
const FCH_HUDSON: (u16, u8) = (0x780b, 0x41);
const FCH_KERNCZ: (u16, u8) = (0x790b, 0x49);
const FCH_ZEN_REVISION: u8 = 0x51;
/// The power management registers: index and data ports, or where Zen
/// has them in memory.
const PM_INDEX: u16 = 0xcd6;
const PM_DATA: u16 = 0xcd7;
const PM_MMIO: u64 = 0xfed8_0300;
/// DecodeEn's first byte, with WatchdogTmrEn.
const PM_DECODE_EN0: u8 = 0x00;
const WDT_TMR_EN: u8 = 1 << 7;
/// DecodeEn's last byte: the watchdog's resolution (3 = 1 s) and its
/// disable bits (clear to have it on).
const PM_DECODE_EN3: u8 = 0x03;
const WDT_RESOLUTION: u8 = 0b11;
const WDT_RESOLUTION_1S: u8 = 0b11;
const WDT_DISABLE: u8 = 0b1100;
/// The ISA control register, with the ACPI MMIO block's decoding.
const PM_ISA_CONTROL: u8 = 0x04;
const MMIO_EN: u8 = 1 << 1;
/// Where the watchdog is decoded with WatchdogTmrEn, and in the ACPI
/// MMIO block.
const FCH_WDT: u64 = 0xfeb0_0000;
const FCH_WDT_ACPI: u64 = 0xfed8_0b00;
/// Its control register, with WatchDogRunStopB (it counts),
/// WatchDogFired (it ran out; written back to clear), WatchDogAction
/// (power off rather than restart), WatchDogDisable and WatchDogTrigger
/// (the count starts again), and its count register.
const WDT_CONTROL: usize = 0x00;
const WDT_RUN: u32 = 1 << 0;
const WDT_FIRED: u32 = 1 << 1;
const WDT_POWER_OFF: u32 = 1 << 2;
const WDT_DISABLED: u32 = 1 << 3;
const WDT_TRIGGER: u32 = 1 << 7;
const WDT_COUNT: usize = 0x04;
const WDT_MAX_COUNT: u32 = 0xffff;

/// A watchdog [`find`] found, stopped.
pub struct Watchdog<P> {
    platform: P,
    kind: Kind,
}

enum Kind {
    Esb {
        config: Function,
        regs: Mmio,
    },
    /// The TCO registers, in I/O space from this port.
    Tco(u16),
    /// The FCH watchdog's registers.
    Fch(Mmio),
}

/// One kind's search: `None` if the chipset has none, else the watchdog
/// or why it is not used.
type Search<P> = fn(&Mmio, &P) -> Option<Result<(Kind, Found), Unusable>>;

/// Finds the watchdog in `bus`, the configuration space of bus 0 of
/// segment 0, and stops it; calls `unusable` with each one there that is
/// not used.
pub fn find<P: Platform>(
    bus: &Mmio,
    platform: P,
    mut unusable: impl FnMut(Unusable),
) -> Option<(Watchdog<P>, Found)> {
    let searches: [Search<P>; 4] = [find_esb, find_lpc_tco, find_smbus_tco, find_fch];
    for search in searches {
        match search(bus, &platform) {
            Some(Ok((kind, found))) => return Some((Watchdog { platform, kind }, found)),
            Some(Err(why)) => unusable(why),
            None => {}
        }
    }
    None
}

/// The configuration space of `device`.`function` on the bus, if the
/// function is there.
fn function(bus: &Mmio, (device, function): (u8, u8)) -> Option<Mmio> {
    let address = Address { segment: 0, bus: 0, device, function };
    let config = bus.window(address.offset_in_bus(), pci::CONFIG_SIZE)?;
    (config.read16(0x00) != 0xffff).then_some(config)
}

/// The 6300ESB's watchdog function, if the bus has one.
fn find_esb<P: Platform>(bus: &Mmio, platform: &P) -> Option<Result<(Kind, Found), Unusable>> {
    let config = (0..32).find_map(|device| {
        let config = function(bus, (device, 0))?;
        let address = Address { segment: 0, bus: 0, device, function: 0 };
        let ids = (config.read16(0x00), config.read16(0x02));
        (ids == ESB).then(|| Function::probe(address, config)).flatten()
    })?;
    let Some(Bar::Memory { address, size, .. }) = config.bar(0) else { return None };
    let regs = platform.map(address, usize::try_from(size).ok()?.max(16))?;
    if config.read8(ESB_LOCK) & ESB_LOCKED != 0 {
        return Some(Err(Unusable::EsbLocked));
    }
    config.set_command(config.command() | pci::command::MEMORY);
    config.write8(ESB_LOCK, 0);
    config.write16(ESB_CONFIG, ESB_NO_INTERRUPT);
    let restarted = regs.read16(ESB_RELOAD) & ESB_TIMEOUT != 0;
    if restarted {
        esb_write(&regs, ESB_RELOAD, u32::from(ESB_TIMEOUT));
    }
    let found = Found {
        name: "the Intel 6300ESB watchdog",
        min_seconds: 1,
        max_seconds: 2 * ESB_MAX_PRELOAD / ESB_HZ,
        restarted,
    };
    Some(Ok((Kind::Esb { config, regs }, found)))
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

/// The TCO timer at the LPC bridge's ACPI base (version 2), if the bus
/// has such a bridge.
fn find_lpc_tco<P: Platform>(bus: &Mmio, platform: &P) -> Option<Result<(Kind, Found), Unusable>> {
    let config = function(bus, LPC)?;
    let device = config.read16(0x02);
    let (_, _, name) = *LPC_BRIDGES
        .iter()
        .find(|&&(first, last, _)| (first..=last).contains(&device))
        .filter(|_| config.read16(0x00) == INTEL)?;
    let pmbase = config.read32(PMBASE) & 0xff80;
    let rcba = config.read32(RCBA);
    if config.read8(ACPI_CNTL) & ACPI_EN == 0 || pmbase == 0 || rcba & 1 == 0 {
        return Some(Err(Unusable::TcoUnreachable));
    }
    let Some(gcs) = platform.map(u64::from(rcba & !0x3fff) + GCS, 4) else {
        return Some(Err(Unusable::TcoUnreachable));
    };
    let base = pmbase as u16 + TCO_OFFSET;
    let restarted = tco_take(platform, base);
    let control = gcs.read32(0);
    if control & GCS_NO_REBOOT != 0 {
        gcs.write32(0, control & !GCS_NO_REBOOT);
        if gcs.read32(0) & GCS_NO_REBOOT != 0 {
            return Some(Err(Unusable::TcoNoReboot));
        }
    }
    Some(Ok((Kind::Tco(base), tco_found(name, restarted))))
}

/// The TCO timer the SMBus controller decodes (versions 4 and 6), if the
/// bus has such a controller.
fn find_smbus_tco<P: Platform>(
    bus: &Mmio,
    platform: &P,
) -> Option<Result<(Kind, Found), Unusable>> {
    let config = function(bus, SMBUS)?;
    if config.read16(0x00) != INTEL {
        return None;
    }
    let device = config.read16(0x02);
    let sideband = SMBUS_SIDEBAND.iter().find(|&&(id, _)| id == device).map(|&(_, port)| port);
    if sideband.is_none() && !SMBUS_TCO1_CNT.contains(&device) {
        return None;
    }
    let base = config.read32(TCOBASE) & 0xffe0;
    if config.read32(TCOCTL) & TCO_BASE_EN == 0 || base == 0 {
        return Some(Err(Unusable::TcoUnreachable));
    }
    let base = base as u16;
    let restarted = tco_take(platform, base);
    let cleared = match sideband {
        Some(port) => {
            let Some(gc) = sideband_bar(bus)
                .and_then(|bar| platform.map(bar + (u64::from(port) << 16) + SMB_GC, 4))
            else {
                return Some(Err(Unusable::TcoUnreachable));
            };
            gc.write32(0, gc.read32(0) & !SMB_GC_NO_REBOOT);
            gc.read32(0) & SMB_GC_NO_REBOOT == 0
        }
        None => {
            tco_write(
                platform,
                base,
                TCO1_CNT,
                tco_read(platform, base, TCO1_CNT) & !CNT_NO_REBOOT,
            );
            tco_read(platform, base, TCO1_CNT) & CNT_NO_REBOOT == 0
        }
    };
    if !cleared {
        return Some(Err(Unusable::TcoNoReboot));
    }
    Some(Ok((Kind::Tco(base), tco_found("the Intel PCH TCO timer", restarted))))
}

/// The sideband bridge's SBREG_BAR: shown for as long as it takes to
/// read it, if the firmware hid it, then hidden again.
fn sideband_bar(bus: &Mmio) -> Option<u64> {
    let address = Address { segment: 0, bus: 0, device: P2SB.0, function: P2SB.1 };
    let config = bus.window(address.offset_in_bus(), pci::CONFIG_SIZE)?;
    let hidden = config.read8(P2SB_HIDE) & HIDDEN != 0;
    if hidden {
        config.write8(P2SB_HIDE, 0);
    }
    let bar = (config.read16(0x00) == INTEL).then(|| {
        let low = u64::from(config.read32(SBREG_BAR));
        let high = u64::from(config.read32(SBREG_BAR + 4));
        (high << 32 | low) & !0xf
    });
    if hidden {
        config.write8(P2SB_HIDE, HIDDEN);
    }
    bar.filter(|&bar| bar != 0)
}

/// Stops the TCO timer at `base`, and clears and returns whether it
/// restarted the machine.
fn tco_take<P: Platform>(platform: &P, base: u16) -> bool {
    tco_write(platform, base, TCO1_CNT, tco_read(platform, base, TCO1_CNT) | TMR_HLT);
    let restarted = tco_read(platform, base, TCO2_STS) & SECOND_TO_STS != 0;
    tco_write(platform, base, TCO2_STS, SECOND_TO_STS);
    tco_write(platform, base, TCO1_STS, TIMEOUT);
    restarted
}

fn tco_found(name: &'static str, restarted: bool) -> Found {
    Found {
        name,
        min_seconds: tco_seconds(TCO_MIN_TICKS),
        max_seconds: tco_seconds(u32::from(TCO_TICKS)),
        restarted,
    }
}

/// The timeout of `ticks` ticks a stage: two stages of 0.6 s ticks.
fn tco_seconds(ticks: u32) -> u32 {
    ticks * 12 / 10
}

fn tco_read<P: Platform>(platform: &P, base: u16, offset: u16) -> u16 {
    platform.in16(base + offset)
}

fn tco_write<P: Platform>(platform: &P, base: u16, offset: u16, value: u16) {
    platform.out16(base + offset, value);
}

/// The FCH's power management registers.
enum Pm<'a, P> {
    Ports(&'a P),
    Mmio(Mmio),
}

impl<P: Platform> Pm<'_, P> {
    fn read(&self, index: u8) -> u8 {
        match self {
            Pm::Ports(platform) => {
                platform.out8(PM_INDEX, index);
                platform.in8(PM_DATA)
            }
            Pm::Mmio(regs) => regs.read8(usize::from(index)),
        }
    }

    fn write(&self, index: u8, value: u8) {
        match self {
            Pm::Ports(platform) => {
                platform.out8(PM_INDEX, index);
                platform.out8(PM_DATA, value);
            }
            Pm::Mmio(regs) => regs.write8(usize::from(index), value),
        }
    }
}

/// The FCH's watchdog, if the bus has an AMD FCH that has it as here.
fn find_fch<P: Platform>(bus: &Mmio, platform: &P) -> Option<Result<(Kind, Found), Unusable>> {
    let config = function(bus, FCH_SMBUS)?;
    let (device, revision) = (config.read16(0x02), config.read8(0x08));
    let known = [FCH_HUDSON, FCH_KERNCZ].iter().any(|&(id, from)| device == id && revision >= from);
    if config.read16(0x00) != AMD || !known {
        return None;
    }
    let zen = device == FCH_KERNCZ.0 && revision >= FCH_ZEN_REVISION;
    let pm = if zen {
        let Some(regs) = platform.map(PM_MMIO, 8) else {
            return Some(Err(Unusable::FchUnreachable));
        };
        Pm::Mmio(regs)
    } else {
        Pm::Ports(platform)
    };
    let mut decode = pm.read(PM_DECODE_EN0);
    if zen && decode & WDT_TMR_EN == 0 {
        pm.write(PM_DECODE_EN0, decode | WDT_TMR_EN);
        decode = pm.read(PM_DECODE_EN0);
    }
    let at = if decode & WDT_TMR_EN != 0 {
        FCH_WDT
    } else if pm.read(PM_ISA_CONTROL) & MMIO_EN != 0 {
        FCH_WDT_ACPI
    } else {
        return Some(Err(Unusable::FchUnreachable));
    };
    let Some(regs) = platform.map(at, 8) else { return Some(Err(Unusable::FchUnreachable)) };
    let control = regs.read32(WDT_CONTROL);
    if control == u32::MAX {
        return Some(Err(Unusable::FchUnreachable));
    }
    // Stopped, set to restart the machine, and WatchDogFired written back.
    regs.write32(WDT_CONTROL, control & !(WDT_RUN | WDT_POWER_OFF | WDT_TRIGGER));
    let restarted = control & WDT_FIRED != 0;
    let resolution = pm.read(PM_DECODE_EN3);
    pm.write(PM_DECODE_EN3, (resolution & !(WDT_RESOLUTION | WDT_DISABLE)) | WDT_RESOLUTION_1S);
    if regs.read32(WDT_CONTROL) & WDT_DISABLED != 0 {
        return Some(Err(Unusable::FchDisabled));
    }
    let found = Found {
        name: "the AMD FCH watchdog",
        min_seconds: 1,
        max_seconds: WDT_MAX_COUNT,
        restarted,
    };
    Some(Ok((Kind::Fch(regs), found)))
}

impl<P: Platform> Watchdog<P> {
    /// Starts it to restart the machine unless petted within `seconds`,
    /// clamped to what it takes, and returns the timeout set.
    pub fn start(&self, seconds: u32) -> u32 {
        match &self.kind {
            Kind::Esb { config, regs } => {
                let seconds = seconds.clamp(1, 2 * ESB_MAX_PRELOAD / ESB_HZ);
                let preload = (seconds * ESB_HZ / 2).max(1);
                esb_write(regs, ESB_PRELOAD1, preload);
                esb_write(regs, ESB_PRELOAD2, preload);
                esb_write(regs, ESB_RELOAD, u32::from(ESB_KICK));
                config.write8(ESB_LOCK, ESB_ENABLE);
                preload * 2 / ESB_HZ
            }
            &Kind::Tco(base) => {
                let ticks = (seconds * 10 / 12).clamp(TCO_MIN_TICKS, u32::from(TCO_TICKS));
                let value = tco_read(&self.platform, base, TCO_TMR) & !TCO_TICKS;
                tco_write(&self.platform, base, TCO_TMR, value | ticks as u16);
                self.pet();
                let control = tco_read(&self.platform, base, TCO1_CNT);
                tco_write(&self.platform, base, TCO1_CNT, control & !TMR_HLT);
                tco_seconds(ticks)
            }
            Kind::Fch(regs) => {
                let count = seconds.clamp(1, WDT_MAX_COUNT);
                regs.write32(WDT_COUNT, count);
                let control = regs.read32(WDT_CONTROL) & !(WDT_FIRED | WDT_TRIGGER);
                regs.write32(WDT_CONTROL, control | WDT_RUN);
                // The trigger only counts as a write of its own.
                regs.write32(WDT_CONTROL, control | WDT_RUN | WDT_TRIGGER);
                count
            }
        }
    }

    /// Pets it.
    pub fn pet(&self) {
        match &self.kind {
            Kind::Esb { regs, .. } => esb_write(regs, ESB_RELOAD, u32::from(ESB_KICK)),
            &Kind::Tco(base) => {
                tco_write(&self.platform, base, TCO_RLD, 1);
                tco_write(&self.platform, base, TCO1_STS, TIMEOUT);
            }
            Kind::Fch(regs) => {
                let control = regs.read32(WDT_CONTROL) & !WDT_FIRED;
                regs.write32(WDT_CONTROL, control | WDT_TRIGGER);
            }
        }
    }

    /// Stops it.
    pub fn stop(&self) {
        match &self.kind {
            Kind::Esb { config, .. } => config.write8(ESB_LOCK, 0),
            &Kind::Tco(base) => {
                let control = tco_read(&self.platform, base, TCO1_CNT);
                tco_write(&self.platform, base, TCO1_CNT, control | TMR_HLT);
            }
            Kind::Fch(regs) => {
                let control = regs.read32(WDT_CONTROL) & !(WDT_FIRED | WDT_TRIGGER);
                regs.write32(WDT_CONTROL, control & !WDT_RUN);
            }
        }
    }
}
