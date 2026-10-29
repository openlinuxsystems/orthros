// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! xHCI, the USB host controller of every PC since about 2012 (Intel's
//! *eXtensible Host Controller Interface for Universal Serial Bus,
//! Revision 1.2*), for controllers of version 0.96 and later (Phase 11,
//! ADR 0146).
//!
//! An xHCI controller is one PCI function with its registers in BAR 0:
//! its capabilities, its operational registers (with a status and control
//! register for each root port), its runtime registers (with an
//! *interrupter* for each interrupt it raises), and a doorbell for itself
//! and for each device slot. It reads its work from rings in memory and
//! writes what happened to an event ring:
//!
//! - the *command ring*, of commands to the controller: give a device
//!   slot, address the device in it, set up its endpoints, reset or stop
//!   an endpoint, give the slot back;
//! - a *transfer ring* for each endpoint of each device, of the transfers
//!   to and from it;
//! - the *event ring* of interrupter 0, where the controller says that a
//!   command or a transfer finished and that a port changed.
//!
//! The crate takes the controller over and runs it:
//!
//! - [`Xhci::new`] takes the controller from the firmware (the USB Legacy
//!   Support capability, 4.22.1: the firmware lets go, and its SMIs are
//!   turned off), halts and resets it, gives it the Device Context Base
//!   Address Array (with the scratchpad pages it asks for), a command
//!   ring and an event ring, and starts it with its interrupt on. It reads
//!   the Supported Protocol capabilities, which say which root ports are
//!   USB 2 and which USB 3, and the speeds their Port Speed field names.
//! - [`Xhci::port_status`] and its neighbours read and change the root
//!   ports: acknowledging changes, resetting, powering.
//! - [`Xhci::next_event`] reads the event ring, and [`Xhci::events_read`]
//!   tells the controller how far, which lets it interrupt again.
//! - [`Xhci::command`] gives the controller a command and waits for it,
//!   aborting it if it takes too long.
//! - [`Device`] is a device in a slot: [`Device::address`] gives it one
//!   and its address, and it makes control transfers on its default
//!   control pipe, recovering the pipe when the device stalls or does not
//!   answer. [`Device::configure`] sets up its other endpoints, whose
//!   transfers the kernel starts and whose events it reads with the rest;
//!   a hub's slot says so ([`Device::set_hub`]).
//! - [`dbc::Dbc`] runs the controller's Debug Capability, if it has one: a
//!   USB 3 device inside the controller, which a second computer reads the
//!   kernel's console from (ADR 0160).
//!
//! The crate is a compartment: it has no `unsafe` code, and reaches the
//! controller only through the [`Mmio`] window and the [`Dma`] memory that
//! `radix-hw` gives it. Nothing here sleeps or takes a lock: the kernel
//! passes in a [`Host`], which waits (for the controller's interrupt, or a
//! time) and keeps the time, and takes the events that come while a
//! command or transfer is waited for that are not its own. The tests run
//! the driver against a simulated controller on the host.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod dbc;
mod device;
pub mod ring;

#[cfg(test)]
mod tests;

use core::fmt;

pub use device::{BUFFER_SIZE, Device, HubSlot, MAX_ENDPOINTS, Place, endpoint_index, interval};
use radix_hw::{Dma, Mmio, PAGE_SIZE};
use radix_usb::Speed;
use ring::{EventRing, Ring, Trb, control, kind};

/// The PCI class, subclass and programming interface of an xHCI
/// controller: serial bus, USB, xHCI.
pub const PCI_CLASS: (u8, u8, u8) = (0x0c, 0x03, 0x30);
/// The BAR that holds its registers.
pub const BAR: usize = 0;

/// The capability registers (5.3).
mod cap {
    /// CAPLENGTH in bits 0 to 7, HCIVERSION in 16 to 31.
    pub const LENGTH_VERSION: usize = 0x00;
    /// Structural parameters: slots, interrupters, ports.
    pub const HCSPARAMS1: usize = 0x04;
    /// Structural parameters: scratchpad pages.
    pub const HCSPARAMS2: usize = 0x08;
    /// Capability parameters.
    pub const HCCPARAMS1: usize = 0x10;
    /// Where the doorbells and the runtime registers are.
    pub const DBOFF: usize = 0x14;
    pub const RTSOFF: usize = 0x18;

    /// HCCPARAMS1: 64-bit addresses.
    pub const AC64: u32 = 1 << 0;
    /// HCCPARAMS1: contexts of 64 bytes, not 32.
    pub const CSZ: u32 = 1 << 2;
    /// HCCPARAMS1: ports are powered by software.
    pub const PPC: u32 = 1 << 3;
}

/// The operational registers (5.4).
mod op {
    pub const USBCMD: usize = 0x00;
    pub const USBSTS: usize = 0x04;
    pub const PAGESIZE: usize = 0x08;
    /// Command Ring Control, 64 bits.
    pub const CRCR: usize = 0x18;
    /// Device Context Base Address Array Pointer, 64 bits.
    pub const DCBAAP: usize = 0x30;
    pub const CONFIG: usize = 0x38;
    /// The first port's registers; each port has 16 bytes.
    pub const PORTS: usize = 0x400;
    pub const PORT_STRIDE: usize = 0x10;

