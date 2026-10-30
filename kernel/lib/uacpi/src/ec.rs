// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The ACPI embedded controller (ACPI 6.5, chapter 12; Phase 11c,
//! ADR 0201): the microcontroller of a laptop that keeps its battery, its
//! AC adapter, its lid and its keyboard's hot keys, which the firmware's
//! AML reaches through `EmbeddedControl` regions.
//!
//! The controller (`PNP0C09`) has two I/O ports, data and command/status,
//! which its `_CRS` names in that order. Each access of AML is one
//! transaction: a command (read or write), the address, and the byte read
//! or written, each step waiting for the controller to empty its input
//! buffer or fill its output buffer. Transactions are one at a time, and
//! under ACPI's global lock too when `_GLK` asks for it. When the
//! controller has something to tell, it raises its status's SCI_EVT and
//! the general-purpose event its `_GPE` names; the kernel then asks it
//! which event (the query command) and runs the controller's `_Qxx`
//! method for it, in the events thread, as Linux does in its own.
//!
//! The controller is found once the namespace is loaded and before its
//! devices' `_STA` and `_INI` run, since those often read it; the ECDT,
//! which describes it for the tables' own code, is not read.

use core::ffi::CStr;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_sync::Once;

use crate::ffi::{self, Node};
use crate::sync::{FOREVER_MS, Semaphore};
use crate::{Level, Queue, host};

/// The status register: the output buffer holds a byte for the host.
const OBF: u8 = 1 << 0;
/// The status register: the input buffer still holds the host's last byte.
const IBF: u8 = 1 << 1;
/// The status register: the controller has an event for the host.
const SCI_EVT: u8 = 1 << 5;

/// Commands.
const READ: u8 = 0x80;
const WRITE: u8 = 0x81;
const QUERY: u8 = 0x84;

/// How long the controller has for each step of a transaction, as
/// Linux's `ec_delay`.
const STEP_TIMEOUT_NS: u64 = 500_000_000;
/// Status reads a microsecond apart before a step sleeps between them.
const SPIN_POLLS: u32 = 100;
/// The most queries one event drains, so that a controller that keeps
/// answering cannot hold the events thread.
const MAX_QUERIES: u32 = 32;

/// The controller did not answer in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeout;

/// The machine's embedded controller.
pub struct Controller {
    node: Node,
    data: u16,
    command: u16,
    gpe: Option<u16>,
    global_lock: bool,
    /// One transaction at a time.
    lock: Semaphore,
    /// The controller raised SCI_EVT: the events thread queries it.
    query: AtomicBool,
}

static CONTROLLER: Once<Controller> = Once::new();

/// What [`probe`] found, for the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    /// The data port.
    pub data: u16,
    /// The command and status port.
    pub command: u16,
    /// The general-purpose event it raises, if it has one in `\_GPE`.
    pub gpe: Option<u16>,
    /// Whether its transactions take ACPI's global lock.
    pub global_lock: bool,
}

/// The controller [`probe`] found, if any.
pub fn found() -> Option<Found> {
    let controller = CONTROLLER.get()?;
    Some(Found {
        data: controller.data,
        command: controller.command,
        gpe: controller.gpe,
        global_lock: controller.global_lock,
    })
}

/// The data and command ports in a `_CRS` resource template: its first
/// two I/O port descriptors.
pub fn ports(resources: &[u8]) -> Option<(u16, u16)> {
    let mut found = [0u16; 2];
    let mut count = 0;
    let mut at = 0;
    while at < resources.len() && count < 2 {
        let tag = resources[at];
        if tag & 0x80 != 0 {
            // A large descriptor: a 16-bit length follows its tag.
            let len =
                usize::from(u16::from_le_bytes([*resources.get(at + 1)?, *resources.get(at + 2)?]));
            at += 3 + len;
            continue;
        }
        let (kind, len) = (tag >> 3, usize::from(tag & 7));
        let body = resources.get(at + 1..at + 1 + len)?;
        match kind {
            // I/O port: information, minimum base, maximum base,
            // alignment, length.
            0x08 if len >= 7 => {
                found[count] = u16::from_le_bytes([body[1], body[2]]);
                count += 1;
            }
            // Fixed I/O port: base, length.
            0x09 if len >= 3 => {
                found[count] = u16::from_le_bytes([body[0], body[1]]) & 0x3ff;
                count += 1;
            }
            // The end tag.
            0x0f => break,
            _ => {}
        }
        at += 1 + len;
    }
    (count == 2).then_some((found[0], found[1]))
}

