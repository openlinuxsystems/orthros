// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The controllers against simulated chipsets: Intel's SMBus function with
//! its INUSE semaphore, and the FCH's with its four ports, its power
//! management registers in memory or behind the index and data ports, and
//! a microcontroller that may hold the bus. Devices are EEPROMs of 256
//! bytes; a transaction runs when START is written, as QEMU's does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::pci::BUS_SIZE;
use radix_hw::{Mmio, Registers};

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Chip {
    /// QEMU's q35: an ICH9, 8086:2930 at 00:1f.3.
    Ich9,
    /// An FCH of this revision.
    Fch(u8),
}

const BASE_INTEL: u16 = 0x0700;
const BASE_FCH: u16 = 0x0b00;

struct State {
    chip: Chip,
    config: HashMap<(u8, u8), Vec<u8>>,
    pm: [u8; 8],
    pm_index: u8,
    status: u8,
    control: u8,
    command: u8,
    address: u8,
    data: [u8; 2],
    /// The FCH's semaphore: the host owns the bus, the tries the
    /// microcontroller still holds it for, and how often it was given back.
    owned: bool,
    held_for: u32,
    given_back: u32,
    /// The devices, by port and address.
    devices: HashMap<(u8, u8), [u8; 256]>,
}

impl State {
    fn base(&self) -> u16 {
        match self.chip {
            Chip::Ich9 => BASE_INTEL,
            Chip::Fch(_) => u16::from(self.pm[1]) << 8,
        }
    }

    fn port(&self) -> u8 {
        match self.chip {
            Chip::Ich9 => 0,
            Chip::Fch(_) => (self.pm[2] & PORT_MASK) >> 3,
        }
    }

    /// Runs the transaction set up.
    fn run(&mut self) {
        let target = self.address >> 1;
        let read = self.address & 1 != 0;
        let Some(memory) = self.devices.get_mut(&(self.port(), target)) else {
            self.status |= DEV_ERR;
            return;
        };
        let at = usize::from(self.command);
        match (self.control & 0x1c, read) {
            (BYTE_DATA, true) => self.data[0] = memory[at],
            (WORD_DATA, true) => self.data = [memory[at], memory[(at + 1) % 256]],
            (BYTE_DATA, false) => memory[at] = self.data[0],
            _ => {
                self.status |= FAILED;
                return;
            }
        }
        self.status |= INTR;
    }

    fn smbus_read(&mut self, offset: u16) -> u8 {
        match offset {
            HOST_STATUS => {
                let status = self.status;
                // Reading takes Intel's semaphore.
                if self.chip == Chip::Ich9 {
                    self.status |= INUSE;
                }
                status
            }
            HOST_CONTROL => self.control,
            HOST_COMMAND => self.command,
            TARGET_ADDRESS => self.address,
            DATA0 => self.data[0],
            DATA1 => self.data[1],
            FCH_SEMAPHORE if matches!(self.chip, Chip::Fch(_)) => {
                if self.owned {
                    SEMAPHORE_TAKE
                } else {
                    0
                }
            }
            _ => 0xff,
        }
    }

