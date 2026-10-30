// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The kernel's TCP/IP stack, on smoltcp (ADR 0180).
//!
//! A [`Stack`] is one network interface: a [`Link`], the card that sends
//! and receives its Ethernet frames, and smoltcp's interface and
//! sockets above it. The kernel's network thread calls [`Stack::poll`]
//! when the card raises an interrupt and when [`Stack::poll_delay`] says
//! a timer is due; everything the stack does happens there, on that
//! thread: smoltcp has no locks and no threads of its own.
//!
//! smoltcp is built without its `alloc` feature, since the kernel's
//! allocations are explicit and fallible (ADR 0017): every buffer it
//! uses is lent to it as a slice that lives as long as the stack. A
//! frame is copied once on the way in, from the card's slot to the
//! stack's receive buffer, and once on the way out, from the transmit
//! buffer to a slot.
//!
//! [`Pinger`] sends ICMP echo requests and matches their replies, for
//! the kernel's own checks of a link (`selftest=net`). The stack itself
//! answers echo requests to its addresses.
//!
//! The tests run two stacks on the host, joined by a simulated wire.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

pub use smoltcp;
pub use smoltcp::wire::{EthernetAddress, Ipv4Address, Ipv4Cidr};

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{self, Checksum, ChecksumCapabilities, DeviceCapabilities, Medium};
use smoltcp::socket::icmp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr};

/// The bytes of the longest Ethernet frame the stack sends or takes:
/// its header and an IP packet of 1500 bytes, without the frame check
/// sequence, which the card adds and removes.
pub const FRAME: usize = 1514;

/// A network card, as the stack sees it: something that takes and gives
/// Ethernet frames.
pub trait Link {
    /// The card's MAC address.
    fn mac(&self) -> [u8; 6];
    /// Copies the next frame the card received into `frame`, and returns
    /// its length; `None` when no frame waits.
    fn receive(&mut self, frame: &mut [u8]) -> Option<usize>;
    /// Whether the card can take a frame to send now.
    fn can_send(&mut self) -> bool;
    /// Hands `frame` to the card to send: false if it could not take it,
    /// and the frame is lost, as on a wire.
    fn send(&mut self, frame: &[u8]) -> bool;
    /// Tells the card about the frames sent and taken since the last
    /// time (its doorbells).
    fn flush(&mut self);
}

/// The memory a stack works in, lent to it for as long as it lives.
pub struct Memory {
    /// A frame received, while the stack reads it: [`FRAME`] bytes.
    pub receive: &'static mut [u8],
    /// A frame to send, while the stack writes it: [`FRAME`] bytes.
    pub transmit: &'static mut [u8],
    /// A slot for each socket the stack can have.
    pub sockets: &'static mut [SocketStorage<'static>],
}

/// One network interface: a card and the stack on it.
pub struct Stack<L: Link> {
    link: L,
    iface: Interface,
    sockets: SocketSet<'static>,
    /// How many sockets fit in `sockets`.
    slots: usize,
    receive: &'static mut [u8],
    transmit: &'static mut [u8],
}

/// The card as smoltcp's device, while the stack polls.
struct Phy<'a, L: Link> {
    link: &'a mut L,
    receive: &'a mut [u8],
    transmit: &'a mut [u8],
}

/// A frame received.
struct Received<'a>(&'a [u8]);

/// A frame to send.
struct Sending<'a, L: Link> {
    link: &'a mut L,
    buffer: &'a mut [u8],
}

impl phy::RxToken for Received<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(self.0)
    }
}

impl<L: Link> phy::TxToken for Sending<'_, L> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        // smoltcp asks for no more than the capabilities' largest frame,
        // which the buffer holds; a frame cut short would be wrong, so a
        // longer one is written, but not sent.
        let fits = len <= self.buffer.len();
        let frame = &mut self.buffer[..len.min(FRAME)];
        let result = f(frame);
        if fits {
            self.link.send(frame);
        }
        result
    }
}