    /// USBCMD: run, reset, interrupts on, host system error interrupts on.
    pub const RUN: u32 = 1 << 0;
    pub const RESET: u32 = 1 << 1;
    pub const INTE: u32 = 1 << 2;
    pub const HSEE: u32 = 1 << 3;
    /// USBSTS: halted, host system error, event interrupt, port change,
    /// not ready, host controller error.
    pub const HALTED: u32 = 1 << 0;
    pub const HSE: u32 = 1 << 2;
    pub const EINT: u32 = 1 << 3;
    pub const PCD: u32 = 1 << 4;
    pub const NOT_READY: u32 = 1 << 11;
    pub const HCE: u32 = 1 << 12;
    /// CRCR: abort, running.
    pub const CRCR_ABORT: u32 = 1 << 2;
    pub const CRCR_RUNNING: u32 = 1 << 3;
}

/// Interrupter 0's runtime registers (5.5.2), from its own start.
mod rt {
    /// Where interrupter 0's registers are in the runtime registers.
    pub const INTERRUPTER0: usize = 0x20;
    pub const IMAN: usize = 0x00;
    pub const IMOD: usize = 0x04;
    pub const ERSTSZ: usize = 0x08;
    /// 64 bits each.
    pub const ERSTBA: usize = 0x10;
    pub const ERDP: usize = 0x18;

    /// IMAN: interrupt pending (write 1 to clear), enabled.
    pub const IP: u32 = 1 << 0;
    pub const IE: u32 = 1 << 1;
    /// ERDP: event handler busy (write 1 to clear).
    pub const EHB: u32 = 1 << 3;
    /// The least time between interrupts, in 250 ns units: 40 µs, as
    /// Linux sets.
    pub const MODERATION: u32 = 160;
}

/// The fields of a port's status and control register, PORTSC (5.4.8).
pub mod portsc {
    /// Something is connected.
    pub const CONNECTED: u32 = 1 << 0;
    /// The port is enabled (write 1 to disable).
    pub const ENABLED: u32 = 1 << 1;
    /// Over-current.
    pub const OVER_CURRENT: u32 = 1 << 3;
    /// A reset is in progress (write 1 to start one).
    pub const RESET: u32 = 1 << 4;
    /// The link state, 4 bits from bit 5.
    pub const LINK_SHIFT: u32 = 5;
    /// Power.
    pub const POWER: u32 = 1 << 9;
    /// The speed's ID, 4 bits from bit 10.
    pub const SPEED_SHIFT: u32 = 10;
    /// Port indicator control, 2 bits.
    pub const INDICATOR: u32 = 3 << 14;
    /// Link state write strobe.
    pub const LINK_STROBE: u32 = 1 << 16;
    /// Changes, each written 1 to acknowledge: connection, enable, warm
    /// reset, over-current, reset, link state, config error.
    pub const CONNECT_CHANGE: u32 = 1 << 17;
    /// Enabled or disabled itself.
    pub const ENABLE_CHANGE: u32 = 1 << 18;
    /// A warm reset finished.
    pub const WARM_RESET_CHANGE: u32 = 1 << 19;
    /// Over-current came or went.
    pub const OVER_CURRENT_CHANGE: u32 = 1 << 20;
    /// A reset finished.
    pub const RESET_CHANGE: u32 = 1 << 21;
    /// The link state changed.
    pub const LINK_CHANGE: u32 = 1 << 22;
    /// The link could not be configured.
    pub const CONFIG_ERROR_CHANGE: u32 = 1 << 23;
    /// Wake on connect, disconnect and over-current.
    pub const WAKE: u32 = 7 << 25;
    /// Device removable (a fixed device says it is not).
    pub const NOT_REMOVABLE: u32 = 1 << 30;
    /// Warm reset (write 1 to start one; USB 3 ports).
    pub const WARM_RESET: u32 = 1 << 31;

    /// Every change bit.
    pub const CHANGES: u32 = CONNECT_CHANGE
        | ENABLE_CHANGE
        | WARM_RESET_CHANGE
        | OVER_CURRENT_CHANGE
        | RESET_CHANGE
        | LINK_CHANGE
        | CONFIG_ERROR_CHANGE;
    /// The bits that may be written back as read without changing
    /// anything: the read-only ones and the ones that keep their value.
    /// The enable bit and the change bits are not among them, since
    /// writing them 1 acts.
    pub const PRESERVE: u32 = CONNECTED
        | OVER_CURRENT
        | (0xf << LINK_SHIFT)
        | POWER
        | (0xf << SPEED_SHIFT)
        | INDICATOR
        | WAKE
        | NOT_REMOVABLE;
}

/// The extended capabilities (7).
mod xcap {
    /// USB Legacy Support.
    pub const LEGACY: u32 = 1;
    /// Supported Protocol.
    pub const PROTOCOL: u32 = 2;
    /// Debug Capability.
    pub const DEBUG: u32 = 10;

    /// USBLEGSUP: the firmware owns the controller; the OS does.
    pub const BIOS_OWNED: u32 = 1 << 16;
    pub const OS_OWNED: u32 = 1 << 24;
    /// USBLEGCTLSTS: the bits kept when the SMIs are turned off (the
    /// reserved ones), and the SMI events written 1 to clear them.
    pub const SMI_KEEP: u32 = (0x7 << 1) | (0xff << 5) | (0x7 << 17);
    pub const SMI_EVENTS: u32 = 0x7 << 29;
}

