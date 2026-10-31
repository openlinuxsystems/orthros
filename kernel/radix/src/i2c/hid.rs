// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! HID over I2C (Phase 11e, ADR 0235), through `radix_i2c::hid`: a
//! laptop's touchpad, and any keyboard or touchscreen on a DesignWare
//! bus that the ACPI namespace names as `PNP0C50` (or `ACPI0C50`), by its
//! `_HID` or its `_CID`.
//!
//! [`start`] runs once the buses are found, and starts the thread
//! `i2c-hid`, which sets each device up as Linux's `i2c-hid-acpi` and
//! `i2c-hid-core` do: it runs the device's `_PS0`, asks its `_DSM` where
//! its HID descriptor is, reads it, asks for the GPIO pin its `_CRS`
//! names as its interrupt ([`gpio::request`]), wakes and resets it and
//! waits a second at most for the interrupt that says it has, then reads
//! its report descriptor and lays it out (`radix_hid::Layout`). A
//! touchpad is left as the mouse it is until told to report its fingers.
//!
//! Then each interrupt of a device wakes the thread, which reads its
//! input register once, decodes the report (`radix_hid::Decoder`) and
//! tells what changed ([`crate::hid::tell`]), as USB's HID interfaces
//! do; a level-triggered pin is unmasked once its report is read. A
//! device whose reads fail more than three times in a row is given up.

use alloc::vec::Vec;
use core::ffi::CStr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use radix_acpi::resources::{GpioUsage, Resource, Resources};
use radix_hid::{Decoder, Layout};
use radix_i2c::hid::{self as protocol, Descriptor, Failed, Input};
use radix_i2c::{Error, Message};
use radix_klog::{info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::Once;

use super::designware::{self, Device};
use crate::sched::{self, Options, Policy, ThreadRef};
use crate::{acpi, gpio};

/// The IDs a HID over I2C device has as its `_HID` or `_CID`.
const IDS: [&CStr; 2] = [c"PNP0C50", c"ACPI0C50"];

/// The most devices kept: a bit each in [`PENDING`].
const MAX_HIDS: usize = 4;

/// How long a device has to say it has reset.
const RESET_NS: u64 = 1_000_000_000;

/// Failed reads in a row before a device is given up.
const READ_ERRORS: u8 = 3;

/// The devices found, by their bit in [`PENDING`].
static CANDIDATES: Once<[Option<Device>; MAX_HIDS]> = Once::new();
/// The devices whose interrupts came, waiting for the thread.
static PENDING: AtomicU32 = AtomicU32::new(0);
static THREAD: Once<ThreadRef> = Once::new();
/// Whether the thread has set up every device it could.
static SETTLED: AtomicBool = AtomicBool::new(false);

/// A device's transfers, on its bus at its address.
#[derive(Clone, Copy)]
struct Target {
    bus: usize,
    address: u16,
    ten_bit: bool,
}

impl Target {
    fn of(device: &Device) -> Target {
        Target { bus: device.bus, address: device.address, ten_bit: device.ten_bit }
    }
}

impl protocol::Device for Target {
    fn transfer(&mut self, messages: &mut [Message<'_>]) -> Result<(), Error> {
        designware::transfer(self.bus, self.address, self.ten_bit, messages)
    }
}

/// A device set up.
struct Hid {
    index: usize,
    device: Device,
    descriptor: Descriptor,
    interrupt: gpio::Interrupt,
    level: bool,
    layout: Layout<Kmalloc>,
    decoder: Decoder,
    errors: u8,
    stopped: bool,
}

/// Why a device is not used.
enum Why {
    NoRegister(radix_uacpi::Error),
    BadRegister(u64),
    Descriptor(u16, Failed),
    NoInterrupt,
    Interrupt(gpio::Failed),
    Power(Error),
    Report(Failed),
    Layout(radix_hid::Error),
    NoMemory,
}

impl core::fmt::Display for Why {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Why::NoRegister(error) => {
                write!(f, "its _DSM gives no HID descriptor register ({error})")
            }
            Why::BadRegister(value) => {
                write!(f, "its _DSM gives {value:#x} as its HID descriptor register")
            }
            Why::Descriptor(register, Failed::Bus(error)) => {
                write!(f, "its HID descriptor at {register:#06x} could not be read ({error})")
            }
            Why::Report(Failed::Bus(error)) => {
                write!(f, "its report descriptor could not be read ({error})")
            }
            Why::Descriptor(_, failed) | Why::Report(failed) => write!(f, "{failed}"),
            Why::NoInterrupt => f.write_str("its resources name no GPIO interrupt"),
            Why::Interrupt(failed) => write!(f, "its interrupt cannot be had: {failed}"),
            Why::Power(error) => write!(f, "it could not be woken and reset ({error})"),
            Why::Layout(error) => write!(f, "{error}"),
            Why::NoMemory => f.write_str("there was no memory for it"),
        }
    }
}

