// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Ethernet adapters on USB (Phase 12, ADR 0199): CDC-ECM and CDC-NCM
//! (`radix_usb::net`), Realtek's RTL8153 and QEMU's `usb-net` among them,
//! in the configuration that has the adapter when the first is their
//! maker's own or RNDIS (`controller::choose_configuration`).
//!
//! Once a device is configured, [`Controller::start_net`] reads its MAC
//! address, asks an NCM adapter for the rules of its transfer blocks and
//! has it send none longer than a page, puts its data interface in the
//! setting with the bulk pair, has it pass on the frames to its own
//! address, broadcasts and every multicast frame, and gives it a network
//! interface (`net::program::attach_usb`), whose link the controller's
//! thread drives as e1000d drives a card's:
//!
//! - What comes on the bulk IN endpoint is a frame (ECM) or a block of
//!   them (NCM), put on the link for the stack, whose thread is woken.
//!   While the link is full the endpoint is not read, and the adapter
//!   holds or drops the frames, as a card would; the stack wakes the
//!   thread when it takes one.
//! - What the stack sends wakes the thread, which sends each frame on
//!   the bulk OUT endpoint, a transfer at a time: alone (ECM), or in a
//!   block of its own (NCM); a transfer that would end on a packet's
//!   boundary gets a byte more, as Linux's usbnet pads it, rather than a
//!   zero-length packet after it.
//! - The interrupt endpoint says when the link comes and goes, and its
//!   speed. An adapter is taken to have its link up, at 100 Mb/s, until
//!   it says otherwise: QEMU's says nothing.
//!
//! An adapter unplugged takes its link away: its interface goes down,
//! keeping its addresses, and is its own again when it comes back. Only
//! xHCI controllers carry the bulk transfers an adapter needs.

use alloc::sync::Arc;

use radix_klog::{info, warn};
use radix_mm::Kmalloc;
use radix_nic::link::{self, Driver};
use radix_nic::{Link, MAX_FRAME, MIN_FRAME};
use radix_usb::net::{self as usbnet, Notification, Port, Protocol, filter, ncm};
use radix_usb::{Setup, descriptor};
use radix_xhci::{code, endpoint_index};

use super::controller::{Controller, Name, Why, alloc};
use crate::net::program::{self, Pages};

/// Errors in a row on an endpoint before the adapter is given up.
const PIPE_ERRORS: u8 = 3;
/// The bytes each read of an ECM adapter asks for: the longest frame,
/// in whole packets at every speed.
const ECM_READ: usize = 2048;
/// The longest block an NCM adapter is asked to send: a page, the most a
/// transfer here carries.
const NCM_READ: usize = 4096;
/// The bytes a notification has at most that are read: a speed change's.
const NOTIFICATION: usize = 16;
/// The link an adapter is taken to have until it says.
const ASSUMED: Link = Link { up: true, mbps: 100, full_duplex: true };

/// What the thread keeps of an adapter it carries.
pub(super) struct Ethernet {
    port: Port,
    /// Its interface's number among the cards.
    number: usize,
    /// The link to its interface's stack, and the thread's side of it.
    pages: Arc<Pages, Kmalloc>,
    side: Driver,
    /// Its endpoints' device context indices.
    input: u8,
    output: u8,
    notify: Option<u8>,
    /// The transfers in flight on them, 0 for none.
    reading: u64,
    writing: u64,
    listening: u64,
    /// The bytes each read asks for.
    read_length: usize,
    /// An NCM adapter's rules for the blocks it takes, and the next
    /// block's sequence number.
    blocks: Option<ncm::Parameters>,
    sequence: u16,
    /// Its link, as it last said.
    link: Link,
    /// Errors in a row on its endpoints.
    errors: u8,
}

/// The adapter `ethernet`, whose device is gone or stopped: its
/// interface goes down.
pub(super) fn gone(ethernet: &mut Option<Ethernet>) {
    if let Some(ethernet) = ethernet.take() {
        ethernet.side.set_link(&*ethernet.pages, Link::default());
        program::detach_usb(ethernet.number);
    }
}