/// Completion codes (6.4.5).
pub mod code {
    /// Success.
    pub const SUCCESS: u8 = 1;
    /// Data Buffer Error.
    pub const DATA_BUFFER: u8 = 2;
    /// Babble Detected.
    pub const BABBLE: u8 = 3;
    /// USB Transaction Error: the device did not answer, after retries.
    pub const TRANSACTION: u8 = 4;
    /// TRB Error.
    pub const TRB: u8 = 5;
    /// Stall Error: the device refused the request.
    pub const STALL: u8 = 6;
    /// Resource Error.
    pub const RESOURCE: u8 = 7;
    /// Bandwidth Error.
    pub const BANDWIDTH: u8 = 8;
    /// No Slots Available.
    pub const NO_SLOTS: u8 = 9;
    /// Slot Not Enabled.
    pub const SLOT_NOT_ENABLED: u8 = 11;
    /// Endpoint Not Enabled.
    pub const ENDPOINT_NOT_ENABLED: u8 = 12;
    /// Short Packet: less came than the TRB asked for.
    pub const SHORT_PACKET: u8 = 13;
    /// Parameter Error.
    pub const PARAMETER: u8 = 17;
    /// Context State Error: the command does not fit the endpoint's or
    /// slot's state.
    pub const CONTEXT_STATE: u8 = 19;
    /// Event Ring Full.
    pub const EVENT_RING_FULL: u8 = 21;
    /// Incompatible Device.
    pub const INCOMPATIBLE: u8 = 22;
    /// Command Ring Stopped.
    pub const COMMAND_RING_STOPPED: u8 = 24;
    /// Command Aborted.
    pub const COMMAND_ABORTED: u8 = 25;
    /// Stopped: a Stop Endpoint command stopped this TRB.
    pub const STOPPED: u8 = 26;
    /// Stopped, Length Invalid.
    pub const STOPPED_LENGTH_INVALID: u8 = 27;
    /// Split Transaction Error.
    pub const SPLIT_TRANSACTION: u8 = 36;

    /// Words for a completion code.
    pub fn name(code: u8) -> &'static str {
        match code {
            SUCCESS => "success",
            DATA_BUFFER => "data buffer error",
            BABBLE => "babble",
            TRANSACTION => "no answer (USB transaction error)",
            TRB => "TRB error",
            STALL => "stall",
            RESOURCE => "out of resources",
            BANDWIDTH => "not enough bandwidth",
            NO_SLOTS => "no device slots left",
            SLOT_NOT_ENABLED => "slot not enabled",
            ENDPOINT_NOT_ENABLED => "endpoint not enabled",
            SHORT_PACKET => "short packet",
            PARAMETER => "parameter error",
            CONTEXT_STATE => "context state error",
            EVENT_RING_FULL => "event ring full",
            INCOMPATIBLE => "incompatible device",
            COMMAND_RING_STOPPED => "command ring stopped",
            COMMAND_ABORTED => "command aborted",
            STOPPED | STOPPED_LENGTH_INVALID => "stopped",
            SPLIT_TRANSACTION => "split transaction error",
            _ => "error",
        }
    }
}

/// How long a command may take: as Linux allows.
const COMMAND_US: u64 = 5_000_000;
/// How long a halt, the start, and stopping the command ring may take.
const HALT_US: u64 = 100_000;
const START_US: u64 = 100_000;
const ABORT_US: u64 = 5_000_000;
/// How long a reset may take.
const RESET_US: u64 = 1_000_000;
/// How long the firmware has to let go of the controller, as Linux allows.
const HANDOFF_US: u64 = 1_000_000;
/// How long ports take to power up (USB 2.0 7.1.7.3: 20 ms, as Linux
/// waits).
const POWER_US: u64 = 20_000;
/// How often registers are read while waiting for them, and how long the
/// driver waits for an event before looking at the clock again.
const POLL_US: u32 = 1_000;

/// What the kernel gives the driver while it waits.
pub trait Host {
    /// Waits for about `us` microseconds, or less if the controller
    /// interrupts.
    fn wait(&mut self, us: u32);
    /// A clock, in microseconds.
    fn now_us(&mut self) -> u64;
    /// An event that came while the driver waited for another, for
    /// whoever wants it.
    fn other(&mut self, event: Event);
}

/// Waits until `done` holds or `limit_us` has passed; returns whether it
/// held.
fn wait_for(host: &mut dyn Host, limit_us: u64, mut done: impl FnMut() -> bool) -> bool {
    let deadline = host.now_us().saturating_add(limit_us);
    loop {
        if done() {
            return true;
        }
        if host.now_us() >= deadline {
            return done();
        }
        host.wait(POLL_US);
    }
}

/// Waits `us` microseconds, whatever wakes the host in between.
fn sleep(host: &mut dyn Host, us: u64) {
    let deadline = host.now_us().saturating_add(us);
    while host.now_us() < deadline {
        host.wait(POLL_US);
    }
}

/// Why the driver could not do what it was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The controller is not one the driver can run.
    Unsupported(&'static str),
    /// It did not halt, reset or start in time, or has stopped answering.
    Unresponsive(&'static str),
    /// There was no memory for its structures.
    NoMemory,
    /// A command or transfer did not finish in time.
    TimedOut,
    /// A command or transfer ended with this completion code.
    Completion(u8),
    /// A request the driver cannot make (too long for its buffer).
    BadRequest,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported(why) | Error::Unresponsive(why) => f.write_str(why),
            Error::NoMemory => f.write_str("out of memory"),
            Error::TimedOut => f.write_str("no answer in time"),
            Error::Completion(c) => write!(f, "{} (completion code {c})", code::name(*c)),
            Error::BadRequest => f.write_str("a request too long"),
        }
    }
}

