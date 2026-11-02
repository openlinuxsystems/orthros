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
//! uses is lent to it as a `&'static mut` slice. The stack's own live as
//! long as it does. A socket's come back when it is removed, through the
//! kernel's patch to smoltcp (ADR 0182), so that the kernel can take
//! their memory back. A frame is copied once on the way in, from the
//! card's slot to the stack's receive buffer, and once on the way out,
//! from the transmit buffer to a slot. The loopback link's packets are
//! not copied at all: the stack writes and reads them in place, in the
//! link's ring (ADR 0185). And a TCP connection with both its ends on the
//! loopback link passes its bytes straight across, in no packet at all,
//! when it can ([`Stack::send_across`], ADR 0186).
//!
//! An Ethernet interface's IPv6 addresses are its own work, in
//! [`ipv6`]: its link-local address, those it makes from the prefixes
//! routers advertise (ADR 0330), and the routes and DNS servers they
//! advertise; smoltcp answers neighbour solicitations for them, and
//! sends what they carry.
//!
//! [`Pinger`] sends ICMP echo requests and matches their replies, for
//! the kernel's own checks of a link (`selftest=net`). The stack itself
//! answers echo requests to its addresses.
//!
//! Programs' TCP and UDP sockets (ADR 0183) are smoltcp's, made from the
//! memory of a [`TcpMemory`] or [`UdpMemory`], which comes back when they
//! are taken out with [`Stack::remove_tcp`] or [`Stack::remove_udp`]. A
//! [`Loopback`] is the link of the loopback interface, whose packets come
//! back to it.
//!
//! The tests run two stacks on the host, joined by a simulated wire, and
//! one on a loopback link.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod across;
pub mod ipv6;
mod loopback;
#[cfg(kani)]
mod proofs;
mod sockets;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests6;

pub use across::{Across, OtherEnd};
pub use loopback::Loopback;
pub use smoltcp;
pub use smoltcp::wire::{
    EthernetAddress, IpAddress, IpVersion, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr,
};
pub use sockets::{ConnectError, Shown, TcpMemory, UdpMemory};

use core::cell::Cell;

use managed::ManagedSlice;
use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{self, Checksum, ChecksumCapabilities, DeviceCapabilities};
use smoltcp::socket::{AnySocket, Socket, icmp};
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpCidr};

/// The bytes of the longest Ethernet frame the stack sends or takes:
/// its header and an IP packet of 1500 bytes, without the frame check
/// sequence, which the card adds and removes.
pub const FRAME: usize = 1514;

/// The bytes of an Ethernet frame's header.
const ETHERNET_HEADER: usize = 14;

/// What a link's frames are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Medium {
    /// Ethernet frames, with their MAC addresses: a network card's.
    Ethernet,
    /// IP packets alone: the loopback interface's.
    Ip,
}

/// A network card, as the stack sees it: something that takes and gives
/// Ethernet frames, or for the loopback interface IP packets.
pub trait Link {
    /// What its frames are: Ethernet's, unless it says otherwise.
    fn medium(&self) -> Medium {
        Medium::Ethernet
    }
    /// Whether a frame waits to be received that the stack has not been
    /// told of: the loopback link's, which come from the stack itself.
    fn pending(&self) -> bool {
        false
    }
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
    /// Whether a frame can be damaged on its way, so that the stack makes
    /// and checks checksums: a card's can; the loopback link's packets
    /// never leave memory, and Linux's loopback skips them too.
    fn needs_checksums(&self) -> bool {
        true
    }
    /// The loopback link, if this is one: the stack writes and reads its
    /// packets in place, in its ring, rather than through
    /// [`Link::send`] and [`Link::receive`].
    fn loopback(&mut self) -> Option<&mut Loopback> {
        None
    }
}

/// The memory a stack works in, lent to it for as long as it lives.
pub struct Memory {
    /// A frame received, while the stack reads it: [`FRAME`] bytes, as
    /// longer frames are cut to it; on an IP link, the longest packet.
    pub receive: &'static mut [u8],
    /// A frame to send, while the stack writes it: [`FRAME`] bytes, or
    /// fewer for a smaller MTU; on an IP link, its MTU.
    pub transmit: &'static mut [u8],
    /// A slot for each socket the stack can have.
    pub sockets: &'static mut [SocketStorage<'static>],
}