impl<L: Link> phy::Device for Phy<'_, L> {
    type RxToken<'b>
        = Received<'b>
    where
        Self: 'b;
    type TxToken<'b>
        = Sending<'b, L>
    where
        Self: 'b;

    fn receive(&mut self, _now: Instant) -> Option<(Received<'_>, Sending<'_, L>)> {
        let len = self.link.receive(self.receive)?;
        let len = len.min(self.receive.len());
        Some((Received(&self.receive[..len]), Sending { link: self.link, buffer: self.transmit }))
    }

    fn transmit(&mut self, _now: Instant) -> Option<Sending<'_, L>> {
        if self.link.can_send() {
            Some(Sending { link: self.link, buffer: self.transmit })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = FRAME;
        // Every checksum is made and checked here, in software: the card
        // is asked for no offloads.
        let mut checksum = ChecksumCapabilities::default();
        checksum.ipv4 = Checksum::Both;
        checksum.icmpv4 = Checksum::Both;
        checksum.udp = Checksum::Both;
        checksum.tcp = Checksum::Both;
        caps.checksum = checksum;
        caps
    }
}

/// Reads an IPv4 address with its prefix length, `10.0.2.15/24`; an
/// address without one is a /32.
pub fn parse_cidr(text: &str) -> Option<Ipv4Cidr> {
    let (address, prefix) = match text.split_once('/') {
        Some((address, prefix)) => (address, prefix.parse::<u8>().ok()?),
        None => (text, 32),
    };
    let address = address.parse::<Ipv4Address>().ok()?;
    (prefix <= 32).then(|| Ipv4Cidr::new(address, prefix))
}

/// An instant for smoltcp, from the kernel's clock in nanoseconds.
fn instant(now_ns: u64) -> Instant {
    Instant::from_micros(i64::try_from(now_ns / 1_000).unwrap_or(i64::MAX))
}

impl<L: Link> Stack<L> {
    /// A stack on `link`, with no address yet, working in `memory`.
    /// `seed` is a random number, which picks the stack's initial TCP
    /// sequence numbers and ports.
    pub fn new(mut link: L, memory: Memory, seed: u64, now_ns: u64) -> Stack<L> {
        let Memory { receive, transmit, sockets } = memory;
        let mac = EthernetAddress(link.mac());
        let mut config = Config::new(HardwareAddress::Ethernet(mac));
        config.random_seed = seed;
        let mut phy = Phy { link: &mut link, receive: &mut *receive, transmit: &mut *transmit };
        let iface = Interface::new(config, &mut phy, instant(now_ns));
        let slots = sockets.len();
        Stack { link, iface, sockets: SocketSet::new(sockets), slots, receive, transmit }
    }

    /// Gives the interface `address` (with its prefix) as its only IPv4
    /// address, and `gateway` as its default route, or none.
    pub fn set_ipv4(&mut self, address: Ipv4Cidr, gateway: Option<Ipv4Address>) {
        self.iface.update_ip_addrs(|addrs| {
            addrs.clear();
            // One address fits: there is room for at least one.
            let _ = addrs.push(IpCidr::Ipv4(address));
        });
        let routes = self.iface.routes_mut();
        routes.remove_default_ipv4_route();
        if let Some(gateway) = gateway {
            // The table has room for a default route once it is removed.
            let _ = routes.add_default_ipv4_route(gateway);
        }
    }

    /// The interface's IPv4 address, if it has one.
    pub fn ipv4(&self) -> Option<Ipv4Address> {
        self.iface.ipv4_addr()
    }

    /// The card under the stack.
    pub fn link(&mut self) -> &mut L {
        &mut self.link
    }

    /// Does what is due: takes the frames the card received, answers and
    /// passes them on to sockets, sends what sockets have to send and
    /// what timers call for. Returns whether a socket's state may have
    /// changed.
    pub fn poll(&mut self, now_ns: u64) -> bool {
        let mut phy = Phy {
            link: &mut self.link,
            receive: &mut *self.receive,
            transmit: &mut *self.transmit,
        };
        let result = self.iface.poll(instant(now_ns), &mut phy, &mut self.sockets);
        self.link.flush();
        result == PollResult::SocketStateChanged
    }

    /// How long until [`Stack::poll`] has something to do even if no
    /// frame comes, in nanoseconds: `None` when only a frame can give it
    /// work, 0 when it has some now.
    pub fn poll_delay(&mut self, now_ns: u64) -> Option<u64> {
        self.iface
            .poll_delay(instant(now_ns), &self.sockets)
            .map(|delay| delay.total_micros().saturating_mul(1_000))
    }

