// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The machine's I2C and SMBus adapters (Phase 11e, ADR 0206): the
//! chipset's SMBus host controller, which the platform finds after the
//! PCI scan, and `/kernel/bus/i2c/devices/i2c-0` names, as Linux's sysfs
//! does; and the DesignWare I2C controllers that Intel's LPSS and AMD's
//! FCH have, `i2c-1` onwards ([`designware`], ADR 0234).
//!
//! On it, the memory modules' SPD EEPROMs (ADR 0232), found as Linux's
//! `i2c_register_spd` finds them. The SMBIOS tables say how many memory
//! slots there are, how many are filled and with which kind of memory,
//! which says which EEPROM the modules have ([`Eeprom::of_kind`]); then
//! the addresses from 50h, one a slot, are tried until as many answer as
//! slots are filled: with a Receive Byte, which writes nothing to an
//! EEPROM, or for DDR5 by the SPD5118's device type. Each found is
//! `/kernel/bus/i2c/devices/0-00<address>`, whose `eeprom` is read whole
//! the first time it is opened, a byte a transaction, and kept.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use radix_hal::{Devices, SmbusError, SmbusTransfer, Timer};
use radix_klog::{TextBuf, info, warn};
use radix_smbios::memory::kind_name;
use radix_spd::{Eeprom, MAX_SIZE};
use radix_sync::{Mutex, Once};

use crate::{Arch, sched};

pub mod designware;

/// Whether the platform found the SMBus host controller.
static FOUND: AtomicBool = AtomicBool::new(false);

/// The memory modules' SPD EEPROMs found.
static SPDS: Once<Spds> = Once::new();

/// What was read of each, by its place in [`SPDS`].
static READ: Mutex<Read> = Mutex::new(Read { done: [false; 8], bytes: [[0; MAX_SIZE]; 8] });

/// The SPD EEPROMs found, all of one kind.
struct Spds {
    eeprom: Eeprom,
    addresses: [u8; 8],
    count: usize,
}

struct Read {
    done: [bool; 8],
    bytes: [[u8; MAX_SIZE]; 8],
}

/// Asks the platform for the SMBus host controller, which logs it, then
/// looks for the memory modules' SPD EEPROMs on it; then finds the
/// DesignWare controllers.
pub fn find() {
    if Arch::find_smbus(wait) {
        FOUND.store(true, Ordering::Release);
        find_spds();
    } else {
        info!("SMBus: none the kernel drives");
    }
    designware::find();
}

/// Whether there is one.
pub fn found() -> bool {
    FOUND.load(Ordering::Acquire)
}

/// Waits `us` microseconds between polls of the controller: asleep where
/// the caller may sleep, as at run time, else with the CPU waiting.
fn wait(us: u64) {
    if sched::can_sleep() {
        sched::sleep_ns(us.saturating_mul(1000));
    } else {
        Arch::delay_us(us);
    }
}

/// The SMBus, as an SPD read uses it.
struct Smbus;

impl radix_spd::Bus for Smbus {
    type Error = SmbusError;

    fn send_byte(&mut self, address: u8, value: u8) -> Result<(), SmbusError> {
        Arch::smbus_transfer(address, SmbusTransfer::SendByte(value)).map(|_| ())
    }

    fn receive_byte(&mut self, address: u8) -> Result<u8, SmbusError> {
        Arch::smbus_transfer(address, SmbusTransfer::ReceiveByte).map(|byte| byte as u8)
    }

    fn read_byte(&mut self, address: u8, command: u8) -> Result<u8, SmbusError> {
        Arch::smbus_transfer(address, SmbusTransfer::ReadByte(command)).map(|byte| byte as u8)
    }

    fn write_byte(&mut self, address: u8, command: u8, value: u8) -> Result<(), SmbusError> {
        Arch::smbus_transfer(address, SmbusTransfer::WriteByte(command, value)).map(|_| ())
    }

    fn is_nack(error: &SmbusError) -> bool {
        *error == SmbusError::NoDevice
    }
}

/// Finds the memory modules' SPD EEPROMs, by what the SMBIOS tables say
/// of the memory, and logs them.
fn find_spds() {
    let Some(memory) = crate::dmi::memory().filter(|memory| memory.modules > 0) else { return };
    let Some(kind) = memory.kind else {
        info!(
            "SMBus: the memory modules are of more than one kind; their SPD EEPROMs are not read"
        );
        return;
    };
    let name = kind_name(kind);
    let Some(eeprom) = Eeprom::of_kind(kind) else {
        info!("SMBus: {name} memory (kind {kind:#04x}) has no SPD EEPROM the kernel reads");
        return;
    };
    if memory.slots > radix_spd::ADDRESSES.len() {
        info!(
            "SMBus: {} memory slots are more than one bus has SPD addresses for; their EEPROMs \
             are not read",
            memory.slots
        );
        return;
    }
    let mut spds = Spds { eeprom, addresses: [0; 8], count: 0 };
    let mut list = TextBuf::<64>::new();
    for address in radix_spd::ADDRESSES.take(memory.slots) {
        if spds.count == memory.modules {
            break;
        }
        let answers = match eeprom {
            Eeprom::Spd5118 => radix_spd::is_spd5118(&mut Smbus, address),
            Eeprom::Spd | Eeprom::Ee1004 => {
                Arch::smbus_transfer(address, SmbusTransfer::ReceiveByte).is_ok()
            }
        };
        if answers {
            spds.addresses[spds.count] = address;
            spds.count += 1;
            let comma = if list.is_empty() { "" } else { ", " };
            let _ = write!(list, "{comma}{address:#x}");
        }
    }
    if spds.count == 0 {
        info!("SMBus: no SPD EEPROM of the {name} memory answers");
        return;
    }
    info!("SMBus: the {name} memory modules' SPD EEPROMs ({}) at {list}", eeprom.name());
    let _ = SPDS.set(spds);
}

/// Calls `spd` with the place, address and kind of each SPD EEPROM found.
pub fn spds(spd: &mut dyn FnMut(usize, u8, Eeprom)) {
    if let Some(spds) = SPDS.get() {
        for (index, &address) in spds.addresses[..spds.count].iter().enumerate() {
            spd(index, address, spds.eeprom);
        }
    }
}

/// Calls `out` with the bytes of the SPD EEPROM at place `index`, which
/// the first call reads; fails if they cannot be read.
pub fn read_spd(index: usize, out: impl FnOnce(&[u8]) -> fmt::Result) -> fmt::Result {
    let spds = SPDS.get().ok_or(fmt::Error)?;
    let address = *spds.addresses[..spds.count].get(index).ok_or(fmt::Error)?;
    let size = spds.eeprom.size();
    let mut read = READ.lock();
    let Read { done, bytes } = &mut *read;
    if !done[index] {
        match radix_spd::read(&mut Smbus, spds.eeprom, address, &mut bytes[index]) {
            Ok(()) => done[index] = true,
            Err(error) => {
                drop(read);
                warn!("SMBus: the SPD EEPROM at {address:#x} could not be read: {error}");
                return Err(fmt::Error);
            }
        }
    }
    out(&bytes[index][..size])
}
