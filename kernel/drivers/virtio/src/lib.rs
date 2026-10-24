// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Virtio over PCI: the transport, virtqueues and the block driver.
//!
//! Virtio (OASIS Virtual I/O Device specification, version 1.2) is how a
//! virtual machine's devices talk to their hypervisor: a device and its
//! driver share queues of buffers in memory (*virtqueues*), the driver
//! adds buffers and rings a doorbell, and the device hands them back and
//! raises an interrupt. This crate is a compartment: it has no `unsafe`
//! code, and reaches its device only through the [`Mmio`] windows and
//! [`radix_hw::Dma`] memory `radix-hw` gives it.
//!
//! - [`Transport`] is the *modern* PCI transport of virtio 1.0 and later:
//!   vendor-specific PCI capabilities locate the common configuration, the
//!   doorbells (notifications), the interrupt status and the device's own
//!   configuration in the function's BARs. The legacy transport of virtio
//!   0.9 is not supported (ADR 0053).
//! - [`queue::Queue`] is a *split* virtqueue: a descriptor table, a ring
//!   of buffers available to the device and a ring of buffers it used.
//! - [`blk::Blk`] is the block device (virtio-blk) on one queue, with one
//!   MSI-X interrupt for its completions.
//!
//! Nothing here waits: drivers submit requests and collect completions,
//! and the kernel decides how threads wait for them (in `radix`, which
//! wakes them from the queue's interrupt). The tests run the driver
//! against a simulated device on the host.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod blk;
pub mod queue;

#[cfg(test)]
mod tests;

use core::fmt;

use radix_hw::Mmio;
use radix_hw::pci::{self, Bar, Function};

/// The PCI vendor ID of virtio devices.
pub const VENDOR: u16 = 0x1af4;
/// Modern devices' PCI device IDs are this plus the virtio device type.
pub const MODERN_BASE: u16 = 0x1040;

/// Virtio device types.
pub mod device_type {
    /// A network card.
    pub const NET: u16 = 1;
    /// A block device.
    pub const BLOCK: u16 = 2;
}

/// The virtio device type of a PCI function, if it is a virtio device: a
/// modern ID (0x1040 plus the type) or a transitional one (0x1000 to
/// 0x103f, whose type is in the subsystem ID).
pub fn device_type(function: &Function) -> Option<u16> {
    if function.vendor_id() != VENDOR {
        return None;
    }
    match function.device_id() {
        id @ 0x1040..=0x107f => Some(id - MODERN_BASE),
        0x1000..=0x103f => Some(function.subsystem_id()),
        _ => None,
    }
}

/// What can go wrong setting up or driving a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A configuration structure the driver needs is missing, or points
    /// outside its BAR.
    MissingStructure(&'static str),
    /// A BAR the device uses could not be mapped.
    Map,
    /// The device lacks a feature the driver needs.
    MissingFeature(&'static str),
    /// The device refused the features the driver chose.
    FeaturesRefused,
    /// The device has no such queue, or offers none of a usable size.
    NoQueue,
    /// The device did not accept the queue's interrupt vector.
    NoVector,
    /// Memory for the queues could not be allocated.
    NoMemory,
    /// The device set its NEEDS_RESET status bit.
    DeviceFailed,
    /// The queue has no free descriptors for the request.
    Full,
    /// The request is outside the device or its limits.
    BadRequest,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::MissingStructure(name) => write!(f, "no usable {name} structure"),
            Error::Map => write!(f, "a BAR could not be mapped"),
            Error::MissingFeature(name) => write!(f, "the device lacks {name}"),
            Error::FeaturesRefused => write!(f, "the device refused the driver's features"),
            Error::NoQueue => write!(f, "no usable queue"),
            Error::NoVector => write!(f, "the device refused the queue's interrupt vector"),
            Error::NoMemory => write!(f, "out of memory for the queues"),
            Error::DeviceFailed => write!(f, "the device needs a reset"),
            Error::Full => write!(f, "the queue is full"),
            Error::BadRequest => write!(f, "the request is outside the device's limits"),
        }
    }
}