/// What the controller said on its event ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A command finished: the command's TRB, its completion code, the
    /// slot it names (or gave), and its completion parameter.
    Command {
        /// The command's TRB.
        trb: u64,
        /// Its completion code.
        code: u8,
        /// The slot.
        slot: u8,
        /// The completion parameter.
        parameter: u32,
    },
    /// A transfer TRB finished, or stopped short.
    Transfer {
        /// The TRB.
        trb: u64,
        /// Its completion code.
        code: u8,
        /// The slot.
        slot: u8,
        /// The endpoint, as its device context index.
        endpoint: u8,
        /// The bytes of the TRB not moved.
        residual: u32,
    },
    /// A root port changed: its status register says how.
    Port {
        /// The port, from 1.
        port: u8,
    },
    /// The controller has something to say about itself (its event ring
    /// filled).
    Controller {
        /// The completion code.
        code: u8,
    },
    /// Another kind of event.
    Other {
        /// Its TRB type.
        kind: u8,
    },
}

impl Event {
    fn decode(trb: Trb) -> Event {
        match trb.kind() {
            kind::COMMAND_COMPLETION => Event::Command {
                trb: trb.parameter & !0xf,
                code: trb.code(),
                slot: trb.slot(),
                parameter: trb.status & 0xff_ffff,
            },
            kind::TRANSFER_EVENT => Event::Transfer {
                trb: trb.parameter,
                code: trb.code(),
                slot: trb.slot(),
                endpoint: trb.endpoint(),
                residual: trb.status & 0xff_ffff,
            },
            kind::PORT_STATUS_CHANGE => Event::Port { port: (trb.parameter >> 24) as u8 },
            kind::HOST_CONTROLLER => Event::Controller { code: trb.code() },
            other => Event::Other { kind: other as u8 },
        }
    }
}

/// A command to the controller (4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// No Op.
    Noop,
    /// Enable Slot, for a device on a port of this protocol slot type.
    EnableSlot {
        /// The Supported Protocol's slot type.
        slot_type: u8,
    },
    /// Disable Slot.
    DisableSlot {
        /// The slot.
        slot: u8,
    },
    /// Address Device, from the input context at `input`.
    AddressDevice {
        /// The slot.
        slot: u8,
        /// The input context's physical address.
        input: u64,
        /// Set up the slot without sending SET_ADDRESS.
        block: bool,
    },
    /// Configure Endpoint, from the input context at `input`.
    ConfigureEndpoint {
        /// The slot.
        slot: u8,
        /// The input context's physical address.
        input: u64,
        /// Drop every endpoint but 0 instead.
        deconfigure: bool,
    },
    /// Evaluate Context, from the input context at `input`.
    EvaluateContext {
        /// The slot.
        slot: u8,
        /// The input context's physical address.
        input: u64,
    },
    /// Reset Endpoint: a halted endpoint goes to the stopped state.
    ResetEndpoint {
        /// The slot.
        slot: u8,
        /// The endpoint's device context index.
        endpoint: u8,
    },
    /// Stop Endpoint.
    StopEndpoint {
        /// The slot.
        slot: u8,
        /// The endpoint's device context index.
        endpoint: u8,
    },
    /// Set TR Dequeue Pointer: where a stopped endpoint goes on reading,
    /// with the cycle state in bit 0.
    SetDequeue {
        /// The slot.
        slot: u8,
        /// The endpoint's device context index.
        endpoint: u8,
        /// The new dequeue pointer and cycle state.
        pointer: u64,
    },
}

impl Command {
    fn trb(&self) -> Trb {
        let (parameter, control) = match *self {
            Command::Noop => (0, control::kind(kind::NO_OP_COMMAND)),
            Command::EnableSlot { slot_type } => {
                (0, control::kind(kind::ENABLE_SLOT) | u32::from(slot_type & 0x1f) << 16)
            }
            Command::DisableSlot { slot } => {
                (0, control::kind(kind::DISABLE_SLOT) | control::slot(slot))
            }
            Command::AddressDevice { slot, input, block } => (
                input,
                control::kind(kind::ADDRESS_DEVICE)
                    | control::slot(slot)
                    | if block { control::BSR } else { 0 },
            ),
            Command::ConfigureEndpoint { slot, input, deconfigure } => (
                input,
                control::kind(kind::CONFIGURE_ENDPOINT)
                    | control::slot(slot)
                    | if deconfigure { control::DECONFIGURE } else { 0 },
            ),
            Command::EvaluateContext { slot, input } => {
                (input, control::kind(kind::EVALUATE_CONTEXT) | control::slot(slot))
            }
            Command::ResetEndpoint { slot, endpoint } => (
                0,
                control::kind(kind::RESET_ENDPOINT)
                    | control::slot(slot)
                    | control::endpoint(endpoint),
            ),
            Command::StopEndpoint { slot, endpoint } => (
                0,
                control::kind(kind::STOP_ENDPOINT)
                    | control::slot(slot)
                    | control::endpoint(endpoint),
            ),
            Command::SetDequeue { slot, endpoint, pointer } => (
                pointer,
                control::kind(kind::SET_TR_DEQUEUE)
                    | control::slot(slot)
                    | control::endpoint(endpoint),
            ),
        };
        Trb { parameter, status: 0, control }
    }
}

/// What a finished command said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completed {
    /// The slot it names, or the one Enable Slot gave.
    pub slot: u8,
    /// Its completion parameter.
    pub parameter: u32,
}

/// What became of the firmware's hold on the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handoff {
    /// The controller has no USB Legacy Support capability.
    None,
    /// The firmware did not hold it.
    Free,
    /// The firmware let go when asked.
    Given,
    /// The firmware did not let go in time, and was overruled.
    Taken,
}