/// A random, locally administered, unicast address, for an adapter whose
/// own cannot be read.
fn random_mac() -> [u8; 6] {
    let mut mac = [0u8; 6];
    radix_random::fill(&mut mac);
    radix_nic::local_mac(mac)
}

/// Gives the first `len` bytes of `out` to send a byte more, a zero, if
/// they would end on a boundary of `packet`-byte packets and `out` has
/// room for it: says how many to send.
fn padded(out: &mut [u8], len: usize, packet: u16) -> usize {
    let packet = usize::from(packet).max(1);
    if len.is_multiple_of(packet) && len < out.len() {
        out[len] = 0;
        len + 1
    } else {
        len
    }
}

impl Controller {
    /// Starts the Ethernet adapter `port` of the configured device in
    /// slot `slot`: gives it its setting and its interface, and starts
    /// reading it.
    #[inline(never)]
    pub(super) fn start_net(&mut self, slot: u8, port: Port) -> Result<(), Why> {
        let at = self.find(slot).ok_or(Why::Device("it is gone"))?;
        let Controller { devices, xhci, host, buffer, source, bus, .. } = self;
        let attached = &mut devices[at];
        let path = attached.path;
        let device = &mut attached.device;
        let mac = match port.mac_string {
            0 => None,
            index => {
                let setup = Setup::get_descriptor(descriptor::STRING, index, 0x0409, 64);
                device
                    .control_in(xhci, host, setup, &mut buffer[..64])
                    .ok()
                    .and_then(|came| usbnet::mac_from_string(&buffer[..came]))
            }
        };
        let mac = mac.unwrap_or_else(random_mac);
        let (read_length, blocks) = match port.protocol {
            Protocol::Ecm => (ECM_READ, None),
            Protocol::Ncm => {
                let setup = usbnet::get_ntb_parameters(port.control);
                let came = device.control_in(xhci, host, setup, &mut buffer[..ncm::PARAMETERS])?;
                let parameters = ncm::Parameters::parse(&buffer[..came])
                    .ok_or(Why::Device("its transfer blocks have rules it cannot keep"))?;
                if parameters.in_max as usize > NCM_READ {
                    let size = usbnet::ntb_input_size(NCM_READ as u32);
                    let setup = usbnet::set_ntb_input_size(port.control);
                    device.control_out(xhci, host, setup, &size)?;
                }
                (NCM_READ.min(parameters.in_max as usize), Some(parameters))
            }
        };
        // Its second setting has the bulk pair, as a rule; an adapter
        // whose first has them was given them as it was configured.
        if port.alternate != 0 {
            let mut alloc = alloc(*source);
            let setting = (port.data, port.alternate);
            let pair = [port.bulk_in, port.bulk_out];
            device.set_interface(xhci, host, &mut alloc, setting, &[], &pair)?;
        }
        // An adapter that passes every frame may refuse the request, as
        // QEMU's does; Linux goes on too.
        let passed = filter::DIRECTED | filter::BROADCAST | filter::ALL_MULTICAST;
        let _ =
            device.control_out(xhci, host, usbnet::set_packet_filter(port.control, passed), &[]);
        let place = Name(*bus, path);
        let (number, pages) = program::attach_usb(*bus - 1, mac, port.protocol.name(), &place)
            .ok_or(Why::Device("it could not be given an interface"))?;
        let side = Driver::new();
        side.set_link(&*pages, ASSUMED);
        program::wake(number);
        attached.ethernet = Some(Ethernet {
            port,
            number,
            pages,
            side,
            input: endpoint_index(port.bulk_in.address),
            output: endpoint_index(port.bulk_out.address),
            notify: port.notify.map(|endpoint| endpoint_index(endpoint.address)),
            reading: 0,
            writing: 0,
            listening: 0,
            read_length,
            blocks,
            sequence: 0,
            link: ASSUMED,
            errors: 0,
        });
        self.net_listen(at);
        Ok(())
    }