/// Device status bits (virtio 1.2, section 2.1).
pub mod status {
    /// The driver found the device.
    pub const ACKNOWLEDGE: u8 = 1;
    /// The driver knows how to drive it.
    pub const DRIVER: u8 = 2;
    /// The driver is ready.
    pub const DRIVER_OK: u8 = 4;
    /// The driver accepted the features.
    pub const FEATURES_OK: u8 = 8;
    /// The device hit an error it cannot recover from.
    pub const NEEDS_RESET: u8 = 64;
    /// The driver gave up on the device.
    pub const FAILED: u8 = 128;
}

/// Feature bits every device type shares.
pub mod feature {
    /// The device follows virtio 1.0 or later.
    pub const VERSION_1: u64 = 1 << 32;
}

/// Offsets in the common configuration structure (virtio 1.2, 4.1.4.3).
mod common {
    pub const DEVICE_FEATURE_SELECT: usize = 0x00;
    pub const DEVICE_FEATURE: usize = 0x04;
    pub const DRIVER_FEATURE_SELECT: usize = 0x08;
    pub const DRIVER_FEATURE: usize = 0x0c;
    pub const CONFIG_MSIX_VECTOR: usize = 0x10;
    pub const NUM_QUEUES: usize = 0x12;
    pub const DEVICE_STATUS: usize = 0x14;
    pub const CONFIG_GENERATION: usize = 0x15;
    pub const QUEUE_SELECT: usize = 0x16;
    pub const QUEUE_SIZE: usize = 0x18;
    pub const QUEUE_MSIX_VECTOR: usize = 0x1a;
    pub const QUEUE_ENABLE: usize = 0x1c;
    pub const QUEUE_NOTIFY_OFF: usize = 0x1e;
    pub const QUEUE_DESC: usize = 0x20;
    pub const QUEUE_DRIVER: usize = 0x28;
    pub const QUEUE_DEVICE: usize = 0x30;
    /// The size of the structure.
    pub const SIZE: usize = 0x38;
}

/// "No interrupt vector", for the MSI-X vector registers.
pub const NO_VECTOR: u16 = 0xffff;

const CAP_COMMON: u8 = 1;
const CAP_NOTIFY: u8 = 2;
const CAP_ISR: u8 = 3;
const CAP_DEVICE: u8 = 4;

/// The modern PCI transport of one device: its four configuration
/// structures, found through its vendor-specific capabilities.
#[derive(Clone, Debug)]
pub struct Transport {
    common: Mmio,
    notify: Mmio,
    notify_multiplier: u32,
    device: Mmio,
}

impl Transport {
    /// Finds the device's configuration structures. `map_bar` maps one of
    /// the function's memory BARs (by index) whole.
    pub fn find(
        function: &Function,
        map_bar: &mut dyn FnMut(usize, Bar) -> Option<Mmio>,
    ) -> Result<Transport, Error> {
        let mut bars: [Option<Mmio>; 6] = [const { None }; 6];
        let mut common = None;
        let mut notify = None;
        let mut device = None;
        for (id, offset) in function.capabilities() {
            if id != pci::capability::VENDOR {
                continue;
            }
            let kind = function.read8(offset + 3);
            let bar = usize::from(function.read8(offset + 4));
            let start = function.read32(offset + 8) as usize;
            let length = function.read32(offset + 12) as usize;
            if !matches!(kind, CAP_COMMON | CAP_NOTIFY | CAP_DEVICE) || bar >= 6 {
                continue;
            }
            // The registers are read whole, so a structure must start
            // aligned to its widest one: 4 bytes for the common and
            // device structures, 2 for the doorbells (virtio 1.2, 4.1.4).
            // One that does not is skipped, as one the driver cannot use.
            let align = if kind == CAP_NOTIFY { 2 } else { 4 };
            if !start.is_multiple_of(align) {
                continue;
            }
            // The first structure of each kind the driver can use wins
            // (virtio 1.2, 4.1.4).
            let slot = match kind {
                CAP_COMMON if common.is_none() => &mut common,
                CAP_NOTIFY if notify.is_none() => &mut notify,
                CAP_DEVICE if device.is_none() => &mut device,
                _ => continue,
            };
            let Some(Bar::Memory { .. }) = function.bar(bar) else { continue };
            if bars[bar].is_none() {
                let found = function.bar(bar).ok_or(Error::Map)?;
                bars[bar] = Some(map_bar(bar, found).ok_or(Error::Map)?);
            }
            let Some(window) = bars[bar].as_ref().and_then(|whole| whole.window(start, length))
            else {
                continue;
            };
            let multiplier = if kind == CAP_NOTIFY { function.read32(offset + 16) } else { 0 };
            *slot = Some((window, multiplier));
        }
        let (common, _) = common.ok_or(Error::MissingStructure("common configuration"))?;
        if common.len() < common::SIZE {
            return Err(Error::MissingStructure("common configuration"));
        }
        let (notify, notify_multiplier) = notify.ok_or(Error::MissingStructure("notification"))?;
        let (device, _) = device.ok_or(Error::MissingStructure("device configuration"))?;
        let _ = CAP_ISR;
        Ok(Transport { common, notify, notify_multiplier, device })
    }

