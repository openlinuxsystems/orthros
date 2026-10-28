// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A device in a slot, and its default control pipe.
//!
//! [`Device::address`] asks the controller for a slot (Enable Slot),
//! gives it an output device context in the Device Context Base Address
//! Array, and an input context that says where the device is (its root
//! port and route), how fast it is, and endpoint 0's transfer ring and
//! packet size; Address Device then sets the slot up and sends the device
//! SET_ADDRESS (4.3.3, 4.3.4). The packet size is the speed's default
//! until the device's descriptor says its own, which
//! [`Device::set_max_packet`] gives the controller (Evaluate Context).
//!
//! A control transfer (4.11.2.2) is three TDs on endpoint 0's ring: the
//! Setup Stage, with the setup packet in the TRB itself; the Data Stage,
//! if there is data, through the device's buffer page, interrupting on a
//! short packet so that the driver learns how much came; and the Status
//! Stage, which interrupts when done. An error halts the endpoint: the
//! driver resets it and points it past the transfer (Reset Endpoint, Set
//! TR Dequeue Pointer). A transfer that takes too long is stopped (Stop
//! Endpoint) and skipped the same way, so the pipe works for the next.

use radix_hw::{Dma, PAGE_SIZE};
use radix_usb::{Setup, Speed};

use crate::ring::{Ring, Trb, control, kind};
use crate::{Command, Error, Event, Host, POLL_US, Xhci, code};

/// The most bytes a control transfer moves: one page.
pub const BUFFER_SIZE: usize = PAGE_SIZE;

/// Where the output context, the input context and the control buffer
/// are in a device's memory, a page each.
const OUTPUT: usize = 0;
const INPUT: usize = PAGE_SIZE;
const BUFFER: usize = 2 * PAGE_SIZE;
const PAGES: usize = 3;

/// How long a control transfer may take, as Linux allows.
const CONTROL_US: u64 = 5_000_000;

/// The input control context's add flags for the slot context and
/// endpoint 0.
const ADD_SLOT: u32 = 1 << 0;
const ADD_EP0: u32 = 1 << 1;
/// Endpoint 0's device context index, which its doorbell takes.
const EP0: u8 = 1;
/// An endpoint context's type field for a control endpoint, and three
/// retries on errors (CErr).
const CONTROL_ENDPOINT: u32 = 4 << 3;
const RETRIES: u32 = 3 << 1;

/// Where a device is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    /// The root port it is on, or its hubs are on, from 1.
    pub root_port: u8,
    /// The route string: the port on each hub after the root port, a
    /// nibble each, the first hub's lowest; 0 for a device on a root port.
    pub route: u32,
    /// Its speed.
    pub speed: Speed,
    /// The Port Speed ID its speed has on its root port's protocol.
    pub speed_id: u8,
}

/// A device the controller has a slot for.
pub struct Device {
    slot: u8,
    place: Place,
    context_size: usize,
    /// The output context, the input context and the control buffer.
    memory: Dma,
    ep0: Ring,
    max_packet: u16,
}

impl Device {
    /// Gives the device at `place` a slot and its address, with memory from
    /// `alloc`.
    pub fn address(
        xhci: &mut Xhci,
        host: &mut dyn Host,
        alloc: &mut dyn FnMut(usize, bool) -> Option<Dma>,
        place: Place,
    ) -> Result<Device, Error> {
        let slot_type = xhci.port_protocol(place.root_port).map_or(0, |p| p.slot_type);
        let slot = xhci.command(Command::EnableSlot { slot_type }, host)?.slot;
        if !(1..=xhci.info().slots).contains(&slot) {
            return Err(Error::Unsupported("it gave a slot it does not have"));
        }
        let below_4g = xhci.below_4g();
        let memory = alloc(PAGES, below_4g);
        let ring = alloc(1, below_4g).and_then(Ring::new);
        let (Some(memory), Some(ep0)) = (memory, ring) else {
            let _ = xhci.command(Command::DisableSlot { slot }, host);
            return Err(Error::NoMemory);
        };
        memory.zero();
        let device = Device {
            slot,
            place,
            context_size: xhci.info().context_size,
            memory,
            ep0,
            max_packet: place.speed.default_max_packet(),
        };
        xhci.set_device_context(slot, device.memory.phys() + OUTPUT as u64);
        device.clear_input();
        device.memory.write32(INPUT + 4, ADD_SLOT | ADD_EP0);
        device.write_slot_context(1);
        device.write_ep0_context();
        let input = device.memory.phys() + INPUT as u64;
        match xhci.command(Command::AddressDevice { slot, input, block: false }, host) {
            Ok(_) => Ok(device),
            Err(err) => {
                let _ = device.disable(xhci, host);
                Err(err)
            }
        }
    }