    /// Starts the reads of the adapter of the device at `at` that are not
    /// running: its frames', while its link has room, and its
    /// notifications'.
    fn net_listen(&mut self, at: usize) {
        let attached = &mut self.devices[at];
        let Some(ethernet) = attached.ethernet.as_mut() else { return };
        let memory = &*ethernet.pages;
        let mut room = ethernet.side.can_receive(memory);
        if ethernet.reading == 0 && !room {
            // Asked before looking again, so that a frame taken between
            // the two still wakes the thread.
            memory.want_room();
            room = ethernet.side.can_receive(memory);
        }
        if ethernet.reading == 0 && room {
            match attached.device.submit_in(&self.xhci, ethernet.input, ethernet.read_length) {
                Ok(trb) => ethernet.reading = trb,
                Err(err) => {
                    warn!(
                        "USB: {}: its network adapter cannot be read: {err}",
                        Name(self.bus, attached.path)
                    );
                    gone(&mut attached.ethernet);
                    return;
                }
            }
        }
        if let (Some(notify), 0) = (ethernet.notify, ethernet.listening) {
            match attached.device.submit_in(&self.xhci, notify, NOTIFICATION) {
                Ok(trb) => ethernet.listening = trb,
                // Its link is left as it is.
                Err(_) => ethernet.notify = None,
            }
        }
    }

    /// Carries every adapter: sends what its stack has for it, and reads
    /// it again once its link has room. Says whether anything moved.
    pub(super) fn net(&mut self) -> bool {
        let mut any = false;
        for at in 0..self.devices.len() {
            if self.devices[at].ethernet.is_some() {
                any |= self.net_send(at);
                self.net_listen(at);
            }
        }
        any
    }

    /// Sends the next frame the stack has for the adapter of the device
    /// at `at`, unless one is being sent. Says whether it took one.
    fn net_send(&mut self, at: usize) -> bool {
        let Controller { devices, buffer, xhci, bus, .. } = self;
        let attached = &mut devices[at];
        let Some(ethernet) = attached.ethernet.as_mut() else { return false };
        if ethernet.writing != 0 {
            return false;
        }
        let (frame, block) = buffer.split_at_mut(link::SLOT);
        let Some(len) = ethernet.side.take(&*ethernet.pages, frame) else { return false };
        if !(MIN_FRAME..=MAX_FRAME).contains(&len) {
            return true;
        }
        let packet = ethernet.port.bulk_out.max_packet();
        let out: &[u8] = match ethernet.blocks {
            None => {
                let len = padded(frame, len, packet);
                &frame[..len]
            }
            Some(parameters) => {
                let sequence = ethernet.sequence;
                ethernet.sequence = sequence.wrapping_add(1);
                let Some(built) = ncm::build(&parameters, sequence, &frame[..len], block) else {
                    return true;
                };
                // The pad byte is past the block's own length, and only
                // where the adapter takes it.
                let room = block.len().min(parameters.out_max as usize);
                let len = padded(&mut block[..room], built, packet);
                &block[..len]
            }
        };
        match attached.device.submit_out(xhci, ethernet.output, out) {
            Ok(trb) => ethernet.writing = trb,
            Err(err) => warn!(
                "USB: {}: a frame its network adapter was to send is lost: {err}",
                Name(*bus, attached.path)
            ),
        }
        true
    }

    /// Whether the transfer at `trb` on `endpoint` of the device at `at`
    /// is its adapter's; if so, takes what it brought.
    pub(super) fn net_transfer(
        &mut self,
        at: usize,
        trb: u64,
        endpoint: u8,
        code: u8,
        residual: u32,
    ) -> bool {
        let Some(ethernet) = self.devices[at].ethernet.as_ref() else { return false };
        if ethernet.input == endpoint && ethernet.reading == trb {
            self.net_received(at, code, residual);
        } else if ethernet.output == endpoint && ethernet.writing == trb {
            self.net_sent(at, code);
        } else if ethernet.notify == Some(endpoint) && ethernet.listening == trb {
            self.net_notified(at, code, residual);
        } else {
            return false;
        }
        self.net_send(at);
        self.net_listen(at);
        true
    }