/// One network interface: a card and the stack on it.
pub struct Stack<L: Link> {
    link: L,
    iface: Interface,
    sockets: SocketSet<'static>,
    receive: &'static mut [u8],
    transmit: &'static mut [u8],
    /// The default route, which [`Stack::configure`] set.
    gateway: Option<Ipv4Address>,
    /// Its IPv6 addresses and routers.
    ipv6: ipv6::Ipv6,
    /// Whether its addresses changed since [`Stack::take_moved`].
    moved: bool,
    counted: Counted,
}

/// What an interface has sent and received since it started, as Linux
/// counts it for `/proc/net/dev`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Frames received, whole or cut to the longest the stack takes.
    pub received_packets: u64,
    /// Their bytes.
    pub received_bytes: u64,
    /// Frames sent: handed to the card, or put in the loopback link's ring.
    pub sent_packets: u64,
    /// Their bytes.
    pub sent_bytes: u64,
    /// Frames lost on their way out: the card could not take them, or the
    /// loopback link's ring had no room.
    pub send_dropped: u64,
}

/// The counts of [`Counters`], which the tokens a poll hands smoltcp add
/// to while both of a frame's are out.
#[derive(Default)]
struct Counted {
    received_packets: Cell<u64>,
    received_bytes: Cell<u64>,
    sent_packets: Cell<u64>,
    sent_bytes: Cell<u64>,
    send_dropped: Cell<u64>,
}

/// Adds `by` to `count`.
fn add(count: &Cell<u64>, by: usize) {
    count.set(count.get().wrapping_add(by as u64));
}

impl Counted {
    fn received(&self, len: usize) {
        add(&self.received_packets, 1);
        add(&self.received_bytes, len);
    }

    fn sent(&self, len: usize) {
        add(&self.sent_packets, 1);
        add(&self.sent_bytes, len);
    }

    fn dropped(&self) {
        add(&self.send_dropped, 1);
    }

    fn get(&self) -> Counters {
        Counters {
            received_packets: self.received_packets.get(),
            received_bytes: self.received_bytes.get(),
            sent_packets: self.sent_packets.get(),
            sent_bytes: self.sent_bytes.get(),
            send_dropped: self.send_dropped.get(),
        }
    }
}

/// The card as smoltcp's device, while the stack polls.
struct Phy<'a, L: Link> {
    link: &'a mut L,
    /// Whether the link is the loopback link, whose packets are written
    /// and read in place.
    in_place: bool,
    receive: &'a mut [u8],
    transmit: &'a mut [u8],
    counted: &'a Counted,
    /// The interface's IPv6 state, which hears the router advertisements
    /// and the claims on its addresses that come, at `now`.
    ipv6: Option<&'a mut ipv6::Ipv6>,
    now: u64,
}

impl<'a, L: Link> Phy<'a, L> {
    fn new(
        link: &'a mut L,
        receive: &'a mut [u8],
        transmit: &'a mut [u8],
        counted: &'a Counted,
    ) -> Phy<'a, L> {
        let in_place = link.loopback().is_some();
        Phy { link, in_place, receive, transmit, counted, ipv6: None, now: 0 }
    }
}

/// A frame received: copied from the card, or a loopback packet in place
/// (counted as it is read).
enum Received<'a> {
    Copied(&'a [u8]),
    InPlace(loopback::Oldest<'a>, &'a Counted),
}

/// A frame to send: written to the stack's buffer for the card to copy, or
/// in place in the loopback link's ring, there or as the answer to the
/// packet received. `spare` takes a loopback packet with no room, which
/// is lost. Each is counted as it goes.
enum Sending<'a, L: Link> {
    Copied { link: &'a mut L, buffer: &'a mut [u8], counted: &'a Counted },
    InPlace { link: &'a mut Loopback, spare: &'a mut [u8], counted: &'a Counted },
    Answer { answer: loopback::Answer<'a>, spare: &'a mut [u8], counted: &'a Counted },
}

impl phy::RxToken for Received<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        match self {
            Received::Copied(frame) => f(frame),
            Received::InPlace(packet, counted) => packet.read(|packet| {
                counted.received(packet.len());
                f(packet)
            }),
        }
    }
}