    /// Its slot.
    pub fn slot(&self) -> u8 {
        self.slot
    }

    /// Where it is.
    pub fn place(&self) -> Place {
        self.place
    }

    /// The USB address the controller gave it, from its output slot
    /// context.
    pub fn usb_address(&self) -> u8 {
        self.memory.read32(OUTPUT + 12) as u8
    }

    /// Endpoint 0's packet size.
    pub fn max_packet(&self) -> u16 {
        self.max_packet
    }

    /// Where context `index` of the input context is: 0 is the input
    /// control context, 1 the slot's, 2 endpoint 0's.
    fn input(&self, index: usize) -> usize {
        INPUT + index * self.context_size
    }

    /// Zeroes the input context.
    fn clear_input(&self) {
        for offset in (INPUT..INPUT + 33 * self.context_size).step_by(8) {
            self.memory.write64(offset, 0);
        }
    }

    /// Fills the input slot context: where the device is, its speed, and
    /// how many endpoint contexts are valid (`entries`, the last one's
    /// device context index).
    fn write_slot_context(&self, entries: u8) {
        let at = self.input(1);
        let place = self.place;
        self.memory.write32(
            at,
            (place.route & 0xf_ffff)
                | u32::from(place.speed_id & 0xf) << 20
                | u32::from(entries & 0x1f) << 27,
        );
        self.memory.write32(at + 4, u32::from(place.root_port) << 16);
    }

    /// Fills the input context for endpoint 0: a control endpoint with its
    /// packet size and its ring.
    fn write_ep0_context(&self) {
        let at = self.input(2);
        self.memory.write32(at, 0);
        self.memory
            .write32(at + 4, RETRIES | CONTROL_ENDPOINT | u32::from(self.max_packet) << 16);
        self.memory.write64(at + 8, self.ep0.dequeue_pointer());
        // The average TRB length: 8, a setup packet, as the spec suggests.
        self.memory.write32(at + 16, 8);
    }

    /// Gives endpoint 0 the packet size the device's descriptor says.
    pub fn set_max_packet(
        &mut self,
        xhci: &mut Xhci,
        host: &mut dyn Host,
        size: u16,
    ) -> Result<(), Error> {
        if size == self.max_packet {
            return Ok(());
        }
        self.max_packet = size;
        self.clear_input();
        self.memory.write32(INPUT + 4, ADD_EP0);
        self.write_ep0_context();
        let input = self.memory.phys() + INPUT as u64;
        xhci.command(Command::EvaluateContext { slot: self.slot, input }, host).map(|_| ())
    }

    /// A control transfer that reads up to `setup.length` bytes into
    /// `out`; returns how many came.
    pub fn control_in(
        &mut self,
        xhci: &mut Xhci,
        host: &mut dyn Host,
        setup: Setup,
        out: &mut [u8],
    ) -> Result<usize, Error> {
        if !setup.device_to_host() || usize::from(setup.length) > out.len() {
            return Err(Error::BadRequest);
        }
        let came = self.control(xhci, host, setup)?;
        self.memory.read(BUFFER, &mut out[..came]);
        Ok(came)
    }

    /// A control transfer that sends `data` (as many bytes as
    /// `setup.length` says, from its start), or nothing.
    pub fn control_out(
        &mut self,
        xhci: &mut Xhci,
        host: &mut dyn Host,
        setup: Setup,
        data: &[u8],
    ) -> Result<(), Error> {
        let length = usize::from(setup.length);
        if setup.device_to_host() || length > data.len() {
            return Err(Error::BadRequest);
        }
        self.memory.write(BUFFER, &data[..length]);
        self.control(xhci, host, setup).map(|_| ())
    }