/// What the controller is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// The xHCI version it implements, in BCD (0x0100 for 1.0).
    pub version: u16,
    /// Its root ports.
    pub ports: u8,
    /// The device slots it has, so the devices it can address.
    pub slots: u8,
    /// The scratchpad pages it asked for.
    pub scratchpads: u16,
    /// The bytes of a context: 32 or 64.
    pub context_size: usize,
    /// Whether it takes 64-bit addresses.
    pub addresses_64: bool,
    /// Whether software powers its ports.
    pub port_power: bool,
    /// What the firmware did when asked to let go.
    pub handoff: Handoff,
    /// Where its Debug Capability's registers start, if it has one.
    pub debug_capability: Option<usize>,
}

/// The most Supported Protocol capabilities read.
const MAX_PROTOCOLS: usize = 6;

/// A Supported Protocol capability (7.2): a range of root ports, the USB
/// version they speak, and the speeds their Port Speed field names.
#[derive(Clone, Debug, Default)]
pub struct Protocol {
    /// The USB version: 2 or 3, and the minor version in BCD.
    pub major: u8,
    /// The minor version.
    pub minor: u8,
    /// The first port, from 1, and how many.
    pub first_port: u8,
    /// How many ports.
    pub ports: u8,
    /// The slot type Enable Slot is given for a device on these ports.
    pub slot_type: u8,
    /// The Protocol Speed IDs' registers, if the ports name their speeds
    /// with their own (otherwise the defaults of 7.2.2.1.1), a word each.
    psi: Option<Mmio>,
}

impl Protocol {
    /// Whether `port` is one of its ports.
    pub fn covers(&self, port: u8) -> bool {
        let first = u16::from(self.first_port);
        (first..first + u16::from(self.ports)).contains(&u16::from(port))
    }

    /// The speed that a port's Port Speed field `id` names.
    pub fn speed(&self, id: u8) -> Option<Speed> {
        let Some(psis) = &self.psi else {
            return match (self.major, id) {
                (2, 1) => Some(Speed::Full),
                (2, 2) => Some(Speed::Low),
                (2, 3) => Some(Speed::High),
                (3, 4) => Some(Speed::Super),
                (3, 5..=7) => Some(Speed::SuperPlus),
                _ => None,
            };
        };
        let psi =
            (0..psis.len() / 4).map(|i| psis.read32(4 * i)).find(|&psi| (psi & 0xf) as u8 == id)?;
        // The bit rate: a mantissa times a power of 1000.
        let exponent = (psi >> 4) & 3;
        let rate = u64::from(psi >> 16) * 1000u64.pow(exponent);
        Some(match self.major {
            2 if rate <= 1_500_000 => Speed::Low,
            2 if rate <= 12_000_000 => Speed::Full,
            2 => Speed::High,
            _ if rate <= 5_000_000_000 => Speed::Super,
            _ => Speed::SuperPlus,
        })
    }

    /// The Port Speed ID it gives `speed`, for a device behind a hub: the
    /// default IDs, or the first of its own of that speed.
    pub fn speed_id(&self, speed: Speed) -> u8 {
        if let Some(psis) = &self.psi {
            for i in 0..psis.len() / 4 {
                let id = (psis.read32(4 * i) & 0xf) as u8;
                if self.speed(id) == Some(speed) {
                    return id;
                }
            }
        }
        match speed {
            Speed::Full => 1,
            Speed::Low => 2,
            Speed::High => 3,
            Speed::Super => 4,
            Speed::SuperPlus => 5,
        }
    }
}

/// A root port's status and control register, as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus(pub u32);

impl PortStatus {
    /// Whether something is connected.
    pub fn connected(self) -> bool {
        self.0 & portsc::CONNECTED != 0
    }

    /// Whether the port is enabled: reset, and ready to talk.
    pub fn enabled(self) -> bool {
        self.0 & portsc::ENABLED != 0
    }

    /// Whether a reset is in progress.
    pub fn resetting(self) -> bool {
        self.0 & portsc::RESET != 0
    }

    /// Whether it has power.
    pub fn powered(self) -> bool {
        self.0 & portsc::POWER != 0
    }

    /// Whether it draws too much current.
    pub fn over_current(self) -> bool {
        self.0 & portsc::OVER_CURRENT != 0
    }

    /// Its Port Speed field, which the port's protocol gives a meaning.
    pub fn speed_id(self) -> u8 {
        ((self.0 >> portsc::SPEED_SHIFT) & 0xf) as u8
    }

    /// Its link state (PLS).
    pub fn link_state(self) -> u8 {
        ((self.0 >> portsc::LINK_SHIFT) & 0xf) as u8
    }

    /// The change bits set.
    pub fn changes(self) -> u32 {
        self.0 & portsc::CHANGES
    }

    /// Whether the connection changed.
    pub fn connect_changed(self) -> bool {
        self.0 & portsc::CONNECT_CHANGE != 0
    }

    /// Whether a reset (or a warm reset) finished.
    pub fn reset_changed(self) -> bool {
        self.0 & (portsc::RESET_CHANGE | portsc::WARM_RESET_CHANGE) != 0
    }
}

