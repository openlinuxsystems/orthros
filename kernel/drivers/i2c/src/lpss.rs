// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Intel's LPSS (the PCH's low power subsystem), whose I2C controllers
//! are DesignWare's: each a PCI function, or, in the firmware's ACPI
//! mode, an ACPI device, with the controller's registers first in its
//! window and LPSS's own after them (Intel's *100 Series Chipset Family
//! PCH Datasheet*; Linux's `intel-lpss` as the reference for what is
//! set). From [`PRIVATE`]:
//!
//! - RESETS (0x04): the function's reset (bits 1:0) and its DMA engine's
//!   (bit 2), each held while 0.
//! - REMAP_ADDR (0x40, 0x44): the window's address, which the DMA engine
//!   is told.
//! - CAPS (0xfc): what the function is (bits 7:4: 0 for I2C), and
//!   whether it has no DMA engine (bit 8).
//!
//! The clock is 120 MHz on Sunrise Point; Linux takes SDA's hold time as
//! 230 ns there unless the firmware says.

use radix_hw::Mmio;

/// Where LPSS's own registers are in a function's window.
pub const PRIVATE: usize = 0x200;
/// How many bytes they take.
pub const PRIVATE_SIZE: usize = 0x100;
/// The controllers' clock on Sunrise Point.
pub const CLOCK_HZ: u64 = 120_000_000;
/// The SDA hold time Linux takes there, in nanoseconds.
pub const SDA_HOLD_NS: u64 = 230;

const RESETS: usize = 0x04;
const RESETS_FUNCTION: u32 = 0x3;
const RESETS_DMA: u32 = 1 << 2;
const REMAP_ADDR: usize = 0x40;
const CAPS: usize = 0xfc;
const CAPS_NO_DMA: u32 = 1 << 8;
const CAPS_TYPE_SHIFT: u32 = 4;
const CAPS_TYPE: u32 = 0xf << CAPS_TYPE_SHIFT;
const TYPE_I2C: u32 = 0;

/// What a function says it is, when it is not an I2C controller: its
/// CAPS register's type (1 a UART, 2 an SPI controller).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotI2c(pub u32);

/// Takes the function whose LPSS registers are `private` (the window's
/// [`PRIVATE`] onwards) and whose window is at `address` out of reset, as
/// Linux's `intel_lpss_init_dev` does: reset, then out of it, and its DMA
/// engine told the window's address.
pub fn set_up(private: &Mmio, address: u64) -> Result<(), NotI2c> {
    let caps = private.read32(CAPS);
    let kind = (caps & CAPS_TYPE) >> CAPS_TYPE_SHIFT;
    if kind != TYPE_I2C || caps == u32::MAX {
        return Err(NotI2c(kind));
    }
    private.write32(RESETS, 0);
    private.write32(RESETS, RESETS_FUNCTION | RESETS_DMA);
    if caps & CAPS_NO_DMA == 0 {
        private.write32(REMAP_ADDR, address as u32);
        private.write32(REMAP_ADDR + 4, (address >> 32) as u32);
    }
    Ok(())
}