    /// Makes the control transfer `setup`, whose data (if any) is in the
    /// buffer; returns how many bytes moved.
    fn control(&mut self, xhci: &mut Xhci, host: &mut dyn Host, setup: Setup) -> Result<usize, Error> {
        let length = usize::from(setup.length);
        if length > BUFFER_SIZE {
            return Err(Error::BadRequest);
        }
        let into_host = setup.device_to_host();
        let transfer_type = match (length, into_host) {
            (0, _) => control::TRT_NONE,
            (_, true) => control::TRT_IN,
            (_, false) => control::TRT_OUT,
        };
        let setup_at = self.ep0.push(Trb {
            parameter: setup.to_u64(),
            status: 8,
            control: control::kind(kind::SETUP_STAGE) | control::IDT | transfer_type,
        });
        let data_at = (length > 0).then(|| {
            self.ep0.push(Trb {
                parameter: self.memory.phys() + BUFFER as u64,
                status: length as u32,
                control: control::kind(kind::DATA_STAGE)
                    | if into_host { control::DIR_IN | control::ISP } else { 0 },
            })
        });
        // The status stage goes the other way from the data, or IN if
        // there is none.
        let status_in = length == 0 || !into_host;
        let status_at = self.ep0.push(Trb {
            parameter: 0,
            status: 0,
            control: control::kind(kind::STATUS_STAGE)
                | control::IOC
                | if status_in { control::DIR_IN } else { 0 },
        });
        xhci.ring(self.slot, u32::from(EP0));
        let mut residual = 0;
        let deadline = host.now_us().saturating_add(CONTROL_US);
        loop {
            while let Some(event) = xhci.next_event() {
                let Event::Transfer { trb, code, slot, endpoint, residual: left } = event else {
                    host.other(event);
                    continue;
                };
                if slot != self.slot || endpoint != EP0 {
                    host.other(event);
                    continue;
                }
                // An event for an earlier transfer, stopped or failed.
                if trb != setup_at && Some(trb) != data_at && trb != status_at {
                    continue;
                }
                match code {
                    code::SHORT_PACKET if Some(trb) == data_at => residual = left,
                    code::SUCCESS | code::SHORT_PACKET if trb == status_at => {
                        xhci.events_read();
                        return Ok(length.saturating_sub(residual as usize));
                    }
                    code::SUCCESS => {}
                    failed => {
                        xhci.events_read();
                        self.skip(xhci, host, failed)?;
                        return Err(Error::Completion(failed));
                    }
                }
            }
            xhci.events_read();
            if host.now_us() >= deadline {
                self.skip(xhci, host, code::STOPPED)?;
                return Err(Error::TimedOut);
            }
            host.wait(POLL_US);
        }
    }

    /// Brings endpoint 0 back after a transfer that failed with `code`
    /// (halting it) or took too long ([`code::STOPPED`]): resets or stops
    /// it, then points it past the transfer.
    fn skip(&mut self, xhci: &mut Xhci, host: &mut dyn Host, code: u8) -> Result<(), Error> {
        let slot = self.slot;
        let tolerate = |result: Result<_, Error>| match result {
            // Not in the state the command expects: already halted, or
            // already stopped.
            Err(Error::Completion(code::CONTEXT_STATE)) | Ok(_) => Ok(()),
            Err(err) => Err(err),
        };
        if code == code::STOPPED {
            // Stopped, unless it halted meanwhile, which Reset Endpoint
            // ends.
            if let Err(Error::Completion(code::CONTEXT_STATE)) =
                xhci.command(Command::StopEndpoint { slot, endpoint: EP0 }, host)
            {
                tolerate(xhci.command(Command::ResetEndpoint { slot, endpoint: EP0 }, host))?;
            }
        } else {
            tolerate(xhci.command(Command::ResetEndpoint { slot, endpoint: EP0 }, host))?;
        }
        let pointer = self.ep0.dequeue_pointer();
        let dequeue = Command::SetDequeue { slot, endpoint: EP0, pointer };
        match xhci.command(dequeue, host) {
            // Running after all: stop it, and move it then.
            Err(Error::Completion(code::CONTEXT_STATE)) => {
                tolerate(xhci.command(Command::StopEndpoint { slot, endpoint: EP0 }, host))?;
                xhci.command(dequeue, host).map(|_| ())
            }
            other => other.map(|_| ()),
        }
    }

    /// Gives the slot back (Disable Slot). If the controller does not
    /// answer, the device's memory is never given back, since the
    /// controller may still use it.
    pub fn disable(self, xhci: &mut Xhci, host: &mut dyn Host) -> Result<(), Error> {
        let result = xhci.command(Command::DisableSlot { slot: self.slot }, host);
        xhci.set_device_context(self.slot, 0);
        match result {
            Ok(_) | Err(Error::Completion(code::SLOT_NOT_ENABLED)) => Ok(()),
            Err(err) => {
                core::mem::forget(self);
                Err(err)
            }
        }
    }
}