    /// The read on the adapter of the device at `at` ended with `code`,
    /// `residual` bytes short: its frames go on the link.
    #[inline(never)]
    fn net_received(&mut self, at: usize, code: u8, residual: u32) {
        let Controller { devices, buffer, xhci, host, bus, .. } = self;
        let attached = &mut devices[at];
        let Some(ethernet) = attached.ethernet.as_mut() else { return };
        ethernet.reading = 0;
        match code {
            code::SUCCESS | code::SHORT_PACKET => {
                ethernet.errors = 0;
                let came = ethernet.read_length.saturating_sub(residual as usize);
                if attached.device.received(ethernet.input, &mut buffer[..came]).is_err() {
                    return;
                }
                let memory = &*ethernet.pages;
                let side = &mut ethernet.side;
                let mut give = |frame: &[u8]| {
                    if (MIN_FRAME..=MAX_FRAME).contains(&frame.len()) {
                        side.received(memory, frame);
                    }
                };
                match ethernet.blocks {
                    None => give(&buffer[..came]),
                    Some(_) => {
                        for (offset, len) in ncm::frames(&buffer[..came]) {
                            give(&buffer[offset..offset + len]);
                        }
                    }
                }
                program::wake(ethernet.number);
            }
            code::STOPPED => {}
            failed => {
                ethernet.errors += 1;
                let input = ethernet.input;
                if ethernet.errors > PIPE_ERRORS
                    || attached.device.recover(xhci, host, input, failed).is_err()
                {
                    warn!(
                        "USB: {}: its network adapter stopped: {}",
                        Name(*bus, attached.path),
                        code::name(failed)
                    );
                    gone(&mut attached.ethernet);
                }
            }
        }
    }

    /// The write on the adapter of the device at `at` ended with `code`.
    fn net_sent(&mut self, at: usize, code: u8) {
        let Controller { devices, xhci, host, bus, .. } = self;
        let attached = &mut devices[at];
        let Some(ethernet) = attached.ethernet.as_mut() else { return };
        ethernet.writing = 0;
        match code {
            code::SUCCESS | code::SHORT_PACKET => ethernet.errors = 0,
            code::STOPPED => {}
            failed => {
                ethernet.errors += 1;
                let output = ethernet.output;
                if ethernet.errors > PIPE_ERRORS
                    || attached.device.recover(xhci, host, output, failed).is_err()
                {
                    warn!(
                        "USB: {}: its network adapter stopped sending: {}",
                        Name(*bus, attached.path),
                        code::name(failed)
                    );
                    gone(&mut attached.ethernet);
                }
            }
        }
    }

    /// The adapter of the device at `at` said something on its interrupt
    /// endpoint, ended with `code`, `residual` bytes short: its link came
    /// or went, or changed its speed.
    fn net_notified(&mut self, at: usize, code: u8, residual: u32) {
        let Controller { devices, buffer, bus, .. } = self;
        let attached = &mut devices[at];
        let Some(ethernet) = attached.ethernet.as_mut() else { return };
        ethernet.listening = 0;
        let Some(notify) = ethernet.notify else { return };
        if !matches!(code, code::SUCCESS | code::SHORT_PACKET) {
            // Not read again: its link stays as it is.
            ethernet.notify = None;
            return;
        }
        let came = NOTIFICATION.saturating_sub(residual as usize);
        if attached.device.received(notify, &mut buffer[..came]).is_err() {
            return;
        }
        let mut link = ethernet.link;
        match usbnet::notification(&buffer[..came]) {
            Some(Notification::Connection(up)) => link.up = up,
            Some(Notification::Speed(down, _)) if down >= 1_000_000 => {
                link.mbps = down / 1_000_000;
            }
            _ => return,
        }
        if link == ethernet.link {
            return;
        }
        if link.up != ethernet.link.up {
            let state = if link.up { "up" } else { "down" };
            info!("USB: {}: its network adapter's link is {state}", Name(*bus, attached.path));
        }
        ethernet.link = link;
        ethernet.side.set_link(&*ethernet.pages, link);
        program::wake(ethernet.number);
    }
}
