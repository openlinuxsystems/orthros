// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The watchdogs against a simulated chipset: configuration space with a
//! sideband bridge that reads all ones while hidden, Intel's TCO timer
//! counting its two stages of 0.6 s ticks, AMD's FCH watchdog counting
//! seconds, and the bits that keep each from restarting the machine, set
//! again by each restart as firmware leaves them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::vec::Vec;

use radix_hw::pci::BUS_SIZE;
use radix_hw::{Mmio, Registers};

use crate::{Found, Platform, Unusable, Watchdog, find};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chip {
    /// QEMU's q35: an ICH9 (TCO version 2).
    Ich9,
    /// An 8 Series PCH (version 2).
    LynxPoint,
    /// The Dell's Sunrise Point-LP (version 4).
    SunrisePoint,
    /// A 300 Series PCH (version 6).
    CannonPoint,
    /// A Zen FCH, the Framework's.
    Zen,
    /// An FCH from before Zen, with its power management registers behind
    /// the index and data ports.
    Carrizo,
}

const INTEL_CHIPS: [Chip; 4] = [Chip::Ich9, Chip::LynxPoint, Chip::SunrisePoint, Chip::CannonPoint];
const ALL_CHIPS: [Chip; 6] =
    [Chip::Ich9, Chip::LynxPoint, Chip::SunrisePoint, Chip::CannonPoint, Chip::Zen, Chip::Carrizo];

const LPC: (u8, u8) = (31, 0);
const P2SB: (u8, u8) = (31, 1);
const SMBUS: (u8, u8) = (31, 4);
const FCH_SMBUS: (u8, u8) = (20, 0);

const PMBASE: u16 = 0x1800;
const TCOBASE: u16 = 0x400;
const RCBA: u64 = 0xfed1_c000;
const GCS: u64 = RCBA + 0x3410;
const SBREG: u64 = 0xfd00_0000;
const SMB_GC: u64 = SBREG + (0xc6 << 16) + 0x0c;
const PM_MMIO: u64 = 0xfed8_0300;
const FCH_WDT: u64 = 0xfeb0_0000;
const FCH_WDT_ACPI: u64 = 0xfed8_0b00;

const GCS_NO_REBOOT: u32 = 1 << 5;
const SMB_GC_NO_REBOOT: u32 = 1 << 1;
/// Another bit of the SMBus's General Control register, which must stay.
const SMB_GC_OTHER: u32 = 1 << 0;

const TIMEOUT: u16 = 1 << 3;
const SECOND_TO_STS: u16 = 1 << 1;
const TMR_HLT: u16 = 1 << 11;

const WDT_RUN: u32 = 1 << 0;
const WDT_FIRED: u32 = 1 << 1;
const WDT_POWER_OFF: u32 = 1 << 2;
const WDT_DISABLED: u32 = 1 << 3;
const WDT_TRIGGER: u32 = 1 << 7;

struct State {
    chip: Chip,
    config: HashMap<(u8, u8), Vec<u8>>,
    p2sb_hidden: bool,
    /// The TCO timer: TCO_TMR, the ticks left in this stage, TMR_HLT,
    /// TIMEOUT and SECOND_TO_STS.
    tmr: u16,
    left: u32,
    halted: bool,
    timeout: bool,
    second: bool,
    /// The NO_REBOOT bits: GCS's, the SMBus General Control register's and
    /// TCO1_CNT's.
    gcs: u32,
    smb_gc: u32,
    cnt_no_reboot: bool,
    /// NO_REBOOT is strapped on: it stays set.
    strapped: bool,
    /// The FCH: its power management registers and the index port, the
    /// watchdog's control and count registers and the seconds left.
    pm: [u8; 8],
    pm_index: u8,
    wdt_control: u32,
    wdt_count: u32,
    wdt_left: u32,
    /// The FCH watchdog's disable bits stay set.
    fch_off: bool,
    millis: u64,
    restarts: u32,
    powered_off: bool,
}