    /// Adds `socket` to the stack: `None` if every socket slot is taken.
    pub fn add_socket(
        &mut self,
        socket: impl smoltcp::socket::AnySocket<'static>,
    ) -> Option<SocketHandle> {
        // `SocketSet::add` panics when it is full; it is not let fill.
        let full = self.sockets.iter().count() >= self.slots;
        (!full).then(|| self.sockets.add(socket))
    }
}

/// The buffers of a [`Pinger`]'s socket, lent to it for as long as it
/// lives.
pub struct PingMemory {
    /// Room for the echo replies that wait to be read.
    pub receive_meta: &'static mut [icmp::PacketMetadata],
    /// Their bytes.
    pub receive: &'static mut [u8],
    /// Room for the echo requests that wait to be sent.
    pub transmit_meta: &'static mut [icmp::PacketMetadata],
    /// Their bytes.
    pub transmit: &'static mut [u8],
}

/// What [`Pinger::send`] can fail with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PingError {
    /// The socket's buffer for requests is full: poll and try again.
    Busy,
    /// The interface has no IPv4 address to send from.
    NoAddress,
}

/// Sends ICMP echo requests ("pings") and matches their replies.
pub struct Pinger {
    handle: SocketHandle,
    ident: u16,
}

/// The bytes after the echo header: the request's own number, so that a
/// reply that echoes another's data is not taken for it.
const PING_DATA: usize = 8;

impl Pinger {
    /// A pinger on `stack` whose requests carry the identifier `ident`:
    /// `None` if the stack has no socket slot left.
    pub fn new<L: Link>(stack: &mut Stack<L>, memory: PingMemory, ident: u16) -> Option<Pinger> {
        let PingMemory { receive_meta, receive, transmit_meta, transmit } = memory;
        let mut socket = icmp::Socket::new(
            icmp::PacketBuffer::new(receive_meta, receive),
            icmp::PacketBuffer::new(transmit_meta, transmit),
        );
        // An unbound socket cannot be bound wrongly: the endpoint is set.
        socket.bind(icmp::Endpoint::Ident(ident)).ok()?;
        let handle = stack.add_socket(socket)?;
        Some(Pinger { handle, ident })
    }

    /// Queues echo request number `seq` to `to`; [`Stack::poll`] sends it.
    pub fn send<L: Link>(
        &self,
        stack: &mut Stack<L>,
        to: Ipv4Address,
        seq: u16,
    ) -> Result<(), PingError> {
        if stack.ipv4().is_none() {
            return Err(PingError::NoAddress);
        }
        let data = u64::from(seq).to_be_bytes();
        let request = Icmpv4Repr::EchoRequest { ident: self.ident, seq_no: seq, data: &data };
        let socket = stack.sockets.get_mut::<icmp::Socket<'static>>(self.handle);
        let buffer =
            socket.send(request.buffer_len(), IpAddress::Ipv4(to)).map_err(|_| PingError::Busy)?;
        let mut packet = Icmpv4Packet::new_unchecked(buffer);
        request.emit(&mut packet, &ChecksumCapabilities::default());
        Ok(())
    }

    /// The next echo reply that arrived for this pinger: its number and
    /// who sent it. Other ICMP messages, and replies that do not echo
    /// their request's data, are skipped.
    pub fn reply<L: Link>(&self, stack: &mut Stack<L>) -> Option<(u16, Ipv4Address)> {
        let socket = stack.sockets.get_mut::<icmp::Socket<'static>>(self.handle);
        while let Ok((payload, from)) = socket.recv() {
            // IPv4 is the stack's only protocol (until IPv6, in 12c).
            let IpAddress::Ipv4(from) = from;
            let Ok(packet) = Icmpv4Packet::new_checked(payload) else { continue };
            let Ok(repr) = Icmpv4Repr::parse(&packet, &ChecksumCapabilities::default()) else {
                continue;
            };
            if let Icmpv4Repr::EchoReply { ident, seq_no, data } = repr
                && ident == self.ident
                && data.len() == PING_DATA
                && data == u64::from(seq_no).to_be_bytes()
            {
                return Some((seq_no, from));
            }
        }
        None
    }
}