/// The controller's general-purpose event, from `_GPE`: an index in
/// `\_GPE`. One in a GPE block device of its own is not supported.
fn gpe(node: Node) -> Option<u16> {
    node.integer(c"_GPE").ok().and_then(|gpe| u16::try_from(gpe).ok())
}

/// Finds the first embedded controller of the namespace and has its
/// regions go through it: called by [`crate::start`] after the namespace
/// is loaded and before it is initialized.
pub fn probe() {
    let mut first = None;
    ffi::find_devices(c"PNP0C09", &mut |node| {
        if first.is_none() {
            first = Some(node);
        }
    });
    let Some(node) = first else { return };
    let ports = node.bytes(c"_CRS", ports);
    let Ok(Some((data, command))) = ports else {
        host().log(
            Level::Warn,
            format_args!("the embedded controller {} names no ports; it is not used", node.path()),
        );
        return;
    };
    let global_lock = node.integer(c"_GLK").is_ok_and(|value| value != 0);
    let gpe = gpe(node);
    let controller = Controller {
        node,
        data,
        command,
        gpe,
        global_lock,
        lock: Semaphore::new(1),
        query: AtomicBool::new(false),
    };
    if CONTROLLER.set(controller).is_err() {
        return;
    }
    if let Err(err) = ffi::serve_embedded_controller(node) {
        host().log(
            Level::Error,
            format_args!("the embedded controller {} cannot serve AML: {err}", node.path()),
        );
        return;
    }
    if let Some(gpe) = gpe
        && let Err(err) = ffi::on_gpe(gpe, event)
    {
        host().log(
            Level::Warn,
            format_args!(
                "the embedded controller's events (GPE {gpe:#x}) cannot be heard: {err}; \
                 its regions still work"
            ),
        );
    }
}

/// The controller's general-purpose event, in an interrupt handler: an
/// event waiting means a query, in the events thread.
fn event() {
    let Some(controller) = CONTROLLER.get() else { return };
    if controller.status() & SCI_EVT != 0 {
        controller.query.store(true, Ordering::Release);
        host().work_queued(Queue::Events);
    }
}

/// Runs the queries the controller asked for, in the events thread:
/// each event's `_Qxx` method. Returns whether there were any.
pub fn run_queries() -> bool {
    let Some(controller) = CONTROLLER.get() else { return false };
    if !controller.query.swap(false, Ordering::Acquire) {
        return false;
    }
    for _ in 0..MAX_QUERIES {
        let event = match controller.transaction(Controller::query) {
            Ok(0) => break,
            Ok(event) => event,
            Err(Timeout) => {
                host().log(
                    Level::Warn,
                    format_args!("the embedded controller did not answer a query"),
                );
                break;
            }
        };
        let name = query_method(event);
        let Ok(method) = CStr::from_bytes_with_nul(&name) else { break };
        match controller.node.run(method) {
            Ok(()) => {}
            Err(err) if err.is_not_found() => host().log(
                Level::Debug,
                format_args!("the embedded controller's event {event:#04x} has no method"),
            ),
            Err(err) => host().log(
                Level::Error,
                format_args!("the embedded controller's method _Q{event:02X} failed: {err}"),
            ),
        }
        if controller.status() & SCI_EVT == 0 {
            break;
        }
    }
    true
}

/// The method of query event `event`, as `_Q1A`, NUL-terminated.
pub fn query_method(event: u8) -> [u8; 5] {
    let hex = |nibble: u8| b"0123456789ABCDEF"[usize::from(nibble & 15)];
    [b'_', b'Q', hex(event >> 4), hex(event), 0]
}