/// A controller the driver runs.
pub struct Xhci {
    regs: Mmio,
    op: Mmio,
    /// Interrupter 0's registers.
    interrupter: Mmio,
    doorbells: Mmio,
    info: Info,
    protocols: [Protocol; MAX_PROTOCOLS],
    protocol_count: usize,
    dcbaa: Dma,
    /// The scratchpad pages and the array that lists them.
    _scratchpads: Option<(Dma, Dma)>,
    commands: Ring,
    events: EventRing,
    /// Whether events were read since ERDP was last written.
    read: bool,
    /// Set when the command ring would not stop: the controller is no
    /// longer given commands.
    dead: bool,
}

impl Xhci {
    /// Takes the controller whose registers are `regs` from the firmware,
    /// resets it, sets it up with memory from `alloc` (given a number of
    /// pages and whether they must be below 4 GiB) and starts it.
    #[inline(never)]
    pub fn new(
        regs: Mmio,
        host: &mut dyn Host,
        alloc: &mut dyn FnMut(usize, bool) -> Option<Dma>,
    ) -> Result<Xhci, Error> {
        if regs.len() < 0x20 {
            return Err(Error::Unsupported("its registers are too few"));
        }
        let first = regs.read32(cap::LENGTH_VERSION);
        let length = (first & 0xff) as usize;
        let version = (first >> 16) as u16;
        let params1 = regs.read32(cap::HCSPARAMS1);
        let params2 = regs.read32(cap::HCSPARAMS2);
        let cparams1 = regs.read32(cap::HCCPARAMS1);
        let slots = params1 as u8;
        let interrupters = (params1 >> 8) & 0x7ff;
        let ports = (params1 >> 24) as u8;
        if slots == 0 || ports == 0 || interrupters == 0 {
            return Err(Error::Unsupported("it says it has no slots, ports or interrupters"));
        }
        let doorbells_at = (regs.read32(cap::DBOFF) & !3) as usize;
        let runtime_at = (regs.read32(cap::RTSOFF) & !0x1f) as usize;
        let op = regs
            .window(length, op::PORTS + op::PORT_STRIDE * usize::from(ports))
            .ok_or(Error::Unsupported("its port registers are outside its window"))?;
        let interrupter = regs
            .window(runtime_at + rt::INTERRUPTER0, 0x20)
            .ok_or(Error::Unsupported("its runtime registers are outside its window"))?;
        let doorbells = regs
            .window(doorbells_at, 4 * (usize::from(slots) + 1))
            .ok_or(Error::Unsupported("its doorbells are outside its window"))?;
        let scratchpads = (((params2 >> 21) & 0x1f) << 5 | ((params2 >> 27) & 0x1f)) as u16;
        let mut info = Info {
            version,
            ports,
            slots,
            scratchpads,
            context_size: if cparams1 & cap::CSZ != 0 { 64 } else { 32 },
            addresses_64: cparams1 & cap::AC64 != 0,
            port_power: cparams1 & cap::PPC != 0,
            handoff: Handoff::None,
            debug_capability: None,
        };
        let mut protocols: [Protocol; MAX_PROTOCOLS] = Default::default();
        let mut protocol_count = 0;
        let mut legacy = None;
        let mut at = ((cparams1 >> 16) as usize) * 4;
        // At most 64 capabilities: the list could loop only through a
        // next pointer of 0, which ends it, but stays bounded anyway.
        for _ in 0..64 {
            if at == 0 || at + 16 > regs.len() {
                break;
            }
            let head = regs.read32(at);
            match head & 0xff {
                xcap::LEGACY => legacy = Some(at),
                xcap::PROTOCOL if protocol_count < MAX_PROTOCOLS => {
                    if let Some(protocol) = read_protocol(&regs, at) {
                        protocols[protocol_count] = protocol;
                        protocol_count += 1;
                    }
                }
                xcap::DEBUG => info.debug_capability = Some(at),
                _ => {}
            }
            let next = ((head >> 8) & 0xff) as usize * 4;
            if next == 0 {
                break;
            }
            at += next;
        }
        if let Some(at) = legacy {
            info.handoff = take_from_firmware(&regs, at, host);
        }
        halt(&op, host)?;
        op.write32(op::USBCMD, op::RESET);
        // Some Intel controllers must not be read for a moment after a
        // reset starts, as Linux finds.
        sleep(host, 1_000);
        if !wait_for(host, RESET_US, || {
            op.read32(op::USBCMD) & op::RESET == 0 && op.read32(op::USBSTS) & op::NOT_READY == 0
        }) {
            return Err(Error::Unresponsive("it did not come out of its reset"));
        }
        if op.read32(op::PAGESIZE) & 1 == 0 {
            return Err(Error::Unsupported("it does not take 4 KiB pages"));
        }
        let below_4g = !info.addresses_64;
        let mut page = |pages: usize| alloc(pages, below_4g).ok_or(Error::NoMemory);
        let dcbaa = page(1)?;
        dcbaa.zero();
        let scratchpad_pages = if scratchpads > 0 {
            let count = usize::from(scratchpads);
            let array = page((count * 8).div_ceil(PAGE_SIZE))?;
            let buffers = page(count)?;
            array.zero();
            buffers.zero();
            for i in 0..count {
                array.write64(i * 8, buffers.phys() + (i * PAGE_SIZE) as u64);
            }
            dcbaa.write64(0, array.phys());
            Some((buffers, array))
        } else {
            None
        };
        let commands = Ring::new(page(1)?).ok_or(Error::NoMemory)?;
        let events = EventRing::new(page(1)?, page(1)?).ok_or(Error::NoMemory)?;
        op.write32(op::CONFIG, u32::from(slots));
        write64(&op, op::DCBAAP, dcbaa.phys());
        write64(&op, op::CRCR, commands.dequeue_pointer());
        interrupter.write32(rt::ERSTSZ, 1);
        write64(&interrupter, rt::ERDP, events.dequeue_phys());
        // Last: writing ERSTBA makes the controller read the table.
        write64(&interrupter, rt::ERSTBA, events.table_phys());
        interrupter.write32(rt::IMOD, rt::MODERATION);
        interrupter.write32(rt::IMAN, rt::IP | rt::IE);
        op.write32(op::USBSTS, op::HSE | op::EINT | op::PCD);
        op.write32(op::USBCMD, op::RUN | op::INTE | op::HSEE);
        if !wait_for(host, START_US, || op.read32(op::USBSTS) & op::HALTED == 0) {
            return Err(Error::Unresponsive("it did not start"));
        }
        let xhci = Xhci {
            regs,
            op,
            interrupter,
            doorbells,
            info,
            protocols,
            protocol_count,
            dcbaa,
            _scratchpads: scratchpad_pages,
            commands,
            events,
            read: false,
            dead: false,
        };
        if xhci.info.port_power {
            let mut powered = false;
            for port in 1..=ports {
                if !xhci.port_status(port).powered() {
                    xhci.power_port(port);
                    powered = true;
                }
            }
            if powered {
                sleep(host, POWER_US);
            }
        }
        Ok(xhci)
    }

