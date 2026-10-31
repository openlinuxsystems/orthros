// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! I2C controllers for the Radix kernel (Phase 11e, ADR 0234): the
//! Synopsys DesignWare controller that Intel's LPSS (the PCH's low power
//! subsystem) and AMD's FCH both have, on which a laptop's touchpad and
//! other small devices sit.
//!
//! Intel's controllers sit in LPSS functions, which [`lpss`] takes out
//! of reset first.
//!
//! A transfer is a list of messages to one device, each a write or a read
//! of some bytes, joined by repeated starts and ended by a stop, as an
//! I2C device's registers are read: a write of the register's address,
//! then a read ([`Message`]). The controller ([`designware`]) is polled:
//! Intel's interrupt is a legacy PCI interrupt, which the kernel does not
//! route, and a transfer is short.
//!
//! The crate has no `unsafe` code; the kernel maps the registers and
//! gives the waits ([`Platform`]). Its tests run it against a simulated
//! controller and devices.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod designware;
pub mod lpss;
#[cfg(test)]
mod tests;

use core::fmt;

/// What a controller needs from the kernel.
pub trait Platform {
    /// Waits at least `us` microseconds, asleep where the caller may
    /// sleep: while a transfer has nothing more to queue.
    fn wait_us(&self, us: u64);
    /// Waits at least `us` microseconds without sleeping: while the
    /// controller's queue must not run dry.
    fn spin_us(&self, us: u64);
}

/// One message of a transfer.
pub enum Message<'a> {
    /// Bytes written to the device.
    Write(&'a [u8]),
    /// Bytes read from it, into the buffer, as many as it has.
    Read(&'a mut [u8]),
}

impl Message<'_> {
    fn len(&self) -> usize {
        match self {
            Message::Write(bytes) => bytes.len(),
            Message::Read(bytes) => bytes.len(),
        }
    }
}

/// The bus's speed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    /// Standard mode, 100 kHz.
    Standard,
    /// Fast mode, 400 kHz.
    Fast,
}

impl Speed {
    /// The fastest the bus may run for devices that allow `hz` at most:
    /// fast mode from 400 kHz, standard mode below.
    pub fn for_devices(hz: u32) -> Speed {
        if hz >= 400_000 { Speed::Fast } else { Speed::Standard }
    }

    /// Its clock in hertz.
    pub fn hz(self) -> u32 {
        match self {
            Speed::Standard => 100_000,
            Speed::Fast => 400_000,
        }
    }
}

/// Why a transfer failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No device acknowledged the address.
    NoDevice,
    /// The device did not acknowledge a byte written to it.
    Nack,
    /// Another controller won the bus.
    ArbitrationLost,
    /// The controller gave up on the transfer for another reason, the
    /// bits of its abort source register.
    Aborted(u32),
    /// The bus stayed busy, or the transfer did not end in time.
    Timeout,
    /// A message has no bytes, or the address does not fit.
    Invalid,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoDevice => f.write_str("no device acknowledged its address"),
            Error::Nack => f.write_str("the device did not acknowledge a byte"),
            Error::ArbitrationLost => f.write_str("another controller won the bus"),
            Error::Aborted(source) => write!(f, "the controller aborted it ({source:#x})"),
            Error::Timeout => f.write_str("the transfer did not end in time"),
            Error::Invalid => f.write_str("a message is empty, or the address too large"),
        }
    }
}