    /// The device's status bits.
    pub fn status(&self) -> u8 {
        self.common.read8(common::DEVICE_STATUS)
    }

    fn set_status(&self, value: u8) {
        self.common.write8(common::DEVICE_STATUS, value);
    }

    /// Adds `bits` to the device status.
    pub fn add_status(&self, bits: u8) {
        self.set_status(self.status() | bits);
    }

    /// Resets the device and waits (up to a bounded number of reads) until
    /// it says it is reset. Afterwards it uses no memory the driver gave
    /// it.
    pub fn reset(&self) -> bool {
        self.set_status(0);
        (0..1_000_000).any(|_| self.status() == 0)
    }

    /// The features the device offers.
    pub fn device_features(&self) -> u64 {
        self.common.write32(common::DEVICE_FEATURE_SELECT, 0);
        let low = self.common.read32(common::DEVICE_FEATURE);
        self.common.write32(common::DEVICE_FEATURE_SELECT, 1);
        let high = self.common.read32(common::DEVICE_FEATURE);
        (u64::from(high) << 32) | u64::from(low)
    }

    /// Tells the device the features the driver uses and checks that it
    /// accepts them.
    pub fn set_driver_features(&self, features: u64) -> Result<(), Error> {
        self.common.write32(common::DRIVER_FEATURE_SELECT, 0);
        self.common.write32(common::DRIVER_FEATURE, features as u32);
        self.common.write32(common::DRIVER_FEATURE_SELECT, 1);
        self.common.write32(common::DRIVER_FEATURE, (features >> 32) as u32);
        self.add_status(status::FEATURES_OK);
        if self.status() & status::FEATURES_OK == 0 {
            return Err(Error::FeaturesRefused);
        }
        Ok(())
    }

    /// How many queues the device has.
    pub fn queues(&self) -> u16 {
        self.common.read16(common::NUM_QUEUES)
    }

    /// Sends configuration-change interrupts nowhere.
    pub fn no_config_interrupt(&self) {
        self.common.write16(common::CONFIG_MSIX_VECTOR, NO_VECTOR);
    }

    /// The device-specific configuration structure.
    pub fn device_config(&self) -> &Mmio {
        &self.device
    }

    /// Reads the device configuration consistently: runs `read` until the
    /// configuration generation is the same before and after.
    pub fn read_config<T>(&self, read: impl Fn(&Mmio) -> T) -> T {
        loop {
            let before = self.common.read8(common::CONFIG_GENERATION);
            let value = read(&self.device);
            if self.common.read8(common::CONFIG_GENERATION) == before {
                return value;
            }
        }
    }

    /// The common configuration structure.
    pub(crate) fn common(&self) -> &Mmio {
        &self.common
    }

    /// The doorbell window of queue `queue`, whose notification offset is
    /// `notify_off`: `None` if the device puts it outside the structure,
    /// or where a 16-bit write cannot go (an odd multiplier).
    pub(crate) fn doorbell(&self, notify_off: u16) -> Option<Mmio> {
        let offset = usize::from(notify_off).checked_mul(self.notify_multiplier as usize)?;
        if !offset.is_multiple_of(2) {
            return None;
        }
        self.notify.window(offset, 2)
    }
}