    fn smbus_write(&mut self, offset: u16, value: u8) {
        match offset {
            HOST_STATUS => self.status &= !value,
            HOST_CONTROL => {
                self.control = value & !START;
                if value & START != 0 {
                    self.run();
                }
            }
            HOST_COMMAND => self.command = value,
            TARGET_ADDRESS => self.address = value,
            DATA0 => self.data[0] = value,
            DATA1 => self.data[1] = value,
            FCH_SEMAPHORE if matches!(self.chip, Chip::Fch(_)) => {
                if value & SEMAPHORE_GIVE != 0 {
                    self.owned = false;
                    self.given_back += 1;
                } else if value & SEMAPHORE_TAKE != 0 {
                    if self.held_for > 0 {
                        self.held_for -= 1;
                    } else {
                        self.owned = true;
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
struct Chipset(Arc<Mutex<State>>);

/// Bus 0's configuration space.
struct Config(Arc<Mutex<State>>);

/// The FCH's power management registers in memory.
struct PmRegisters(Arc<Mutex<State>>);

fn split(offset: usize) -> ((u8, u8), usize) {
    let function = offset / pci::CONFIG_SIZE;
    (((function / 8) as u8, (function % 8) as u8), offset % pci::CONFIG_SIZE)
}

impl Registers for Config {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let state = self.0.lock().unwrap();
        let (function, register) = split(offset);
        let ones = u64::MAX >> (64 - 8 * size);
        state.config.get(&function).map_or(ones, |bytes| {
            let mut value = [0; 8];
            value[..size].copy_from_slice(&bytes[register..register + size]);
            u64::from_le_bytes(value)
        })
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut state = self.0.lock().unwrap();
        let (function, register) = split(offset);
        if let Some(bytes) = state.config.get_mut(&function) {
            bytes[register..register + size].copy_from_slice(&value.to_le_bytes()[..size]);
        }
    }
}

impl Registers for PmRegisters {
    fn read(&self, offset: usize, _size: usize) -> u64 {
        u64::from(self.0.lock().unwrap().pm[offset])
    }

    fn write(&self, offset: usize, _size: usize, value: u64) {
        self.0.lock().unwrap().pm[offset] = value as u8;
    }
}

impl Platform for Chipset {
    fn map(&self, phys: u64, len: usize) -> Option<Mmio> {
        (phys == PM_MMIO && len <= 8).then(|| Mmio::fake(Arc::new(PmRegisters(self.0.clone())), 8))
    }

    fn in8(&self, port: u16) -> u8 {
        let mut state = self.0.lock().unwrap();
        let base = state.base();
        match port {
            PM_DATA => state.pm[usize::from(state.pm_index) % 8],
            _ if (base..base + 16).contains(&port) => state.smbus_read(port - base),
            _ => 0xff,
        }
    }

    fn out8(&self, port: u16, value: u8) {
        let mut state = self.0.lock().unwrap();
        let base = state.base();
        match port {
            PM_INDEX => state.pm_index = value,
            PM_DATA => {
                let index = usize::from(state.pm_index) % 8;
                state.pm[index] = value;
            }
            _ if (base..base + 16).contains(&port) => state.smbus_write(port - base, value),
            _ => {}
        }
    }

    fn delay_us(&self, _us: u64) {}
}

/// A function's configuration space: its IDs, revision and class.
fn header(vendor: u16, device: u16, revision: u8, class: (u8, u8)) -> Vec<u8> {
    let mut bytes = std::vec![0; pci::CONFIG_SIZE];
    bytes[0..2].copy_from_slice(&vendor.to_le_bytes());
    bytes[2..4].copy_from_slice(&device.to_le_bytes());
    bytes[0x08] = revision;
    bytes[0x0a] = class.1;
    bytes[0x0b] = class.0;
    bytes
}

/// An SPD EEPROM whose byte `n` is `n` + `seed`.
fn eeprom(seed: u8) -> [u8; 256] {
    core::array::from_fn(|n| (n as u8).wrapping_add(seed))
}

fn simulated(chip: Chip) -> (Mmio, Chipset) {
    let mut config = HashMap::new();
    let mut pm = [0; 8];
    match chip {
        Chip::Ich9 => {
            let mut bytes = header(INTEL, 0x2930, 0x02, SMBUS_CLASS);
            bytes[INTEL_BAR..INTEL_BAR + 4]
                .copy_from_slice(&u32::from(BASE_INTEL | 1).to_le_bytes());
            // Firmware left HST_EN clear.
            config.insert((31, 3), bytes);
        }
        Chip::Fch(revision) => {
            config.insert(FCH_SMBUS, header(AMD, FCH_KERNCZ.0, revision, SMBUS_CLASS));
            pm[0] = SMBUS_EN;
            pm[1] = (BASE_FCH >> 8) as u8;
        }
    }
    let state = State {
        chip,
        config,
        pm,
        pm_index: 0,
        status: 0,
        control: 0,
        command: 0,
        address: 0,
        data: [0; 2],
        owned: false,
        held_for: 0,
        given_back: 0,
        devices: HashMap::from([((0, 0x50), eeprom(0)), ((0, 0x52), eeprom(0x80))]),
    };
    let state = Arc::new(Mutex::new(state));
    (Mmio::fake(Arc::new(Config(state.clone())), BUS_SIZE), Chipset(state))
}

fn name<P: Platform>(smbus: &Smbus<P>) -> std::string::String {
    let mut name = std::string::String::new();
    smbus.adapter_name(&mut name).unwrap();
    name
}

#[test]
fn intels_controller_reads_and_writes_its_devices() {
    let (bus, chipset) = simulated(Chip::Ich9);
    let mut smbus = find(&bus, chipset.clone(), |why| panic!("{why}")).unwrap();
    assert_eq!(smbus.kind(), Kind::Intel(0x2930));
    assert_eq!(name(&smbus), "SMBus I801 adapter at 0700");
    assert_eq!(smbus.ports(), 0x700..0x720);
    {
        let state = chipset.0.lock().unwrap();
        let config = &state.config[&(31, 3)];
        assert_ne!(config[HOSTC] & HST_EN, 0, "HST_EN is set");
        assert_ne!(config[0x04] & 1, 0, "I/O decoding is on");
    }
    assert_eq!(smbus.read_byte_data(0x50, 2), Ok(2));
    assert_eq!(smbus.read_byte_data(0x52, 2), Ok(0x82));
    assert_eq!(smbus.read_word_data(0x50, 0x10), Ok(0x1110));
    assert_eq!(smbus.write_byte_data(0x50, 0x20, 0x5a), Ok(()));
    assert_eq!(smbus.read_byte_data(0x50, 0x20), Ok(0x5a));
    assert_eq!(smbus.read_byte_data(0x51, 0), Err(Error::NoDevice));
    // Each transaction gives firmware back the semaphore and clears its
    // status, the failed one too.
    assert_eq!(chipset.0.lock().unwrap().status, 0);
    assert_eq!(smbus.read_byte_data(0x52, 0), Ok(0x80));
}

#[test]
fn the_fchs_controller_takes_the_bus_and_port_0() {
    for revision in [0x49, 0x61] {
        let (bus, chipset) = simulated(Chip::Fch(revision));
        // The firmware left port 2 picked, and the microcontroller holds
        // the bus for a few tries.
        {
            let mut state = chipset.0.lock().unwrap();
            state.pm[2] = 2 << 3 | 0b111;
            state.held_for = 3;
            state.devices.insert((2, 0x50), eeprom(0x40));
        }
        let mut smbus = find(&bus, chipset.clone(), |why| panic!("{why}")).unwrap();
        assert_eq!(smbus.kind(), Kind::Fch(revision));
        assert_eq!(name(&smbus), "SMBus PIIX4 adapter port 0 at 0b00");
        assert_eq!(smbus.ports(), 0xb00..0xb10);
        assert_eq!(smbus.read_byte_data(0x50, 3), Ok(3), "port 0's device answers");
        assert_eq!(smbus.read_byte_data(0x51, 0), Err(Error::NoDevice));
        let state = chipset.0.lock().unwrap();
        assert_eq!(state.pm[2], 2 << 3 | 0b111, "the port and the other bits are put back");
        assert!(!state.owned);
        assert_eq!(state.given_back, 2);
    }
}

#[test]
fn a_bus_the_microcontroller_keeps_is_busy() {
    let (bus, chipset) = simulated(Chip::Fch(0x61));
    chipset.0.lock().unwrap().held_for = u32::MAX;
    let mut smbus = find(&bus, chipset.clone(), |why| panic!("{why}")).unwrap();
    assert_eq!(smbus.read_byte_data(0x50, 0), Err(Error::Busy));
    // No transaction ran, and the port was left alone.
    assert_eq!(chipset.0.lock().unwrap().address, 0);
}

#[test]
fn controllers_that_are_off_or_absent_are_not_used() {
    // The FCH's, not enabled.
    let (bus, chipset) = simulated(Chip::Fch(0x61));
    chipset.0.lock().unwrap().pm[0] = 0;
    let mut why = None;
    assert!(find(&bus, chipset, |reason| why = Some(reason)).is_none());
    assert_eq!(why, Some(Unusable::FchOff));
    // Intel's, with no base.
    let (bus, chipset) = simulated(Chip::Ich9);
    chipset.0.lock().unwrap().config.get_mut(&(31, 3)).unwrap()[INTEL_BAR] = 0;
    let mut why = None;
    assert!(find(&bus, chipset, |reason| why = Some(reason)).is_none());
    assert_eq!(why, Some(Unusable::NoBase));
    // An FCH from before Zen's kind, and a bus with neither.
    let (bus, chipset) = simulated(Chip::Fch(0x42));
    assert!(find(&bus, chipset.clone(), |why| panic!("{why}")).is_none());
    chipset.0.lock().unwrap().config.clear();
    assert!(find(&bus, chipset, |why| panic!("{why}")).is_none());
}