/// `write`, which writes a loopback packet of `len` bytes, counting it as
/// sent, or as lost when it is written to the spare buffer at `spare_at`,
/// out of the ring.
fn counted_write<R>(
    len: usize,
    spare_at: usize,
    counted: &Counted,
    write: impl FnOnce(&mut [u8]) -> R,
) -> impl FnOnce(&mut [u8]) -> R {
    move |packet: &mut [u8]| {
        if packet.as_ptr().addr() == spare_at {
            counted.dropped();
        } else {
            counted.sent(len);
        }
        write(packet)
    }
}

impl<L: Link> phy::TxToken for Sending<'_, L> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        match self {
            Sending::Copied { link, buffer, counted } => {
                // smoltcp asks for no more than the capabilities' largest
                // frame, which is no longer than the buffer. Were it to ask
                // for more, a frame cut short would be wrong: it is
                // written, but not sent.
                let room = buffer.len();
                let fits = len <= room;
                let frame = &mut buffer[..len.min(room)];
                let result = f(frame);
                if fits && link.send(frame) {
                    counted.sent(len);
                } else {
                    counted.dropped();
                }
                result
            }
            Sending::InPlace { link, spare, counted } => {
                let write = counted_write(len, spare.as_ptr().addr(), counted, f);
                link.send_with(len, spare, write)
            }
            Sending::Answer { answer, spare, counted } => {
                let write = counted_write(len, spare.as_ptr().addr(), counted, f);
                answer.send_with(len, spare, write)
            }
        }
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
        let counted = self.counted;
        if self.in_place {
            let (packet, answer) = self.link.loopback()?.oldest()?;
            let answer = Sending::Answer { answer, spare: self.transmit, counted };
            return Some((Received::InPlace(packet, counted), answer));
        }
        let len = self.link.receive(self.receive)?;
        let len = len.min(self.receive.len());
        counted.received(len);
        if let Some(ipv6) = self.ipv6.as_deref_mut()
            && ipv6.enabled()
            && let Some(heard) = ipv6::hear(&self.receive[..len])
        {
            ipv6.heard(heard, self.now);
        }
        let sending = Sending::Copied { link: self.link, buffer: self.transmit, counted };
        Some((Received::Copied(&self.receive[..len]), sending))
    }

    fn transmit(&mut self, _now: Instant) -> Option<Sending<'_, L>> {
        let counted = self.counted;
        if self.in_place {
            let link = self.link.loopback()?;
            return link.can_send().then_some(Sending::InPlace {
                link,
                spare: self.transmit,
                counted,
            });
        }
        if self.link.can_send() {
            Some(Sending::Copied { link: self.link, buffer: self.transmit, counted })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        // The largest frame, with its Ethernet header: one the transmit
        // buffer holds; on an IP link, the largest packet.
        match self.link.medium() {
            Medium::Ethernet => {
                caps.medium = phy::Medium::Ethernet;
                caps.max_transmission_unit = self.transmit.len().min(FRAME);
            }
            Medium::Ip => {
                caps.medium = phy::Medium::Ip;
                caps.max_transmission_unit = self.transmit.len();
            }
        }
        // Every checksum is made and checked here, in software: the card
        // is asked for no offloads. A link that cannot damage a frame
        // needs none.
        let both = if self.link.needs_checksums() { Checksum::Both } else { Checksum::None };
        let mut checksum = ChecksumCapabilities::default();
        checksum.ipv4 = both;
        checksum.icmpv4 = both;
        checksum.icmpv6 = both;
        checksum.udp = both;
        checksum.tcp = both;
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
        let hardware = match link.medium() {
            Medium::Ethernet => HardwareAddress::Ethernet(EthernetAddress(link.mac())),
            Medium::Ip => HardwareAddress::Ip,
        };
        let mut config = Config::new(hardware);
        config.random_seed = seed;
        let counted = Counted::default();
        let mut phy = Phy::new(&mut link, &mut *receive, &mut *transmit, &counted);
        let iface = Interface::new(config, &mut phy, instant(now_ns));
        let sockets = SocketSet::new(sockets);
        let ipv6 = ipv6::Ipv6::new(link.mac(), seed.rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15);
        Stack {
            link,
            iface,
            sockets,
            receive,
            transmit,
            gateway: None,
            ipv6,
            moved: false,
            counted,
        }
    }

    /// Gives the interface `address` (with its prefix) as its only IPv4
    /// address, and `gateway` as its default route, or none.
    pub fn set_ipv4(&mut self, address: Ipv4Cidr, gateway: Option<Ipv4Address>) {
        self.configure(&[address], gateway);
    }

    /// Gives the interface `addresses` as its IPv4 addresses, as many as
    /// fit (8), the first its own for what it sends, and `gateway` as its
    /// default route, or none (radix-network(2)'s `configure`). The
    /// connections and sockets of addresses it no longer has stay, and
    /// what they send goes nowhere, as on Linux.
    pub fn configure(&mut self, addresses: &[Ipv4Cidr], gateway: Option<Ipv4Address>) {
        self.gateway = gateway;
        self.set_addresses(Some(addresses));
        let routes = self.iface.routes_mut();
        routes.remove_default_ipv4_route();
        if let Some(gateway) = gateway {
            // The table has room for a default route once it is removed.
            let _ = routes.add_default_ipv4_route(gateway);
        }
    }

    /// The interface's IPv4 address, with its prefix, if it has one: the
    /// first, which it sends from.
    pub fn ipv4(&self) -> Option<Ipv4Cidr> {
        self.ipv4_addresses().next()
    }

    /// Gives smoltcp the interface's addresses: `v4`, or the IPv4 ones it
    /// has for `None`, then the IPv6 ones that are the interface's, in the
    /// order it is to choose among them, as many as fit (16).
    fn set_addresses(&mut self, v4: Option<&[Ipv4Cidr]>) {
        let mut kept = [Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0); 8];
        let mut count = 0;
        if v4.is_none() {
            for (slot, cidr) in kept.iter_mut().zip(self.ipv4_addresses()) {
                *slot = cidr;
                count += 1;
            }
        }
        let v4 = v4.unwrap_or(&kept[..count]);
        let v6 = self.ipv6.ordered();
        self.iface.update_ip_addrs(|addrs| {
            addrs.clear();
            let v4 = v4.iter().map(|&cidr| IpCidr::Ipv4(cidr));
            let v6 = v6.iter().flatten().map(|&cidr| IpCidr::Ipv6(cidr));
            for cidr in v4.chain(v6) {
                if addrs.push(cidr).is_err() {
                    break;
                }
            }
        });
    }

    /// Gives smoltcp the IPv6 addresses and default route the interface
    /// has now, if they changed.
    fn sync_ipv6(&mut self) -> bool {
        if !self.ipv6.take_changed() {
            return false;
        }
        self.moved = true;
        self.set_addresses(None);
        let routes = self.iface.routes_mut();
        routes.remove_default_ipv6_route();
        if let Some(router) = self.ipv6.router() {
            let _ = routes.add_default_ipv6_route(router);
        }
        true
    }

    /// The interface's IPv4 addresses, with their prefixes, the first
    /// first.
    pub fn ipv4_addresses(&self) -> impl Iterator<Item = Ipv4Cidr> + '_ {
        self.iface.ip_addrs().iter().filter_map(|cidr| match *cidr {
            IpCidr::Ipv4(cidr) => Some(cidr),
            IpCidr::Ipv6(_) => None,
        })
    }

    /// Sets how the interface makes its IPv6 addresses, or turns IPv6 off
    /// on it for `None`, and gives it `addresses` besides, tested before
    /// they are its, and `router` as its default route while no router
    /// advertises one (radix-network(2)'s `configure_ipv6`).
    pub fn configure_ipv6(
        &mut self,
        settings: Option<ipv6::Settings>,
        addresses: &[Ipv6Cidr],
        router: Option<Ipv6Address>,
    ) {
        self.ipv6.configure(settings, addresses, router);
        self.sync_ipv6();
    }

    /// Gives the interface `addresses` in place of those it was given,
    /// and `router`, keeping how it makes its own IPv6 addresses and those
    /// it made (`configure_ipv6`'s given_only).
    pub fn set_ipv6_given(&mut self, addresses: &[Ipv6Cidr], router: Option<Ipv6Address>) {
        self.ipv6.set_given(addresses, router);
        self.sync_ipv6();
    }

    /// Gives an interface with no neighbours, the loopback one,
    /// `addresses` as its IPv6 addresses, as they are.
    pub fn set_ipv6_addresses(&mut self, addresses: &[Ipv6Cidr]) {
        self.ipv6.assign(addresses);
        self.sync_ipv6();
    }

    /// Whether its addresses changed since the last call, which clears
    /// it: those it makes for itself change as it polls.
    pub fn take_moved(&mut self) -> bool {
        core::mem::take(&mut self.moved)
    }

    /// What happened to its IPv6 since the last call, oldest first.
    pub fn take_ipv6_event(&mut self) -> Option<ipv6::Event> {
        self.ipv6.take_event()
    }

    /// The interface's IPv6 state: its addresses, tested or not, its
    /// routers and the DNS servers they advertised.
    pub fn ipv6(&self) -> &ipv6::Ipv6 {
        &self.ipv6
    }

    /// The interface's IPv6 addresses that are its own, with their
    /// prefixes.
    pub fn ipv6_addresses(&self) -> impl Iterator<Item = Ipv6Cidr> + '_ {
        self.iface.ip_addrs().iter().filter_map(|cidr| match *cidr {
            IpCidr::Ipv6(cidr) => Some(cidr),
            IpCidr::Ipv4(_) => None,
        })
    }

    /// Whether `to` is the broadcast address of one of the interface's
    /// networks, such as 10.0.2.255 on 10.0.2.0/24.
    pub fn is_broadcast(&self, to: Ipv4Address) -> bool {
        self.ipv4_addresses().any(|cidr| cidr.broadcast() == Some(to))
    }

    /// The interface's default route, if it has one.
    pub fn router(&self) -> Option<Ipv4Address> {
        self.gateway
    }

    /// The interface's default IPv6 route, if it has one.
    pub fn router_ipv6(&self) -> Option<Ipv6Address> {
        self.ipv6.router()
    }

    /// Gives the interface `addresses` as its IPv4 addresses, as many as
    /// fit (8), the first first, and no default route: the loopback
    /// interface's, which has the cards' addresses too.
    pub fn set_ipv4_addresses(&mut self, addresses: &[Ipv4Cidr]) {
        self.configure(addresses, None);
    }

    /// The interface's MAC address: `None` on an IP link, which has none.
    pub fn mac(&self) -> Option<EthernetAddress> {
        // smoltcp asks for an IP link's hardware address no further.
        match self.link.medium() {
            Medium::Ethernet => Some(EthernetAddress(self.link.mac())),
            Medium::Ip => None,
        }
    }

    /// Gives the interface the MAC address `mac`, which its link reports
    /// from now on too: a card whose driver program attached it gives its
    /// own (ADR 0198). An IP link has none.
    pub fn set_mac(&mut self, mac: [u8; 6]) {
        if self.link.medium() == Medium::Ethernet {
            self.iface.set_hardware_addr(HardwareAddress::Ethernet(EthernetAddress(mac)));
            self.ipv6.set_mac(mac);
            self.sync_ipv6();
        }
    }

    /// What the interface has sent and received since it started.
    pub fn counters(&self) -> Counters {
        self.counted.get()
    }

    /// Each of the stack's TCP and UDP sockets, as `/proc/net/tcp` and
    /// `/proc/net/udp` list them, in the order of its slots.
    pub fn sockets(&self) -> impl Iterator<Item = Shown> + '_ {
        self.sockets.iter().filter_map(|(_, socket)| Shown::of(socket))
    }

    /// Whether `address` is one of the interface's own.
    pub fn has_address(&self, address: impl Into<IpAddress>) -> bool {
        self.iface.has_ip_addr(address.into())
    }

    /// Whether `to` is on one of the interface's networks: for IPv6, a
    /// link-local or link-scoped multicast address is on every
    /// interface's that has IPv6.
    pub fn reaches(&self, to: impl Into<IpAddress>) -> bool {
        let to = to.into();
        if let IpAddress::Ipv6(to) = to
            && (ipv6::link_local(&to) || (to.is_multicast() && to.segments()[0] & 0xf <= 2))
        {
            return self.ipv6_addresses().next().is_some();
        }
        self.iface.ip_addrs().iter().any(|cidr| cidr.contains_addr(&to))
    }

    /// Whether the interface can send to `to`: it has an address of its
    /// version of IP, and `to` is on its network or it has a router.
    pub fn routes_to(&self, to: impl Into<IpAddress>) -> bool {
        let to = to.into();
        self.reaches(to)
            || match to {
                IpAddress::Ipv4(_) => self.ipv4().is_some() && self.gateway.is_some(),
                IpAddress::Ipv6(_) => {
                    self.ipv6_addresses().any(|cidr| !ipv6::link_local(&cidr.address()))
                        && self.ipv6.router().is_some()
                }
            }
    }

    /// The longest IP packet the interface sends, its MTU.
    pub fn mtu(&self) -> usize {
        match self.link.medium() {
            Medium::Ethernet => self.transmit.len().min(FRAME).saturating_sub(ETHERNET_HEADER),
            Medium::Ip => self.transmit.len(),
        }
    }

    /// The card under the stack.
    pub fn link(&mut self) -> &mut L {
        &mut self.link
    }

    /// Does what is due: takes the frames the card received, answers and
    /// passes them on to sockets, sends what sockets have to send and
    /// what timers call for. Returns whether a socket's state may have
    /// changed. On a link whose frames come from the stack itself, it goes
    /// on while they do, a few rounds at most, so that a packet and its
    /// answer go through in one call.
    pub fn poll(&mut self, now_ns: u64) -> bool {
        let mut changed = false;
        for _ in 0..POLL_ROUNDS {
            let mut phy =
                Phy::new(&mut self.link, &mut *self.receive, &mut *self.transmit, &self.counted);
            if phy.link.medium() == Medium::Ethernet {
                phy.ipv6 = Some(&mut self.ipv6);
                phy.now = now_ns;
            }
            let result = self.iface.poll(instant(now_ns), &mut phy, &mut self.sockets);
            self.link.flush();
            changed |= result == PollResult::SocketStateChanged;
            if !self.link.pending() {
                break;
            }
        }
        if self.ipv6.enabled() && self.link.medium() == Medium::Ethernet {
            let (link, counted) = (&mut self.link, &self.counted);
            self.ipv6.poll(now_ns, &mut |frame| {
                let sent = link.can_send() && link.send(frame);
                if sent {
                    counted.sent(frame.len());
                } else {
                    counted.dropped();
                }
                sent
            });
            self.link.flush();
        }
        // New addresses may let sockets send what waited for one.
        changed | self.sync_ipv6()
    }

    /// How long until [`Stack::poll`] has something to do even if no
    /// frame comes, in nanoseconds: `None` when only a frame can give it
    /// work, 0 when it has some now, as when the link has frames of the
    /// stack's own to receive.
    pub fn poll_delay(&mut self, now_ns: u64) -> Option<u64> {
        if self.link.pending() {
            return Some(0);
        }
        let ipv6 = self.ipv6.next_due().map(|due| due.saturating_sub(now_ns));
        let sockets = self
            .iface
            .poll_delay(instant(now_ns), &self.sockets)
            .map(|delay| delay.total_micros().saturating_mul(1_000));
        match (ipv6, sockets) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Adds `socket` to the stack, or gives it back if every socket slot
    /// is taken.
    ///
    /// # Errors
    ///
    /// `socket`, when the stack has no room for it.
    pub fn add_socket<S: AnySocket<'static>>(&mut self, socket: S) -> Result<SocketHandle, S> {
        self.sockets.try_add(socket)
    }

    /// How many sockets the stack has slots for.
    pub fn socket_slots(&self) -> usize {
        self.sockets.capacity()
    }

    /// Moves the stack's sockets into `slots`, more of them and empty,
    /// each to the same place, so that their handles stay, and gives back
    /// the slots they were in, empty, to take the memory of.
    ///
    /// # Errors
    ///
    /// `slots`, back, if they are not more than the stack has, or not
    /// empty.
    pub fn grow_sockets(
        &mut self,
        slots: &'static mut [SocketStorage<'static>],
    ) -> Result<&'static mut [SocketStorage<'static>], &'static mut [SocketStorage<'static>]> {
        self.sockets.grow(slots).map(lent)
    }

    /// The socket `handle` of the stack: `None` if the stack has no such
    /// socket, or it is of another kind. smoltcp's own `get_mut` panics
    /// then; the kernel's calls do not (ADR 0182).
    pub fn socket_mut<S: AnySocket<'static>>(&mut self, handle: SocketHandle) -> Option<&mut S> {
        self.sockets.try_get_mut(handle)
    }

    /// Takes the socket `handle` out of the stack, with the buffers it was
    /// lent: `None` if the stack has no such socket.
    pub fn remove_socket(&mut self, handle: SocketHandle) -> Option<Socket<'static>> {
        self.sockets.try_remove(handle)
    }
}