/// Reads `width` bytes from `address` of the controller's space, the
/// first byte lowest, for AML.
pub fn read(address: u64, width: u8) -> Result<u64, Access> {
    let controller = CONTROLLER.get().ok_or(Access::Missing)?;
    let first = range(address, width)?;
    controller
        .transaction(|controller| {
            let mut value = 0u64;
            for i in 0..width {
                value |= u64::from(controller.read(first + i)?) << (8 * i);
            }
            Ok(value)
        })
        .map_err(|Timeout| Access::Timeout)
}

/// Writes `width` bytes of `value` to `address` of the controller's
/// space, the lowest first, for AML.
pub fn write(address: u64, width: u8, value: u64) -> Result<(), Access> {
    let controller = CONTROLLER.get().ok_or(Access::Missing)?;
    let first = range(address, width)?;
    controller
        .transaction(|controller| {
            for i in 0..width {
                controller.write(first + i, (value >> (8 * i)) as u8)?;
            }
            Ok(())
        })
        .map_err(|Timeout| Access::Timeout)
}

/// Why an access of AML's failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// There is no controller.
    Missing,
    /// The access is outside the controller's 256 bytes.
    Range,
    /// The controller did not answer.
    Timeout,
}

fn range(address: u64, width: u8) -> Result<u8, Access> {
    match u8::try_from(address) {
        Ok(first) if (1..=8).contains(&width) && u64::from(first) + u64::from(width) <= 256 => {
            Ok(first)
        }
        _ => Err(Access::Range),
    }
}

impl Controller {
    fn status(&self) -> u8 {
        host().port_read(self.command, 1) as u8
    }

    /// Waits until the status satisfies `ready`: polls for a while, then
    /// sleeps a millisecond between polls.
    fn wait_for(&self, ready: impl Fn(u8) -> bool) -> Result<(), Timeout> {
        let host = host();
        for _ in 0..SPIN_POLLS {
            if ready(self.status()) {
                return Ok(());
            }
            host.stall_us(1);
        }
        let deadline = host.now_ns().saturating_add(STEP_TIMEOUT_NS);
        loop {
            if ready(self.status()) {
                return Ok(());
            }
            if host.now_ns() >= deadline {
                return Err(Timeout);
            }
            host.sleep_ms(1);
        }
    }

    fn send_command(&self, command: u8) -> Result<(), Timeout> {
        self.wait_for(|status| status & IBF == 0)?;
        host().port_write(self.command, 1, u32::from(command));
        Ok(())
    }

    fn send_data(&self, byte: u8) -> Result<(), Timeout> {
        self.wait_for(|status| status & IBF == 0)?;
        host().port_write(self.data, 1, u32::from(byte));
        Ok(())
    }

    fn receive(&self) -> Result<u8, Timeout> {
        self.wait_for(|status| status & OBF != 0)?;
        Ok(host().port_read(self.data, 1) as u8)
    }

    fn read(&self, address: u8) -> Result<u8, Timeout> {
        self.send_command(READ)?;
        self.send_data(address)?;
        self.receive()
    }

    fn write(&self, address: u8, value: u8) -> Result<(), Timeout> {
        self.send_command(WRITE)?;
        self.send_data(address)?;
        self.send_data(value)?;
        self.wait_for(|status| status & IBF == 0)
    }

    fn query(&self) -> Result<u8, Timeout> {
        self.send_command(QUERY)?;
        self.receive()
    }

    /// Runs `steps` as one transaction: alone, and under the global lock
    /// if `_GLK` asks for it.
    fn transaction<T>(
        &self,
        steps: impl FnOnce(&Self) -> Result<T, Timeout>,
    ) -> Result<T, Timeout> {
        let host = host();
        self.lock.take(host, FOREVER_MS);
        let held = if self.global_lock { ffi::take_global_lock() } else { None };
        let result = steps(self);
        if let Some(sequence) = held {
            ffi::give_global_lock(sequence);
        }
        self.lock.give(host);
        result
    }
}

impl fmt::Display for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ports {:#x} and {:#x}", self.data, self.command)?;
        match self.gpe {
            Some(gpe) => write!(f, ", GPE {gpe:#x}")?,
            None => f.write_str(", no GPE")?,
        }
        if self.global_lock {
            f.write_str(", under the global lock")?;
        }
        Ok(())
    }
}
