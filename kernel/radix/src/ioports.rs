// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A device's I/O BAR as `radix_hw::IoPorts`, for the drivers whose
//! registers are in I/O port space (UHCI, IDE): accesses past its end
//! read all ones and write nothing, so a driver reaches only its own
//! device's ports (Phase 11g, ADR 0243).

use radix_hal::Devices;

use crate::Arch;

/// A function's I/O BAR: the only ports it reaches.
#[derive(Clone, Copy, Debug)]
pub struct IoWindow {
    base: u16,
    len: u16,
}

impl IoWindow {
    /// The `len` ports from `base`, cut at the end of port space.
    pub fn new(base: u16, len: u16) -> IoWindow {
        IoWindow { base, len: len.min(u16::MAX - base) }
    }

    fn port(self, offset: u16, size: u16) -> Option<u16> {
        (offset.checked_add(size)? <= self.len).then(|| self.base + offset)
    }

    fn read(self, offset: u16, size: u8) -> Option<u32> {
        let port = self.port(offset, u16::from(size))?;
        Some(Arch::device_port_read(port, size))
    }

    fn write(self, offset: u16, size: u8, value: u32) {
        if let Some(port) = self.port(offset, u16::from(size)) {
            Arch::device_port_write(port, size, value);
        }
    }
}

impl radix_hw::IoPorts for IoWindow {
    fn len(&self) -> u16 {
        self.len
    }

    fn read8(&self, offset: u16) -> u8 {
        self.read(offset, 1).map_or(u8::MAX, |v| v as u8)
    }

    fn write8(&self, offset: u16, value: u8) {
        self.write(offset, 1, u32::from(value));
    }

    fn read16(&self, offset: u16) -> u16 {
        self.read(offset, 2).map_or(u16::MAX, |v| v as u16)
    }

    fn write16(&self, offset: u16, value: u16) {
        self.write(offset, 2, u32::from(value));
    }

    fn read32(&self, offset: u16) -> u32 {
        self.read(offset, 4).unwrap_or(u32::MAX)
    }

    fn write32(&self, offset: u16, value: u32) {
        self.write(offset, 4, value);
    }
}