/// Why a read of a device's input register failed.
enum ReadFailed {
    Bus(Error),
    Length(usize),
}

impl core::fmt::Display for ReadFailed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReadFailed::Bus(error) => write!(f, "{error}"),
            ReadFailed::Length(length) => {
                write!(f, "a report's length ({length}) is not one its descriptor allows")
            }
        }
    }
}

/// Finds the HID over I2C devices on the buses found, and starts the
/// thread that sets them up and reads them.
pub fn start() {
    // Without the firmware's AML there are no buses, and uACPI is not to
    // be asked.
    if !acpi::running() {
        SETTLED.store(true, Ordering::Release);
        return;
    }
    let mut candidates = [None; MAX_HIDS];
    let mut count = 0;
    for id in IDS {
        radix_uacpi::find_devices(id, &mut |node| {
            designware::devices(&mut |device| {
                let known = candidates[..count].iter().flatten().any(|d: &Device| d.node == node);
                if device.node != node || known {
                    return;
                }
                if count == MAX_HIDS {
                    warn!(
                        "I2C-HID: more devices than the kernel keeps; {} is not used",
                        node.path()
                    );
                    return;
                }
                candidates[count] = Some(*device);
                count += 1;
            });
        });
    }
    if count == 0 {
        SETTLED.store(true, Ordering::Release);
        return;
    }
    CANDIDATES.call_once(|| candidates);
    let options = Options::new().policy(Policy::Fair { nice: -5 });
    if let Err(err) = sched::spawn("i2c-hid", run, 0, options) {
        warn!("I2C-HID: could not start the thread `i2c-hid`: {err}; its devices are not used");
        SETTLED.store(true, Ordering::Release);
    }
}

/// Whether every device found has been set up, or given up.
pub fn settled() -> bool {
    SETTLED.load(Ordering::Acquire)
}

/// A device's pin interrupted: the thread reads it.
fn on_interrupt(index: usize) {
    PENDING.fetch_or(1 << index, Ordering::AcqRel);
    if let Some(thread) = THREAD.get() {
        sched::wake(thread);
    }
}

/// The thread `i2c-hid`: sets the devices up, then reads each whose
/// interrupt came.
fn run(_: usize) {
    if let Some(me) = sched::current() {
        let _ = THREAD.set(me);
    }
    let Some(candidates) = CANDIDATES.get() else { return };
    let heap = HEAP.with(Flags::MAY_SLEEP);
    let mut buffer: Vec<u8, Kmalloc> = Vec::new_in(heap);
    let mut hids: Vec<Hid, Kmalloc> = Vec::new_in(heap);
    if buffer.try_reserve_exact(protocol::MAX_INPUT).is_err()
        || hids.try_reserve_exact(MAX_HIDS).is_err()
    {
        warn!("I2C-HID: there was no memory for its thread; its devices are not used");
        SETTLED.store(true, Ordering::Release);
        return;
    }
    while buffer.len() < protocol::MAX_INPUT {
        // Room was reserved above.
        let _ = buffer.push_within_capacity(0);
    }
    for (index, device) in candidates.iter().enumerate() {
        let Some(device) = device else { continue };
        match set_up(index, device, &mut buffer) {
            Ok(hid) => {
                let _ = hids.push_within_capacity(hid);
            }
            Err(why) => warn!(
                "I2C-HID: {} on i2c-{} at {:#04x}: {why}; it is not used",
                device.node.path(),
                device.bus,
                device.address
            ),
        }
    }
    SETTLED.store(true, Ordering::Release);
    if hids.is_empty() {
        return;
    }
    loop {
        let pending = PENDING.swap(0, Ordering::AcqRel);
        for hid in hids.iter_mut().filter(|hid| pending & (1 << hid.index) != 0) {
            serve(hid, &mut buffer);
        }
        if PENDING.load(Ordering::Acquire) == 0 {
            sched::block("i2c-hid");
        }
    }
}

