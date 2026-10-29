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
//!
//! [`Device::configure`] gives the controller the device's other
//! endpoints (Configure Endpoint, 4.6.6), each with a transfer ring and a
//! page for its data, then sends the device SET_CONFIGURATION (ADR 0147).
//! A transfer on one of them, [`Device::submit_in`] or
//! [`Device::submit_out`], is one Normal TRB that interrupts when done:
//! the driver does not wait for it, and its event comes with the others.
//! A hub's slot says it is a hub, and how many ports it has
//! ([`Device::set_hub`]); a low- or full-speed device behind a high-speed
//! hub names the hub's slot and port, whose Transaction Translator talks
//! to it ([`Place`]).

use radix_hw::{Dma, PAGE_SIZE};
use radix_usb::{EndpointDescriptor, Setup, Speed, TransferType};

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
/// The slot context's Hub and Multi-TT flags (6.2.2).
const SLOT_HUB: u32 = 1 << 26;
const SLOT_MULTI_TT: u32 = 1 << 25;

/// The most endpoints a device is given besides endpoint 0.
pub const MAX_ENDPOINTS: usize = 8;

/// The device context index of the endpoint at `address`
/// (bEndpointAddress): twice its number, plus one for IN (4.5.1);
/// endpoint 0's is 1.
pub fn endpoint_index(address: u8) -> u8 {
    let number = address & 0x0f;
    if number == 0 { EP0 } else { number * 2 + u8::from(address & 0x80 != 0) }
}

/// The xHCI Interval of an endpoint at `speed` (6.2.3.6): the time
/// between its service opportunities, as 125 µs times 2 to its power.
/// bInterval is frames (1 ms) for low- and full-speed interrupt
/// endpoints, and a power of two of frames or microframes for the rest
/// of the periodic ones; bulk and control endpoints have none.
pub fn interval(endpoint: &EndpointDescriptor, speed: Speed) -> u8 {
    let b = endpoint.interval;
    match (endpoint.transfer_type(), speed) {
        (TransferType::Bulk | TransferType::Control, _) => 0,
        (TransferType::Interrupt, Speed::Low | Speed::Full) => {
            // floor(log2(bInterval)) frames of 8 microframes.
            (b.max(1).ilog2() as u8 + 3).clamp(3, 10)
        }
        (TransferType::Isochronous, Speed::Low | Speed::Full) => b.clamp(1, 16) + 2,
        _ => b.clamp(1, 16) - 1,
    }
}

/// What a hub's slot says of it (6.2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubSlot {
    /// How many downstream ports it has.
    pub ports: u8,
    /// A high-speed hub's TT think time, 0 to 3.
    pub think_time: u8,
    /// A high-speed hub with a Transaction Translator per port, in the
    /// interface setting that uses them.
    pub multi_tt: bool,
}

/// An endpoint the device was configured with.
struct Endpoint {
    /// Its device context index.
    index: u8,
    /// Its ring, in the first page of its memory; the second page holds
    /// its transfers' data.
    ring: Ring,
}

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
    /// For a low- or full-speed device behind a high-speed hub: the slot
    /// of the hub whose Transaction Translator talks to it, and the port
    /// it is on (or the port of the hub it is behind); 0 otherwise.
    pub tt_slot: u8,
    /// See `tt_slot`.
    pub tt_port: u8,
}

impl Place {
    /// A device on root port `root_port`.
    pub fn root(root_port: u8, speed: Speed, speed_id: u8) -> Place {
        Place { root_port, route: 0, speed, speed_id, tt_slot: 0, tt_port: 0 }
    }
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
    /// The endpoints it was configured with.
    endpoints: [Option<Endpoint>; MAX_ENDPOINTS],
    /// The last valid endpoint context's index (Context Entries).
    entries: u8,
    /// What its slot says if it is a hub.
    hub: Option<HubSlot>,
}