/// A configuration space of 4 KiB with these registers set.
fn config(vendor: u16, device: u16, revision: u8, registers: &[(usize, u32)]) -> Vec<u8> {
    let mut bytes = vec![0; 4096];
    bytes[0..2].copy_from_slice(&vendor.to_le_bytes());
    bytes[2..4].copy_from_slice(&device.to_le_bytes());
    bytes[8] = revision;
    for &(offset, value) in registers {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

impl State {
    fn new(chip: Chip) -> State {
        let mut config = HashMap::new();
        config.insert((0, 0), self::config(0x8086, 0x1234, 0, &[]));
        let lpc_v2 = [(0x40, u32::from(PMBASE) | 1), (0x44, 0x80), (0xf0, RCBA as u32 | 1)];
        let smbus = [(0x50, u32::from(TCOBASE) | 1), (0x54, 1 << 8)];
        match chip {
            Chip::Ich9 => {
                config.insert(LPC, self::config(0x8086, 0x2918, 2, &lpc_v2));
            }
            Chip::LynxPoint => {
                config.insert(LPC, self::config(0x8086, 0x8c4e, 5, &lpc_v2));
                config.insert(SMBUS, self::config(0x8086, 0x8c22, 5, &[]));
            }
            Chip::SunrisePoint => {
                config.insert(LPC, self::config(0x8086, 0x9d58, 0x21, &[]));
                config
                    .insert(P2SB, self::config(0x8086, 0x9d20, 0x21, &[(0x10, SBREG as u32 | 4)]));
                config.insert(SMBUS, self::config(0x8086, 0x9d23, 0x21, &smbus));
            }
            Chip::CannonPoint => {
                config.insert(LPC, self::config(0x8086, 0xa30e, 0x10, &[]));
                config
                    .insert(P2SB, self::config(0x8086, 0xa320, 0x10, &[(0x10, SBREG as u32 | 4)]));
                config.insert(SMBUS, self::config(0x8086, 0xa323, 0x10, &smbus));
            }
            Chip::Zen => {
                config.insert(FCH_SMBUS, self::config(0x1022, 0x790b, 0x71, &[]));
            }
            Chip::Carrizo => {
                config.insert(FCH_SMBUS, self::config(0x1022, 0x790b, 0x4b, &[]));
            }
        }
        let mut state = State {
            chip,
            config,
            p2sb_hidden: false,
            tmr: 0x04,
            left: 0,
            halted: false,
            timeout: false,
            second: false,
            gcs: 0,
            smb_gc: SMB_GC_OTHER,
            cnt_no_reboot: false,
            strapped: false,
            // DecodeEn without WatchdogTmrEn; the watchdog off, counting
            // 32 us; the ACPI MMIO block decoded.
            pm: [0, 0, 0, 0b1100, 1 << 1, 0, 0, 0],
            pm_index: 0,
            wdt_control: WDT_POWER_OFF,
            wdt_count: 0,
            wdt_left: 0,
            fch_off: false,
            millis: 0,
            restarts: 0,
            powered_off: false,
        };
        state.firmware();
        state
    }

    /// What firmware leaves at boot: the bridge hidden, NO_REBOOT set, and
    /// the TCO timer stopped.
    fn firmware(&mut self) {
        self.p2sb_hidden = true;
        self.gcs |= GCS_NO_REBOOT;
        self.smb_gc |= SMB_GC_NO_REBOOT;
        self.cnt_no_reboot = true;
        self.halted = true;
    }

    fn intel(&self) -> bool {
        INTEL_CHIPS.contains(&self.chip)
    }

    fn tco_base(&self) -> u16 {
        match self.chip {
            Chip::Ich9 | Chip::LynxPoint => PMBASE + 0x60,
            _ => TCOBASE,
        }
    }

    /// Whether the TCO timer may restart the machine.
    fn may_restart(&self) -> bool {
        match self.chip {
            Chip::Ich9 | Chip::LynxPoint => self.gcs & GCS_NO_REBOOT == 0,
            Chip::SunrisePoint => self.smb_gc & SMB_GC_NO_REBOOT == 0,
            _ => !self.cnt_no_reboot,
        }
    }

    fn restart(&mut self) {
        self.restarts += 1;
        self.timeout = false;
        self.firmware();
    }

    fn tco_read(&self, offset: u16) -> u16 {
        match offset {
            0x00 => self.left as u16,
            0x04 => {
                if self.timeout {
                    TIMEOUT
                } else {
                    0
                }
            }
            0x06 => {
                if self.second {
                    SECOND_TO_STS
                } else {
                    0
                }
            }
            0x08 => {
                let no_reboot = self.chip == Chip::CannonPoint && self.cnt_no_reboot;
                (if self.halted { TMR_HLT } else { 0 }) | u16::from(no_reboot)
            }
            0x12 => self.tmr,
            _ => 0,
        }
    }

    fn tco_write(&mut self, offset: u16, value: u16) {
        match offset {
            0x00 => self.left = u32::from(self.tmr & 0x3ff),
            0x04 if value & TIMEOUT != 0 => self.timeout = false,
            0x06 if value & SECOND_TO_STS != 0 => self.second = false,
            0x08 => {
                self.halted = value & TMR_HLT != 0;
                if self.chip == Chip::CannonPoint {
                    self.cnt_no_reboot = value & 1 != 0 || self.strapped;
                }
            }
            0x12 => self.tmr = value,
            _ => {}
        }
    }

    /// A tick of 0.6 s.
    fn tco_tick(&mut self) {
        if !self.intel() || self.halted {
            return;
        }
        self.left = self.left.saturating_sub(1);
        if self.left > 0 {
            return;
        }
        if !self.timeout {
            self.timeout = true;
        } else if self.may_restart() {
            self.second = true;
            self.restart();
            return;
        }
        self.left = u32::from(self.tmr & 0x3ff);
    }

    fn zen(&self) -> bool {
        self.chip == Chip::Zen
    }

    fn pm_write(&mut self, index: u8, value: u8) {
        let value = if index == 3 && self.fch_off { value | 0b1100 } else { value };
        self.pm[usize::from(index)] = value;
    }

    /// Where the FCH decodes its watchdog.
    fn wdt_decoded(&self, address: u64) -> bool {
        let fch = matches!(self.chip, Chip::Zen | Chip::Carrizo);
        (address == FCH_WDT && self.pm[0] & 0x80 != 0)
            || (address == FCH_WDT_ACPI && self.pm[4] & 0x02 != 0 && fch)
    }

    fn wdt_off(&self) -> bool {
        self.pm[3] & 0b1100 != 0 || (self.zen() && self.pm[0] & 0x80 == 0)
    }

    fn wdt_write(&mut self, offset: u64, value: u32) {
        match offset {
            0 => {
                let fired = self.wdt_control & WDT_FIRED & !value;
                self.wdt_control = fired | (value & (WDT_RUN | WDT_POWER_OFF));
                if value & WDT_TRIGGER != 0 {
                    self.wdt_left = self.wdt_count;
                }
            }
            4 => self.wdt_count = value & 0xffff,
            _ => {}
        }
    }

    /// A second.
    fn fch_tick(&mut self) {
        if self.wdt_control & WDT_RUN == 0 || self.wdt_off() {
            return;
        }
        assert_eq!(self.pm[3] & 0b11, 0b11, "the watchdog counts seconds");
        self.wdt_left = self.wdt_left.saturating_sub(1);
        if self.wdt_left == 0 {
            self.wdt_control = (self.wdt_control | WDT_FIRED) & !WDT_RUN;
            if self.wdt_control & WDT_POWER_OFF != 0 {
                self.powered_off = true;
            } else {
                self.restart();
            }
        }
    }
}

#[derive(Clone)]
struct Sim(Arc<Mutex<State>>);

impl Sim {
    fn new(chip: Chip) -> Sim {
        Sim(Arc::new(Mutex::new(State::new(chip))))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    fn bus(&self) -> Mmio {
        Mmio::fake(Arc::new(Config(self.0.clone())), BUS_SIZE)
    }

    /// Lets `millis` pass.
    fn advance(&self, millis: u64) {
        let mut state = self.state();
        for _ in 0..millis / 100 {
            state.millis += 100;
            if state.millis.is_multiple_of(600) {
                state.tco_tick();
            }
            if state.millis.is_multiple_of(1000) {
                state.fch_tick();
            }
        }
    }

    fn restarts(&self) -> u32 {
        self.state().restarts
    }

    /// Finds the watchdog, as the kernel does at boot.
    fn find(&self) -> (Option<(Watchdog<Sim>, Found)>, Vec<Unusable>) {
        let mut unusable = Vec::new();
        let found = find(&self.bus(), self.clone(), |why| unusable.push(why));
        (found, unusable)
    }

    fn found(&self) -> (Watchdog<Sim>, Found) {
        let (found, unusable) = self.find();
        assert_eq!(unusable, []);
        found.expect("a watchdog")
    }
}

/// Configuration space of bus 0.
struct Config(Arc<Mutex<State>>);

fn split(offset: usize) -> ((u8, u8), usize) {
    (((offset >> 15) as u8, ((offset >> 12) & 7) as u8), offset & 0xfff)
}

impl Registers for Config {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let state = self.0.lock().unwrap();
        let (function, register) = split(offset);
        let ones = u64::MAX >> (64 - 8 * size);
        if function == P2SB && state.p2sb_hidden {
            return ones;
        }
        state.config.get(&function).map_or(ones, |bytes| {
            let mut value = [0; 8];
            value[..size].copy_from_slice(&bytes[register..register + size]);
            u64::from_le_bytes(value)
        })
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut state = self.0.lock().unwrap();
        let (function, register) = split(offset);
        if function == P2SB && (register..register + size).contains(&0xe1) {
            state.p2sb_hidden = (value >> (8 * (0xe1 - register))) & 1 != 0;
            return;
        }
        if function == P2SB && state.p2sb_hidden {
            return;
        }
        if let Some(bytes) = state.config.get_mut(&function) {
            bytes[register..register + size].copy_from_slice(&value.to_le_bytes()[..size]);
        }
    }
}

/// Physical memory: the chipsets' registers, and all ones elsewhere.
struct Physical(Arc<Mutex<State>>);

impl Registers for Physical {
    fn read(&self, address: usize, size: usize) -> u64 {
        let state = self.0.lock().unwrap();
        let address = address as u64;
        let ones = u64::MAX >> (64 - 8 * size);
        match address {
            GCS if matches!(state.chip, Chip::Ich9 | Chip::LynxPoint) => u64::from(state.gcs),
            SMB_GC if state.chip == Chip::SunrisePoint => u64::from(state.smb_gc),
            PM_MMIO..0xfed8_0308 if state.zen() => {
                u64::from(state.pm[(address - PM_MMIO) as usize])
            }
            _ => {
                let base = address & !0x7;
                if !state.wdt_decoded(base) {
                    return ones;
                }
                match address - base {
                    0 => u64::from(
                        state.wdt_control | if state.wdt_off() { WDT_DISABLED } else { 0 },
                    ),
                    4 => u64::from(state.wdt_left),
                    _ => ones,
                }
            }
        }
    }

    fn write(&self, address: usize, _size: usize, value: u64) {
        let mut state = self.0.lock().unwrap();
        let address = address as u64;
        match address {
            GCS if matches!(state.chip, Chip::Ich9 | Chip::LynxPoint) => {
                state.gcs = value as u32 | if state.strapped { GCS_NO_REBOOT } else { 0 };
            }
            SMB_GC if state.chip == Chip::SunrisePoint => {
                state.smb_gc = value as u32 | if state.strapped { SMB_GC_NO_REBOOT } else { 0 };
            }
            PM_MMIO..0xfed8_0308 if state.zen() => {
                state.pm_write((address - PM_MMIO) as u8, value as u8);
            }
            _ => {
                let base = address & !0x7;
                if state.wdt_decoded(base) {
                    state.wdt_write(address - base, value as u32);
                }
            }
        }
    }
}

impl Platform for Sim {
    fn map(&self, phys: u64, len: usize) -> Option<Mmio> {
        Mmio::fake(Arc::new(Physical(self.0.clone())), usize::MAX).window(phys as usize, len)
    }

    fn in8(&self, port: u16) -> u8 {
        let state = self.state();
        match port {
            // Zen's FCH may not answer at the ports.
            0xcd7 if state.chip == Chip::Carrizo => state.pm[usize::from(state.pm_index)],
            _ => 0xff,
        }
    }

    fn out8(&self, port: u16, value: u8) {
        let mut state = self.state();
        match port {
            0xcd6 if state.chip == Chip::Carrizo => state.pm_index = value,
            0xcd7 if state.chip == Chip::Carrizo => {
                let index = state.pm_index;
                state.pm_write(index, value);
            }
            _ => {}
        }
    }

    fn in16(&self, port: u16) -> u16 {
        let state = self.state();
        let base = state.tco_base();
        if state.intel() && (base..base + 0x20).contains(&port) {
            state.tco_read(port - base)
        } else {
            0xffff
        }
    }

    fn out16(&self, port: u16, value: u16) {
        let mut state = self.state();
        let base = state.tco_base();
        if state.intel() && (base..base + 0x20).contains(&port) {
            state.tco_write(port - base, value);
        }
    }
}

#[test]
fn each_chipset_has_its_watchdog() {
    let names = [
        (Chip::Ich9, "the Intel ICH9 TCO timer", 4, 1227),
        (Chip::LynxPoint, "the Intel PCH TCO timer", 4, 1227),
        (Chip::SunrisePoint, "the Intel PCH TCO timer", 4, 1227),
        (Chip::CannonPoint, "the Intel PCH TCO timer", 4, 1227),
        (Chip::Zen, "the AMD FCH watchdog", 1, 65535),
        (Chip::Carrizo, "the AMD FCH watchdog", 1, 65535),
    ];
    for (chip, name, min_seconds, max_seconds) in names {
        let sim = Sim::new(chip);
        let (_, found) = sim.found();
        assert_eq!(found, Found { name, min_seconds, max_seconds, restarted: false }, "{chip:?}");
    }
}

#[test]
fn unpetted_it_restarts_the_machine_at_its_timeout_and_the_next_boot_knows() {
    for chip in ALL_CHIPS {
        let sim = Sim::new(chip);
        let (watchdog, _) = sim.found();
        assert_eq!(watchdog.start(60), 60, "{chip:?}");
        sim.advance(59_400);
        assert_eq!(sim.restarts(), 0, "{chip:?} restarted early");
        sim.advance(600);
        assert_eq!(sim.restarts(), 1, "{chip:?} did not restart");
        assert!(!sim.state().powered_off);

        let (watchdog, found) = sim.found();
        assert!(found.restarted, "{chip:?} does not say it restarted the machine");
        let (_, found) = sim.found();
        assert!(!found.restarted, "{chip:?} still says so a boot later");
        // And it works again after the restart.
        watchdog.start(10);
        sim.advance(10_000);
        assert_eq!(sim.restarts(), 2, "{chip:?}");
    }
}

#[test]
fn petted_it_never_restarts_the_machine_and_stopped_it_stays_quiet() {
    for chip in ALL_CHIPS {
        let sim = Sim::new(chip);
        let (watchdog, _) = sim.found();
        watchdog.start(60);
        for _ in 0..20 {
            // Into the TCO timer's second stage, each time.
            sim.advance(45_000);
            watchdog.pet();
        }
        assert_eq!(sim.restarts(), 0, "{chip:?}");
        watchdog.stop();
        sim.advance(3_600_000);
        assert_eq!(sim.restarts(), 0, "{chip:?} restarted while stopped");
        watchdog.start(30);
        sim.advance(30_000);
        assert_eq!(sim.restarts(), 1, "{chip:?} did not start again");
    }
}

#[test]
fn timeouts_are_clamped_to_what_each_takes() {
    for chip in ALL_CHIPS {
        let sim = Sim::new(chip);
        let (watchdog, found) = sim.found();
        assert_eq!(watchdog.start(0), found.min_seconds, "{chip:?}");
        assert_eq!(watchdog.start(u32::MAX / 16), found.max_seconds, "{chip:?}");
        watchdog.stop();
    }
    // The TCO timer's ticks are 0.6 s, two stages to a timeout.
    let sim = Sim::new(Chip::SunrisePoint);
    let (watchdog, _) = sim.found();
    assert_eq!(watchdog.start(61), 60);
    assert_eq!(sim.state().tmr & 0x3ff, 50);
}

#[test]
fn it_is_found_stopped() {
    for chip in ALL_CHIPS {
        let sim = Sim::new(chip);
        {
            // Firmware that left it running.
            let mut state = sim.state();
            state.halted = false;
            state.left = 100;
            state.tmr = 100;
            state.wdt_control |= WDT_RUN;
            state.wdt_count = 100;
            state.wdt_left = 100;
        }
        let _ = sim.found();
        sim.advance(1_000_000);
        assert_eq!(sim.restarts(), 0, "{chip:?}");
    }
}

#[test]
fn sunrise_point_clears_no_reboot_behind_the_hidden_sideband_bridge() {
    let sim = Sim::new(Chip::SunrisePoint);
    let _ = sim.found();
    let state = sim.state();
    assert_eq!(state.smb_gc, SMB_GC_OTHER, "NO_REBOOT clear, the rest kept");
    assert!(state.p2sb_hidden, "the bridge is hidden again");
    drop(state);

    // A bridge the firmware left shown stays shown.
    let sim = Sim::new(Chip::SunrisePoint);
    sim.state().p2sb_hidden = false;
    let _ = sim.found();
    assert!(!sim.state().p2sb_hidden);
}

#[test]
fn cannon_point_clears_no_reboot_in_tco1_cnt() {
    let sim = Sim::new(Chip::CannonPoint);
    let _ = sim.found();
    assert!(!sim.state().cnt_no_reboot);
    assert!(sim.state().p2sb_hidden, "the sideband bridge is left alone");
}

#[test]
fn a_tco_timer_that_cannot_restart_the_machine_is_not_used() {
    for chip in INTEL_CHIPS {
        let sim = Sim::new(chip);
        sim.state().strapped = true;
        let (found, unusable) = sim.find();
        assert!(found.is_none(), "{chip:?}");
        assert_eq!(unusable, [Unusable::TcoNoReboot], "{chip:?}");
    }
}

#[test]
fn a_tco_timer_the_firmware_left_undecoded_is_not_used() {
    let sim = Sim::new(Chip::SunrisePoint);
    sim.state().config.get_mut(&SMBUS).unwrap()[0x55] = 0;
    assert!(matches!(sim.find(), (None, unusable) if unusable == [Unusable::TcoUnreachable]));

    let sim = Sim::new(Chip::Ich9);
    sim.state().config.get_mut(&LPC).unwrap()[0x44] = 0;
    assert!(matches!(sim.find(), (None, unusable) if unusable == [Unusable::TcoUnreachable]));

    // Or the sideband bridge, so NO_REBOOT cannot be cleared.
    let sim = Sim::new(Chip::SunrisePoint);
    sim.state().config.remove(&P2SB);
    assert!(matches!(sim.find(), (None, unusable) if unusable == [Unusable::TcoUnreachable]));
}

#[test]
fn zen_turns_the_fch_watchdog_on_to_restart_the_machine_each_second() {
    let sim = Sim::new(Chip::Zen);
    sim.state().wdt_control |= WDT_FIRED;
    let (watchdog, found) = sim.found();
    assert!(found.restarted);
    let state = sim.state();
    assert_eq!(state.pm[0] & 0x80, 0x80, "WatchdogTmrEn set");
    assert_eq!(state.pm[3] & 0b1111, 0b0011, "on, counting seconds");
    assert_eq!(state.wdt_control & (WDT_FIRED | WDT_POWER_OFF | WDT_RUN), 0);
    drop(state);
    watchdog.start(5);
    assert_eq!(sim.state().wdt_left, 5);
}

#[test]
fn an_fch_before_zen_is_reached_through_the_ports_and_the_acpi_block() {
    let sim = Sim::new(Chip::Carrizo);
    let (watchdog, _) = sim.found();
    assert_eq!(sim.state().pm[0] & 0x80, 0, "WatchdogTmrEn left alone");
    watchdog.start(20);
    sim.advance(20_000);
    assert_eq!(sim.restarts(), 1);

    // Decoded nowhere, it is not used.
    let sim = Sim::new(Chip::Carrizo);
    sim.state().pm[4] = 0;
    assert!(matches!(sim.find(), (None, unusable) if unusable == [Unusable::FchUnreachable]));
}

#[test]
fn an_fch_watchdog_the_firmware_turned_off_is_not_used() {
    for chip in [Chip::Zen, Chip::Carrizo] {
        let sim = Sim::new(chip);
        sim.state().fch_off = true;
        let (found, unusable) = sim.find();
        assert!(found.is_none(), "{chip:?}");
        assert_eq!(unusable, [Unusable::FchDisabled], "{chip:?}");
    }
}

#[test]
fn other_chipsets_have_none() {
    // A Sunrise Point whose SMBus controller is not one the kernel knows.
    let sim = Sim::new(Chip::SunrisePoint);
    sim.state().config.get_mut(&SMBUS).unwrap()[2] = 0x00;
    assert!(matches!(sim.find(), (None, unusable) if unusable.is_empty()));
    // An older FCH.
    let sim = Sim::new(Chip::Carrizo);
    sim.state().config.get_mut(&FCH_SMBUS).unwrap()[8] = 0x42;
    assert!(matches!(sim.find(), (None, unusable) if unusable.is_empty()));
}
