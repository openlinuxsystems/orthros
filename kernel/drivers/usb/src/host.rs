// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the kernel asks of the USB 2 and USB 1.1 host controllers, EHCI,
//! OHCI and UHCI, whose devices it sets up itself (Phase 11g, ADR 0243).
//!
//! Unlike an xHCI controller, which gives each device a slot and its
//! address and keeps its endpoints' state, these controllers only carry
//! transfers: the kernel gives each device its address (SET_ADDRESS), and
//! tells the controller, with every pipe it opens, where the device is,
//! how fast it talks, the largest packet its endpoint takes, and, for a
//! low- or full-speed device behind a high-speed hub, which hub port's
//! Transaction Translator reaches it ([`Target`]). Each controller's
//! driver implements [`Hcd`], and one kernel thread runs it.
//!
//! - Root ports: [`Hcd::port`] reads one, [`Hcd::acknowledge`] clears
//!   what changed, and [`Hcd::reset`] resets it and says how fast the
//!   device on it is, or that it went to the controller's companion (an
//!   EHCI controller hands its low- and full-speed devices to an OHCI or
//!   UHCI controller beside it).
//! - Control transfers on a device's default pipe ([`Hcd::control`])
//!   are synchronous: the driver waits for each.
//! - Interrupt and bulk pipes are opened ([`Hcd::open`]) and their
//!   transfers started ([`Hcd::start`]) and looked at ([`Hcd::poll`])
//!   without waiting. An interrupt transfer lands in the pipe's own
//!   buffer, which [`Hcd::received`] copies out.
//!
//! Nothing here sleeps itself: the kernel passes in a [`Waiter`].

use core::fmt;

use crate::{Setup, Speed, TransferType};

/// Waits and keeps the time, for a driver that waits for its controller.
pub trait Waiter {
    /// Waits for about `us` microseconds.
    fn wait(&mut self, us: u32);
    /// A clock, in microseconds.
    fn now_us(&mut self) -> u64;
}

/// The Transaction Translator that carries a low- or full-speed device's
/// transfers from a high-speed bus: the address of the high-speed hub it
/// is in, and the hub's port that leads to the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tt {
    /// The hub's address.
    pub hub: u8,
    /// Its port, from 1.
    pub port: u8,
}

/// Where a device's transfers go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// Its address, 0 until SET_ADDRESS.
    pub address: u8,
    /// How fast it talks.
    pub speed: Speed,
    /// For a low- or full-speed device behind a high-speed hub, the
    /// Transaction Translator that reaches it.
    pub tt: Option<Tt>,
}

/// An endpoint a pipe is opened to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The device.
    pub target: Target,
    /// The endpoint's number, 1 to 15.
    pub number: u8,
    /// Whether its data goes to the host.
    pub into_host: bool,
    /// Interrupt or bulk.
    pub kind: TransferType,
    /// wMaxPacketSize's packet size (bits 0 to 10).
    pub max_packet: u16,
    /// The transactions a high-speed interrupt endpoint makes in a
    /// microframe, 1 to 3 (1 more than wMaxPacketSize's bits 11 and 12);
    /// 1 for any other.
    pub transactions: u8,
    /// bInterval as the descriptor says it: frames at low and full speed,
    /// 2^(bInterval - 1) microframes at high speed.
    pub interval: u8,
}

impl Endpoint {
    /// How often an interrupt endpoint is polled, in microframes (an
    /// eighth of a millisecond), as its bInterval asks, at most every
    /// 32 ms: a power of two, which the controllers' schedules take.
    pub fn period_microframes(&self) -> u32 {
        let asked = if self.target.speed == Speed::High {
            1u32 << (self.interval.clamp(1, 16) - 1)
        } else {
            // Frames: the largest power of two not above it.
            let frames = u32::from(self.interval.max(1));
            8 << frames.ilog2()
        };
        asked.min(32 * 8)
    }
}

/// A root port, as [`Hcd::port`] reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Port {
    /// A device is plugged in.
    pub connected: bool,
    /// The port is enabled: its device has been reset, and is this
    /// controller's.
    pub enabled: bool,
    /// The connection changed since it was last acknowledged: the device
    /// may be another.
    pub connect_changed: bool,
    /// Something else changed (enabled, over-current, reset, resume).
    pub other_changed: bool,
    /// The port draws too much current.
    pub over_current: bool,
    /// The device on it went to the controller's companion.
    pub companion: bool,
}