    /// What it is.
    pub fn info(&self) -> &Info {
        &self.info
    }

    /// Its registers, for its Debug Capability.
    pub fn registers(&self) -> &Mmio {
        &self.regs
    }

    /// Its Supported Protocol capabilities.
    pub fn protocols(&self) -> &[Protocol] {
        &self.protocols[..self.protocol_count]
    }

    /// The protocol of root port `port`.
    pub fn port_protocol(&self, port: u8) -> Option<&Protocol> {
        self.protocols().iter().find(|p| p.covers(port))
    }

    /// Whether its memory must be below 4 GiB.
    pub fn below_4g(&self) -> bool {
        !self.info.addresses_64
    }

    /// Whether it has stopped: halted by itself after a host system error
    /// or a controller error, or no longer taking commands.
    pub fn failed(&self) -> bool {
        self.dead || self.op.read32(op::USBSTS) & (op::HSE | op::HCE | op::HALTED) != 0
    }

    fn port_offset(port: u8) -> usize {
        op::PORTS + op::PORT_STRIDE * usize::from(port.saturating_sub(1))
    }

    fn valid_port(&self, port: u8) -> bool {
        (1..=self.info.ports).contains(&port)
    }

    /// Root port `port`'s status (from 1); empty for a port it does not
    /// have.
    pub fn port_status(&self, port: u8) -> PortStatus {
        if !self.valid_port(port) {
            return PortStatus(0);
        }
        PortStatus(self.op.read32(Self::port_offset(port)))
    }

    /// Writes `bits` to root port `port`'s register, with what must be
    /// kept as `status` has it.
    fn write_port(&self, port: u8, status: PortStatus, bits: u32) {
        if self.valid_port(port) {
            self.op.write32(Self::port_offset(port), (status.0 & portsc::PRESERVE) | bits);
        }
    }

    /// Acknowledges the changes `status` shows, so that the port raises an
    /// event for the next.
    pub fn acknowledge_port(&self, port: u8, status: PortStatus) {
        if status.changes() != 0 {
            self.write_port(port, status, status.changes());
        }
    }

    /// Starts a reset of root port `port`: a warm one for a USB 3 port
    /// whose link would not come up. It ends with the port's reset change
    /// bit set and, if a device is there, the port enabled.
    pub fn reset_port(&self, port: u8, warm: bool) {
        let status = self.port_status(port);
        self.write_port(port, status, if warm { portsc::WARM_RESET } else { portsc::RESET });
    }

    /// Powers root port `port`.
    pub fn power_port(&self, port: u8) {
        let status = self.port_status(port);
        self.write_port(port, status, portsc::POWER);
    }

    /// The speed of the device on root port `port`, from its status.
    pub fn port_speed(&self, port: u8, status: PortStatus) -> Option<Speed> {
        self.port_protocol(port)?.speed(status.speed_id())
    }

    /// Rings doorbell `index` (0 for the controller, a slot for a device)
    /// for `target` (0 for the command ring, an endpoint's device context
    /// index for a device).
    pub fn ring(&self, index: u8, target: u32) {
        self.doorbells.write32(4 * usize::from(index), target);
    }

    /// Points slot `slot`'s entry of the Device Context Base Address Array
    /// at a device context (0 for none).
    fn set_device_context(&self, slot: u8, phys: u64) {
        if (1..=self.info.slots).contains(&slot) {
            self.dcbaa.write64(usize::from(slot) * 8, phys);
        }
    }

    /// The next event the controller wrote, if any.
    pub fn next_event(&mut self) -> Option<Event> {
        let trb = self.events.pop()?;
        self.read = true;
        Some(Event::decode(trb))
    }

    /// Tells the controller how far the events have been read, and that
    /// the driver is done with them for now, so that it interrupts again
    /// for the next (it does at once if more came meanwhile).
    pub fn events_read(&mut self) {
        // The segment is one page, so the high half, written at the start,
        // never changes.
        self.read = false;
        self.interrupter.write32(rt::ERDP, self.events.dequeue_phys() as u32 | rt::EHB);
    }