impl Device {
    /// Gives the device at `place` a slot and its address, with memory from
    /// `alloc`.
    #[inline(never)]
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
            endpoints: [const { None }; MAX_ENDPOINTS],
            entries: EP0,
            hub: None,
        };
        xhci.set_device_context(slot, device.memory.phys() + OUTPUT as u64);
        device.clear_input();
        device.memory.write32(INPUT + 4, ADD_SLOT | ADD_EP0);
        device.write_slot_context();
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

    /// Fills the input slot context: where the device is, its speed, how
    /// many endpoint contexts are valid (the last one's device context
    /// index), what a hub has, and which Transaction Translator a slow
    /// device behind a hub uses.
    fn write_slot_context(&self) {
        let at = self.input(1);
        let place = self.place;
        let hub = self.hub.map_or(0, |hub| SLOT_HUB | if hub.multi_tt { SLOT_MULTI_TT } else { 0 });
        self.memory.write32(
            at,
            (place.route & 0xf_ffff)
                | u32::from(place.speed_id & 0xf) << 20
                | hub
                | u32::from(self.entries & 0x1f) << 27,
        );
        let ports = self.hub.map_or(0, |hub| hub.ports);
        self.memory.write32(at + 4, u32::from(place.root_port) << 16 | u32::from(ports) << 24);
        let think_time = self.hub.map_or(0, |hub| hub.think_time & 3);
        self.memory.write32(
            at + 8,
            u32::from(place.tt_slot) | u32::from(place.tt_port) << 8 | u32::from(think_time) << 16,
        );
    }

    /// Fills the input context of endpoint `index`, described by
    /// `endpoint`, with its ring.
    fn write_endpoint_context(&self, index: u8, endpoint: EndpointDescriptor, ring: &Ring) {
        let at = self.input(usize::from(index) + 1);
        let speed = self.place.speed;
        let max_packet = u32::from(endpoint.max_packet());
        // Packets in a burst: a SuperSpeed endpoint's companion says; a
        // high-speed periodic endpoint's extra transactions are its burst.
        let burst = if speed.is_super() {
            u32::from(endpoint.max_burst.min(15))
        } else if speed == Speed::High
            && matches!(
                endpoint.transfer_type(),
                TransferType::Interrupt | TransferType::Isochronous
            )
        {
            u32::from(endpoint.extra_transactions().min(2))
        } else {
            0
        };
        let (kind, periodic) = match (endpoint.transfer_type(), endpoint.is_in()) {
            (TransferType::Isochronous, false) => (1, true),
            (TransferType::Bulk, false) => (2, false),
            (TransferType::Interrupt, false) => (3, true),
            (TransferType::Control, _) => (4, false),
            (TransferType::Isochronous, true) => (5, true),
            (TransferType::Bulk, true) => (6, false),
            (TransferType::Interrupt, true) => (7, true),
        };
        // The most bytes it moves in a service interval (6.2.3.8).
        let esit = if periodic { max_packet * (burst + 1) } else { 0 };
        // Isochronous endpoints get no retries; the rest three.
        let retries =
            if endpoint.transfer_type() == TransferType::Isochronous { 0 } else { RETRIES };
        self.memory.write32(at, u32::from(interval(&endpoint, speed)) << 16 | (esit >> 16) << 24);
        self.memory.write32(at + 4, retries | kind << 3 | burst << 8 | max_packet << 16);
        self.memory.write64(at + 8, ring.dequeue_pointer());
        // The average TRB length (4.14.1.1): a periodic endpoint's
        // interval's worth; 3 KiB for bulk.
        let average = if periodic { esit.max(1) } else { 3072 };
        self.memory.write32(at + 16, (average & 0xffff) | (esit & 0xffff) << 16);
    }

    /// Fills the input context for endpoint 0: a control endpoint with its
    /// packet size and its ring.
    fn write_ep0_context(&self) {
        let at = self.input(2);
        self.memory.write32(at, 0);
        self.memory.write32(at + 4, RETRIES | CONTROL_ENDPOINT | u32::from(self.max_packet) << 16);
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

    /// Gives the controller `endpoints` (at most [`MAX_ENDPOINTS`]), each
    /// with a ring and a page for its data from `alloc`, then puts the
    /// device in configuration `value` (bConfigurationValue).
    #[inline(never)]
    pub fn configure(
        &mut self,
        xhci: &mut Xhci,
        host: &mut dyn Host,
        alloc: &mut dyn FnMut(usize, bool) -> Option<Dma>,
        value: u8,
        endpoints: &[EndpointDescriptor],
    ) -> Result<(), Error> {
        if endpoints.len() > MAX_ENDPOINTS
            || self.endpoints.iter().any(Option::is_some)
            || endpoints.iter().any(|e| e.number() == 0 || e.max_packet() == 0)
        {
            return Err(Error::BadRequest);
        }
        self.clear_input();
        let mut add = ADD_SLOT;
        let mut entries = EP0;
        for (i, endpoint) in endpoints.iter().enumerate() {
            let index = endpoint_index(endpoint.address);
            let ring = if add & 1 << index != 0 {
                Err(Error::BadRequest)
            } else {
                alloc(2, xhci.below_4g()).and_then(Ring::new).ok_or(Error::NoMemory)
            };
            let ring = match ring {
                Ok(ring) => ring,
                Err(err) => {
                    self.drop_endpoints();
                    return Err(err);
                }
            };
            self.write_endpoint_context(index, *endpoint, &ring);
            add |= 1 << index;
            entries = entries.max(index);
            self.endpoints[i] = Some(Endpoint { index, ring });
        }
        self.memory.write32(INPUT + 4, add);
        let before = self.entries;
        self.entries = entries;
        self.write_slot_context();
        let input = self.memory.phys() + INPUT as u64;
        let slot = self.slot;
        if let Err(err) =
            xhci.command(Command::ConfigureEndpoint { slot, input, deconfigure: false }, host)
        {
            // Refused: the controller never had them.
            self.entries = before;
            self.drop_endpoints();
            return Err(err);
        }
        self.control_out(xhci, host, Setup::set_configuration(value), &[])
    }

    /// Gives back the endpoints' memory, which the controller does not
    /// have.
    fn drop_endpoints(&mut self) {
        for endpoint in &mut self.endpoints {
            *endpoint = None;
        }
    }

    /// Tells the controller the device is a hub (Configure Endpoint with
    /// the slot context alone, 4.6.6).
    pub fn set_hub(
        &mut self,
        xhci: &mut Xhci,
        host: &mut dyn Host,
        hub: HubSlot,
    ) -> Result<(), Error> {
        let before = self.hub.replace(hub);
        self.clear_input();
        self.memory.write32(INPUT + 4, ADD_SLOT);
        self.write_slot_context();
        let input = self.memory.phys() + INPUT as u64;
        let slot = self.slot;
        let result =
            xhci.command(Command::ConfigureEndpoint { slot, input, deconfigure: false }, host);
        if result.is_err() {
            self.hub = before;
        }
        result.map(|_| ())
    }

    /// The configured endpoint with device context index `index`.
    fn endpoint(&mut self, index: u8) -> Option<&mut Endpoint> {
        self.endpoints.iter_mut().flatten().find(|e| e.index == index)
    }

    /// Starts a transfer of up to `length` bytes from the IN endpoint at
    /// device context index `index` into its page; returns the TRB's
    /// physical address, which its event names. [`Device::received`]
    /// reads what came once it is done.
    pub fn submit_in(&mut self, xhci: &Xhci, index: u8, length: usize) -> Result<u64, Error> {
        if index & 1 == 0 || length == 0 || length > PAGE_SIZE {
            return Err(Error::BadRequest);
        }
        let slot = self.slot;
        let endpoint = self.endpoint(index).ok_or(Error::BadRequest)?;
        let buffer = endpoint.ring.phys() + PAGE_SIZE as u64;
        let at = endpoint.ring.push(Trb {
            parameter: buffer,
            status: length as u32,
            control: control::kind(kind::NORMAL) | control::IOC | control::ISP,
        });
        xhci.ring(slot, u32::from(index));
        Ok(at)
    }

    /// Starts a transfer of `data` (a page at most) to the OUT endpoint at
    /// device context index `index`; returns the TRB's physical address.
    pub fn submit_out(&mut self, xhci: &Xhci, index: u8, data: &[u8]) -> Result<u64, Error> {
        if index & 1 != 0 || index == 0 || data.len() > PAGE_SIZE {
            return Err(Error::BadRequest);
        }
        let slot = self.slot;
        let endpoint = self.endpoint(index).ok_or(Error::BadRequest)?;
        endpoint.ring.memory().write(PAGE_SIZE, data);
        let buffer = endpoint.ring.phys() + PAGE_SIZE as u64;
        let at = endpoint.ring.push(Trb {
            parameter: buffer,
            status: data.len() as u32,
            control: control::kind(kind::NORMAL) | control::IOC,
        });
        xhci.ring(slot, u32::from(index));
        Ok(at)
    }

    /// Copies what the last transfer on the IN endpoint at device context
    /// index `index` brought, `out.len()` bytes, into `out`.
    pub fn received(&mut self, index: u8, out: &mut [u8]) -> Result<(), Error> {
        let endpoint = self.endpoint(index).ok_or(Error::BadRequest)?;
        if out.len() > PAGE_SIZE {
            return Err(Error::BadRequest);
        }
        endpoint.ring.memory().read(PAGE_SIZE, out);
        Ok(())
    }

    /// Brings the endpoint at device context index `index` back after a
    /// transfer on it failed with `code` (an error halts it; a stall also
    /// halts the device's side, which CLEAR_FEATURE ends) or took too long
    /// ([`code::STOPPED`]): its ring then starts after the transfer.
    pub fn recover(
        &mut self,
        xhci: &mut Xhci,
        host: &mut dyn Host,
        index: u8,
        failed: u8,
    ) -> Result<(), Error> {
        if index == EP0 || self.endpoint(index).is_none() {
            return Err(Error::BadRequest);
        }
        self.skip(xhci, host, index, failed)?;
        if failed == code::STALL {
            // Endpoint `index` is number index / 2, IN if odd.
            let address = (index / 2) | if index & 1 != 0 { 0x80 } else { 0 };
            self.control_out(xhci, host, Setup::clear_halt(address), &[])?;
        }
        Ok(())
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
    fn control(
        &mut self,
        xhci: &mut Xhci,
        host: &mut dyn Host,
        setup: Setup,
    ) -> Result<usize, Error> {
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
                        self.skip(xhci, host, EP0, failed)?;
                        return Err(Error::Completion(failed));
                    }
                }
            }
            xhci.events_read();
            // A controller may end a transfer to a device pulled out of
            // its root port with no event (QEMU's does), so the port is
            // asked rather than the deadline waited for.
            if !xhci.port_status(self.place.root_port).connected() {
                let _ = self.skip(xhci, host, EP0, code::STOPPED);
                return Err(Error::Gone);
            }
            if host.now_us() >= deadline {
                self.skip(xhci, host, EP0, code::STOPPED)?;
                return Err(Error::TimedOut);
            }
            host.wait(POLL_US);
        }
    }

    /// Brings endpoint `endpoint` (a device context index) back after a
    /// transfer that failed with `code` (halting it) or took too long
    /// ([`code::STOPPED`]): resets or stops it, then points it past the
    /// transfer.
    fn skip(
        &mut self,
        xhci: &mut Xhci,
        host: &mut dyn Host,
        endpoint: u8,
        code: u8,
    ) -> Result<(), Error> {
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
                xhci.command(Command::StopEndpoint { slot, endpoint }, host)
            {
                tolerate(xhci.command(Command::ResetEndpoint { slot, endpoint }, host))?;
            }
        } else {
            tolerate(xhci.command(Command::ResetEndpoint { slot, endpoint }, host))?;
        }
        let pointer = if endpoint == EP0 {
            self.ep0.dequeue_pointer()
        } else {
            self.endpoint(endpoint).ok_or(Error::BadRequest)?.ring.dequeue_pointer()
        };
        let dequeue = Command::SetDequeue { slot, endpoint, pointer };
        match xhci.command(dequeue, host) {
            // Running after all: stop it, and move it then.
            Err(Error::Completion(code::CONTEXT_STATE)) => {
                tolerate(xhci.command(Command::StopEndpoint { slot, endpoint }, host))?;
                xhci.command(dequeue, host).map(|_| ())
            }
            other => other.map(|_| ()),
        }
    }

    /// Gives the slot back (Disable Slot). If the controller does not
    /// answer, the device's memory is never given back, since the
    /// controller may still use it.
    #[inline(never)]
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