/// The most rounds one [`Stack::poll`] goes on for the link's own
/// frames.
const POLL_ROUNDS: usize = 8;

/// The slice a buffer was lent, given back. smoltcp is built without
/// `alloc`, so a buffer has no other kind of storage.
fn lent<T>(storage: ManagedSlice<'static, T>) -> &'static mut [T] {
    match storage {
        ManagedSlice::Borrowed(slice) => slice,
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

impl PingMemory {
    /// The memory `socket` was made with, back.
    fn of(socket: icmp::Socket<'static>) -> PingMemory {
        let (receive, transmit) = socket.into_buffers();
        let (receive_meta, receive) = receive.into_storage();
        let (transmit_meta, transmit) = transmit.into_storage();
        PingMemory {
            receive_meta: lent(receive_meta),
            receive: lent(receive),
            transmit_meta: lent(transmit_meta),
            transmit: lent(transmit),
        }
    }
}

/// What [`Pinger::send`] can fail with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PingError {
    /// The socket's buffer for requests is full: poll and try again.
    Busy,
    /// The interface has no IPv4 address to send from.
    NoAddress,
    /// The pinger's socket is not on the stack it was given: it was made
    /// on another.
    NotHere,
}

/// Sends ICMP echo requests ("pings") and matches their replies, until
/// [`Pinger::close`] gives its memory back.
pub struct Pinger {
    handle: SocketHandle,
    ident: u16,
}

/// The bytes after the echo header: the request's own number, so that a
/// reply that echoes another's data is not taken for it.
const PING_DATA: usize = 8;

impl Pinger {
    /// A pinger on `stack` whose requests carry the identifier `ident`.
    ///
    /// # Errors
    ///
    /// `memory`, back, if the stack has no socket slot left.
    pub fn new<L: Link>(
        stack: &mut Stack<L>,
        memory: PingMemory,
        ident: u16,
    ) -> Result<Pinger, PingMemory> {
        let PingMemory { receive_meta, receive, transmit_meta, transmit } = memory;
        let mut socket = icmp::Socket::new(
            icmp::PacketBuffer::new(receive_meta, receive),
            icmp::PacketBuffer::new(transmit_meta, transmit),
        );
        // An unbound socket cannot be bound wrongly: the endpoint is set.
        if socket.bind(icmp::Endpoint::Ident(ident)).is_err() {
            return Err(PingMemory::of(socket));
        }
        match stack.add_socket(socket) {
            Ok(handle) => Ok(Pinger { handle, ident }),
            Err(socket) => Err(PingMemory::of(socket)),
        }
    }

    /// Takes the pinger's socket out of `stack`, and gives back the memory
    /// it was lent.
    ///
    /// # Errors
    ///
    /// The pinger, back, if `stack` has no ICMP socket where its was: it
    /// is not the stack the pinger was made on.
    pub fn close<L: Link>(self, stack: &mut Stack<L>) -> Result<PingMemory, Pinger> {
        // Another kind of socket in the slot is another's, and stays.
        if stack.socket_mut::<icmp::Socket<'static>>(self.handle).is_none() {
            return Err(self);
        }
        match stack.remove_socket(self.handle) {
            Some(Socket::Icmp(socket)) => Ok(PingMemory::of(socket)),
            // The slot held an ICMP socket, looked at above.
            _ => Err(self),
        }
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
        let socket =
            stack.socket_mut::<icmp::Socket<'static>>(self.handle).ok_or(PingError::NotHere)?;
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
        let socket = stack.socket_mut::<icmp::Socket<'static>>(self.handle)?;
        while let Ok((payload, from)) = socket.recv() {
            let IpAddress::Ipv4(from) = from else { continue };
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