/// Sets up device `index`, `device`, with `buffer` for its input.
#[inline(never)]
fn set_up(index: usize, device: &Device, buffer: &mut [u8]) -> Result<Hid, Why> {
    let node = device.node;
    if node.find("_PS0").is_some()
        && let Err(error) = node.run(c"_PS0")
    {
        warn!("I2C-HID: {}'s _PS0 failed: {error}", node.path());
    }
    let register = node
        .dsm(&protocol::DSM_UUID, protocol::DSM_REVISION, protocol::DSM_FUNCTION)
        .map_err(Why::NoRegister)?;
    let register = u16::try_from(register).map_err(|_| Why::BadRegister(register))?;
    let mut target = Target::of(device);
    let descriptor = Descriptor::read(&mut target, register)
        .map_err(|failed| Why::Descriptor(register, failed))?;
    // Its interrupt: the first GpioInt of its resources.
    let bit = 1 << index;
    PENDING.fetch_and(!bit, Ordering::AcqRel);
    let requested = node
        .bytes(c"_CRS", |bytes| {
            Resources::new(bytes).find_map(|resource| {
                let Ok(Resource::Gpio(pin)) = resource else { return None };
                let GpioUsage::Interrupt(signal) = pin.usage else { return None };
                Some(gpio::request(node, &pin, on_interrupt, index).map(|i| (i, !signal.edge)))
            })
        })
        .ok()
        .flatten()
        .ok_or(Why::NoInterrupt)?;
    let (interrupt, level) = requested.map_err(Why::Interrupt)?;
    // Some devices sleep so deeply that the first request wakes them
    // without being acknowledged.
    if descriptor.set_power(&mut target, true) == Err(Error::NoDevice) {
        sched::sleep_ns(1_000_000);
    }
    descriptor.set_power(&mut target, true).map_err(Why::Power)?;
    descriptor.reset(&mut target).map_err(Why::Power)?;
    let deadline = sched::now_ns().saturating_add(RESET_NS);
    while PENDING.load(Ordering::Acquire) & bit == 0 && sched::now_ns() < deadline {
        sched::block_until(deadline, "i2c-hid");
    }
    if PENDING.fetch_and(!bit, Ordering::AcqRel) & bit != 0 {
        // What it read after resetting: a length of 0.
        let _ = descriptor.read_input(&mut target, buffer);
        if level {
            interrupt.unmask();
        }
    } else {
        warn!("I2C-HID: {} did not say it had reset; it is read anyway", node.path());
    }
    let heap = HEAP.with(Flags::MAY_SLEEP);
    let length = usize::from(descriptor.report_length);
    let mut bytes: Vec<u8, Kmalloc> = Vec::new_in(heap);
    bytes.try_reserve_exact(length).map_err(|_| Why::NoMemory)?;
    while bytes.len() < length {
        // Room was reserved above.
        let _ = bytes.push_within_capacity(0);
    }
    let report = descriptor.read_report_descriptor(&mut target, &mut bytes).map_err(Why::Report)?;
    let layout = Layout::parse(report, heap).map_err(Why::Layout)?;
    let (what, _) = crate::hid::what(&layout);
    info!(
        "I2C-HID: {} ({:04x}:{:04x}) on i2c-{} at {:#04x}: {what}",
        node.path(),
        descriptor.vendor,
        descriptor.product,
        device.bus,
        device.address
    );
    Ok(Hid {
        index,
        device: *device,
        descriptor,
        interrupt,
        level,
        layout,
        decoder: Decoder::new(),
        errors: 0,
        stopped: false,
    })
}

/// Reads `hid`'s input register once, as its interrupt asks, and tells
/// what its report changed.
#[inline(never)]
fn serve(hid: &mut Hid, buffer: &mut [u8]) {
    if hid.stopped {
        return;
    }
    let mut target = Target::of(&hid.device);
    let failed = match hid.descriptor.read_input(&mut target, buffer) {
        Ok(Input::Report(report)) => {
            hid.decoder.report(&hid.layout, report, &mut crate::hid::tell);
            None
        }
        Ok(Input::Reset) => None,
        Ok(Input::Bad(length)) => Some(ReadFailed::Length(length)),
        Err(error) => Some(ReadFailed::Bus(error)),
    };
    match failed {
        None => hid.errors = 0,
        Some(why) => {
            hid.errors += 1;
            if hid.errors > READ_ERRORS {
                // Left masked, if its pin is level-triggered.
                hid.stopped = true;
                hid.decoder.release_all(&mut crate::hid::tell);
                warn!("I2C-HID: {} stopped reporting: {why}", hid.device.node.path());
                return;
            }
        }
    }
    if hid.level {
        hid.interrupt.unmask();
    }
}
