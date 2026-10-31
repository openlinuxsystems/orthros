// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The machine's I2C and SMBus adapters (Phase 11e, ADR 0206): for now the
//! chipset's SMBus host controller, which the platform finds after the
//! PCI scan, and `/kernel/bus/i2c/devices/i2c-0` names, as Linux's sysfs
//! does.

use core::sync::atomic::{AtomicBool, Ordering};

use radix_hal::Devices;
use radix_klog::info;

use crate::Arch;

/// Whether the platform found the SMBus host controller.
static FOUND: AtomicBool = AtomicBool::new(false);

/// Asks the platform for the SMBus host controller, which logs it.
pub fn find() {
    if Arch::find_smbus() {
        FOUND.store(true, Ordering::Release);
    } else {
        info!("SMBus: none the kernel drives");
    }
}

/// Whether there is one.
pub fn found() -> bool {
    FOUND.load(Ordering::Acquire)
}
