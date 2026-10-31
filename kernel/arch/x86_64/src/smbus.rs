// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The chipset's SMBus host controller (ADR 0206), through `radix-smbus`:
//! Intel's, or that of AMD's FCH. It reaches what the watchdog does
//! ([`Chipset`]): device memory that is not RAM, and the chipset's I/O
//! ports.
//!
//! The firmware's AML may drive the controller itself, through its
//! ports, which a transaction of the kernel's would mix with. As Linux's
//! `i2c-i801` does, the first time the AML reads or writes one of them
//! ([`firmware_port`]) the kernel's transaction in flight is waited for,
//! and the controller is left to the firmware from then on.
//!
//! A transaction polls the controller every 250 us until it is done, with
//! the wait the kernel gave [`find`], which sleeps where it can, under a
//! lock whose waiters sleep too (ADR 0232): reading a memory module's SPD
//! EEPROM at run time takes hundreds of them.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use radix_hal::{SmbusError, SmbusTransfer};
use radix_hw::pci;
use radix_klog::{TextBuf, info, warn};
use radix_smbus::{Error, Kind, Platform, Smbus};
use radix_sync::{Mutex, Once};

use crate::watchdog::Chipset;
use crate::{acpi, devices, time};

/// The controller, if found; the lock keeps it to one transaction.
static CONTROLLER: Once<Mutex<Smbus<Chipset>>> = Once::new();

/// How a transaction waits between polls, which the kernel gives.
static WAIT: Once<fn(u64)> = Once::new();

/// The controller's ports once found: the first in the low half, one past
/// the last in the high.
static PORTS: AtomicU32 = AtomicU32::new(0);

/// Whether the firmware's AML has used the controller's ports.
static FIRMWARE: AtomicBool = AtomicBool::new(false);

/// Where memory modules' SPD EEPROMs answer.
const SPD: core::ops::RangeInclusive<u8> = 0x50..=0x57;

impl Platform for Chipset {
    fn map(&self, phys: u64, len: usize) -> Option<radix_hw::Mmio> {
        radix_watchdog::Platform::map(self, phys, len)
    }

    fn in8(&self, port: u16) -> u8 {
        radix_watchdog::Platform::in8(self, port)
    }

    fn out8(&self, port: u16, value: u8) {
        radix_watchdog::Platform::out8(self, port, value);
    }

    fn delay_us(&self, us: u64) {
        match WAIT.get() {
            Some(wait) => wait(us),
            None => time::delay_us(us),
        }
    }
}

/// Finds the controller, and logs it and the memory modules' EEPROMs that
/// answer on it; its transactions wait between polls with `wait`. Returns
/// whether there is one.
pub fn find(wait: fn(u64)) -> bool {
    let _ = WAIT.set(wait);
    let Some(bus) = acpi::get().and_then(|tables| tables.mcfg.as_ref()).and_then(|mcfg| {
        let window = radix_acpi::mcfg_entries(mcfg).find(|w| w.segment == 0 && w.start_bus == 0)?;
        devices::map(window.base, pci::BUS_SIZE)
    }) else {
        return false;
    };
    let Some(smbus) = radix_smbus::find(&bus, Chipset, |why| warn!("SMBus: {why}; it is not used"))
    else {
        return false;
    };
    let mut what = TextBuf::<48>::new();
    let _ = match smbus.kind() {
        Kind::Intel(device) => write!(what, "Intel's controller (8086:{device:04x})"),
        Kind::Fch(revision) => write!(what, "the FCH's controller (revision {revision:#x})"),
    };
    let mut name = TextBuf::<48>::new();
    let _ = smbus.adapter_name(&mut name);
    let (base, ports) = (smbus.base(), smbus.ports());
    if CONTROLLER.set(Mutex::new(smbus)).is_err() {
        return false;
    }
    PORTS.store(u32::from(ports.start) | u32::from(ports.end) << 16, Ordering::Release);
    let mut answer = TextBuf::<64>::new();
    for address in SPD {
        if transfer(|smbus| smbus.read_byte_data(address, 0)).is_ok() {
            let comma = if answer.is_empty() { "" } else { ", " };
            let _ = write!(answer, "{comma}{address:#x}");
        }
    }
    if answer.is_empty() {
        info!("SMBus: {what} at port {base:#x} ({name}); no memory module's EEPROM answers");
    } else {
        info!(
            "SMBus: {what} at port {base:#x} ({name}); memory modules' EEPROMs answer at {answer}"
        );
    }
    true
}

/// Runs `transaction` on the controller, unless there is none or the
/// firmware has it.
fn transfer<R>(
    transaction: impl FnOnce(&mut Smbus<Chipset>) -> Result<R, Error>,
) -> Result<R, SmbusError> {
    let mut smbus = CONTROLLER.get().ok_or(SmbusError::NoController)?.lock();
    if FIRMWARE.load(Ordering::Acquire) {
        return Err(SmbusError::NoController);
    }
    transaction(&mut smbus).map_err(|error| match error {
        Error::NoDevice => SmbusError::NoDevice,
        Error::Busy | Error::Collision => SmbusError::Busy,
        Error::Failed | Error::Timeout => SmbusError::Failed,
    })
}

/// Runs one of the kernel's transactions.
pub fn run(address: u8, request: SmbusTransfer) -> Result<u16, SmbusError> {
    transfer(|smbus| match request {
        SmbusTransfer::SendByte(value) => smbus.send_byte(address, value).map(|()| 0),
        SmbusTransfer::ReceiveByte => smbus.receive_byte(address).map(u16::from),
        SmbusTransfer::ReadByte(command) => smbus.read_byte_data(address, command).map(u16::from),
        SmbusTransfer::ReadWord(command) => smbus.read_word_data(address, command),
        SmbusTransfer::WriteByte(command, value) => {
            smbus.write_byte_data(address, command, value).map(|()| 0)
        }
    })
}

/// Called before the firmware's AML reads or writes `port`: if it is one
/// of the controller's, the kernel's transaction in flight, if any, is
/// waited for, and the controller is the firmware's from then on.
pub fn firmware_port(port: u16) {
    let ports = PORTS.load(Ordering::Acquire);
    let (start, end) = (ports as u16, (ports >> 16) as u16);
    if !(start..end).contains(&port) || FIRMWARE.load(Ordering::Acquire) {
        return;
    }
    if let Some(controller) = CONTROLLER.get() {
        let _wait = controller.lock();
        FIRMWARE.store(true, Ordering::Release);
    }
}

/// Writes the controller's name as Linux names its adapter; fails if
/// there is none.
pub fn name(out: &mut dyn Write) -> fmt::Result {
    CONTROLLER.get().ok_or(fmt::Error)?.lock().adapter_name(out)
}