    /// Gives the controller `command` and waits for it to finish; the
    /// events that come meanwhile go to `host`. A command that takes too
    /// long is aborted.
    pub fn command(&mut self, command: Command, host: &mut dyn Host) -> Result<Completed, Error> {
        if self.dead {
            return Err(Error::Unresponsive("it stopped taking commands"));
        }
        let trb = self.commands.push(command.trb());
        self.ring(0, 0);
        let deadline = host.now_us().saturating_add(COMMAND_US);
        loop {
            while let Some(event) = self.next_event() {
                match event {
                    Event::Command { trb: at, code, slot, parameter } if at == trb => {
                        self.events_read();
                        return if code == code::SUCCESS {
                            Ok(Completed { slot, parameter })
                        } else {
                            Err(Error::Completion(code))
                        };
                    }
                    // A command given up on earlier, or the ring stopping.
                    Event::Command { .. } => {}
                    other => host.other(other),
                }
            }
            self.events_read();
            if host.now_us() >= deadline {
                self.abort(host);
                return Err(Error::TimedOut);
            }
            host.wait(POLL_US);
        }
    }

    /// Stops the command ring after a command took too long, and points it
    /// past what was given it; if it will not stop, gives no commands
    /// again.
    fn abort(&mut self, host: &mut dyn Host) {
        let pointer = self.commands.dequeue_pointer();
        self.op.write32(op::CRCR, (pointer as u32 & !0x3f) | op::CRCR_ABORT | (pointer as u32 & 1));
        self.op.write32(op::CRCR + 4, (pointer >> 32) as u32);
        let op = self.op.clone();
        if !wait_for(host, ABORT_US, || op.read32(op::CRCR) & op::CRCR_RUNNING == 0) {
            self.dead = true;
            return;
        }
        while let Some(event) = self.next_event() {
            if !matches!(event, Event::Command { .. }) {
                host.other(event);
            }
        }
        self.events_read();
        write64(&self.op, op::CRCR, self.commands.dequeue_pointer());
    }

    /// Halts the controller, before the machine powers off or restarts, so
    /// that it stops using memory.
    pub fn halt(&mut self, host: &mut dyn Host) -> Result<(), Error> {
        self.interrupter.write32(rt::IMAN, rt::IP);
        halt(&self.op, host)
    }
}

/// Stops the controller whose registers are `regs` at once, from outside
/// the thread that runs it: before the machine powers off or restarts, so
/// that it stops using memory. Waits for it to halt for up to 20 ms with
/// `delay` (given microseconds); returns whether it did.
pub fn stop(regs: &Mmio, delay: &mut dyn FnMut(u32)) -> bool {
    let length = (regs.read32(cap::LENGTH_VERSION) & 0xff) as usize;
    let Some(op) = regs.window(length, op::USBSTS + 4) else { return false };
    op.write32(op::USBCMD, op.read32(op::USBCMD) & !(op::RUN | op::INTE | op::HSEE));
    for _ in 0..200 {
        if op.read32(op::USBSTS) & op::HALTED != 0 {
            return true;
        }
        delay(100);
    }
    false
}

/// Writes a 64-bit register as two 32-bit halves, low first, as every
/// controller takes them.
fn write64(regs: &Mmio, offset: usize, value: u64) {
    regs.write32(offset, value as u32);
    regs.write32(offset + 4, (value >> 32) as u32);
}

/// Clears USBCMD's run bit and waits for the controller to halt.
fn halt(op: &Mmio, host: &mut dyn Host) -> Result<(), Error> {
    if op.read32(op::USBSTS) & op::HALTED != 0 {
        return Ok(());
    }
    op.write32(op::USBCMD, op.read32(op::USBCMD) & !(op::RUN | op::INTE | op::HSEE));
    if wait_for(host, HALT_US, || op.read32(op::USBSTS) & op::HALTED != 0) {
        Ok(())
    } else {
        Err(Error::Unresponsive("it did not halt"))
    }
}

/// Reads the Supported Protocol capability at `at`.
fn read_protocol(regs: &Mmio, at: usize) -> Option<Protocol> {
    let head = regs.read32(at);
    let ports = regs.read32(at + 8);
    let slot_type = regs.read32(at + 12);
    let count = ((ports >> 28) & 0xf) as usize;
    let protocol = Protocol {
        major: (head >> 24) as u8,
        minor: (head >> 16) as u8,
        first_port: ports as u8,
        ports: (ports >> 8) as u8,
        slot_type: (slot_type & 0x1f) as u8,
        psi: if count > 0 { regs.window(at + 16, 4 * count) } else { None },
    };
    (protocol.first_port != 0 && matches!(protocol.major, 2 | 3)).then_some(protocol)
}

/// Asks the firmware to let go of the controller through its USB Legacy
/// Support capability at `at`, overruling it if it does not in time, and
/// turns its SMIs off.
fn take_from_firmware(regs: &Mmio, at: usize, host: &mut dyn Host) -> Handoff {
    let legacy = regs.read32(at);
    regs.write32(at, legacy | xcap::OS_OWNED);
    let handoff = if legacy & xcap::BIOS_OWNED == 0 {
        Handoff::Free
    } else if wait_for(host, HANDOFF_US, || regs.read32(at) & xcap::BIOS_OWNED == 0) {
        Handoff::Given
    } else {
        regs.write32(at, (regs.read32(at) & !xcap::BIOS_OWNED) | xcap::OS_OWNED);
        Handoff::Taken
    };
    let control = regs.read32(at + 4);
    regs.write32(at + 4, (control & xcap::SMI_KEEP) | xcap::SMI_EVENTS);
    handoff
}