/// How a transfer or a request to the controller failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The device stalled: it refuses the request, or its endpoint is
    /// halted.
    Stall,
    /// The device did not answer properly, three times: no handshake, a
    /// bad CRC or a bit-stuffing error.
    NoAnswer,
    /// The device sent more than the packet allows.
    Babble,
    /// The controller could not move the data to or from memory in time.
    Buffer,
    /// It did not finish in time.
    TimedOut,
    /// The device was unplugged, or its controller stopped.
    Gone,
    /// There was no memory or no room in the controller's structures.
    NoMemory,
    /// The driver cannot make the request: too long, or of a kind it
    /// does not carry.
    BadRequest,
    /// The device is a low- or full-speed one, which this controller hands
    /// to a companion controller it does not have.
    NoCompanion,
    /// The controller halted with an error.
    Halted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Stall => "the device stalled",
            Error::NoAnswer => "the device did not answer",
            Error::Babble => "the device babbled",
            Error::Buffer => "the controller fell behind",
            Error::TimedOut => "no answer in time",
            Error::Gone => "it was unplugged",
            Error::NoMemory => "out of memory",
            Error::BadRequest => "a request the controller cannot carry",
            Error::NoCompanion => {
                "it is a low- or full-speed device, and the controller has no companion for it"
            }
            Error::Halted => "the controller stopped with an error",
        })
    }
}

/// A pipe a driver opened, by its number.
pub type PipeId = u8;

/// The data of a transfer started on a pipe.
#[derive(Clone, Copy, Debug)]
pub enum Data<'a> {
    /// Into the pipe's own buffer: this many bytes at most (an interrupt
    /// IN pipe).
    In(usize),
    /// From the pipe's own buffer, which these bytes are copied into.
    Out(&'a [u8]),
    /// Straight to or from these buffers of memory the device reaches
    /// (physical address, length), in order: a bulk transfer.
    Frames(&'a [(u64, u32)]),
}

/// A USB 2 or USB 1.1 host controller, as its thread drives it.
pub trait Hcd {
    /// Its name: `EHCI`, `OHCI` or `UHCI`.
    fn kind(&self) -> &'static str;

    /// How many root ports it has, numbered from 1.
    fn ports(&self) -> u8;

    /// Reads root port `port`.
    fn port(&mut self, port: u8) -> Port;

    /// Clears what changed on root port `port`.
    fn acknowledge(&mut self, port: u8);

    /// Resets root port `port` and enables it; returns how fast the device
    /// on it talks, or `None` if it went to the companion controller
    /// instead.
    fn reset(&mut self, port: u8, waiter: &mut dyn Waiter) -> Result<Option<Speed>, Error>;

    /// Disables root port `port`.
    fn disable(&mut self, port: u8);

    /// Makes the control transfer `setup` on the default pipe of `target`,
    /// whose packets are `max_packet` bytes, with its data in or from
    /// `data`; returns how many bytes moved. Waits at most `timeout_ms`.
    fn control(
        &mut self,
        target: Target,
        max_packet: u16,
        setup: Setup,
        data: &mut [u8],
        timeout_ms: u32,
        waiter: &mut dyn Waiter,
    ) -> Result<usize, Error>;

    /// Opens a pipe to an interrupt or bulk endpoint, its own buffer
    /// `buffer` bytes long (for [`Data::In`] and [`Data::Out`]).
    fn open(&mut self, endpoint: Endpoint, buffer: usize) -> Result<PipeId, Error>;

    /// Starts a transfer on `pipe`, which has none in flight.
    fn start(&mut self, pipe: PipeId, data: Data<'_>) -> Result<(), Error>;

    /// How the transfer on `pipe` ended, with the bytes that moved; `None`
    /// while it is in flight, or if none was started.
    fn poll(&mut self, pipe: PipeId) -> Option<Result<usize, Error>>;

    /// Copies what the last transfer on `pipe` brought into its own buffer
    /// to `out`.
    fn received(&self, pipe: PipeId, out: &mut [u8]);

    /// Stops the transfer in flight on `pipe`, if any: it ends with nothing
    /// more moved, and [`Hcd::poll`] says nothing of it.
    fn cancel(&mut self, pipe: PipeId, waiter: &mut dyn Waiter);

    /// Clears `pipe`'s halt and starts its data toggle over at DATA0, as
    /// after CLEAR_FEATURE(ENDPOINT_HALT).
    fn reset_pipe(&mut self, pipe: PipeId);

    /// Closes `pipe`, stopping its transfer.
    fn close(&mut self, pipe: PipeId, waiter: &mut dyn Waiter);

    /// The device at `address` is gone, or has another address now: the
    /// driver lets go of what it keeps for its default pipe.
    fn forget(&mut self, address: u8, waiter: &mut dyn Waiter);

    /// Whether the controller stopped with an error, or stopped answering.
    fn failed(&mut self) -> bool;

    /// Halts the controller, so that it reads and writes no more memory.
    fn halt(&mut self, waiter: &mut dyn Waiter);
}
