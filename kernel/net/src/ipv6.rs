// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! IPv6 addresses for an Ethernet interface: its link-local address, the
//! addresses it makes itself from the prefixes routers advertise
//! (SLAAC, RFC 4862), and the default routes, DNS servers and MTU they
//! advertise with them.
//!
//! smoltcp answers neighbour solicitations and echo requests, and keeps
//! the neighbour cache; this module does what it leaves out. It reads
//! every router advertisement the card receives, with all of its prefix
//! options and its RDNSS option (RFC 8106), which smoltcp does not keep,
//! and sends router solicitations of its own (RFC 4861, 6.3.7).
//!
//! An address the interface makes for itself is one of two kinds:
//!
//! - A stable address (RFC 7217, and RFC 8064 for its link-local
//!   address): the same on the same network every time, but different on
//!   each network and from the card's MAC address, so that it does not
//!   follow the machine from network to network. Its interface
//!   identifier is the first 64 bits of SHA-256 over the prefix, the
//!   card's MAC address, the duplicate address counter and a secret key
//!   that `netd` keeps for the machine.
//! - A temporary address (RFC 8981): a random interface identifier,
//!   preferred for a day and valid for two, and a new one made before it
//!   is deprecated, so that what the machine connects to cannot tell
//!   one day's connections from the next. The interface sends from a
//!   temporary address when it has one, as RFC 8981 recommends.
//!
//! Every address is tested before it is used (duplicate address
//! detection, RFC 4862, 5.4): one neighbour solicitation from the
//! unspecified address, then a second's wait for anyone who says it is
//! theirs. A stable address that is taken is made again with the next
//! counter, three times at most (RFC 7217, 6); a temporary one, with a
//! new random identifier.
//!
//! Joining the solicited-node multicast groups by MLD, which only
//! switches that snoop MLD need, is left out.

use sha2::{Digest, Sha256};
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, Icmpv6Packet, Icmpv6Repr,
    IpProtocol, Ipv6Address, Ipv6Cidr, Ipv6Packet, Ipv6Repr, NdiscRepr, RawHardwareAddress,
};

/// One second, in nanoseconds.
const SECOND: u64 = 1_000_000_000;
/// One day.
const DAY: u64 = 86_400 * SECOND;

/// How long a temporary address is valid, at most (RFC 8981, 3.8:
/// TEMP_VALID_LIFETIME).
pub const TEMP_VALID: u64 = 2 * DAY;
/// How long it is preferred, at most (TEMP_PREFERRED_LIFETIME).
pub const TEMP_PREFERRED: u64 = DAY;
/// How long before a temporary address is deprecated its successor is
/// made (REGEN_ADVANCE: 2 s, and a duplicate address test of 1 s for each
/// of the three tries).
const REGEN_ADVANCE: u64 = 5 * SECOND;
/// The most a temporary address's preferred lifetime is cut by, at
/// random, so that machines do not make theirs at once (MAX_DESYNC_FACTOR,
/// 0.4 of TEMP_PREFERRED_LIFETIME).
const MAX_DESYNC: u64 = TEMP_PREFERRED / 10 * 4;

/// How long a duplicate address test waits for an answer
/// (RetransTimer, RFC 4861).
const RETRANS: u64 = SECOND;
/// How many times a stable address is made again when it is taken
/// (IDGEN_RETRIES, RFC 7217), or a temporary one (TEMP_IDGEN_RETRIES).
const IDGEN_RETRIES: u8 = 3;

/// Router solicitations: how many, and how far apart (RFC 4861, 10).
const MAX_RTR_SOLICITATIONS: u8 = 3;
const RTR_SOLICITATION_INTERVAL: u64 = 4 * SECOND;

/// A prefix's valid lifetime is shortened by an advertisement only to
/// two hours, unless what it had left is shorter (RFC 4862, 5.5.3 e).
const TWO_HOURS: u64 = 7200 * SECOND;

/// The prefixes the interface makes addresses from, at most.
pub const PREFIXES: usize = 3;
/// The routers it keeps, at most.
pub const ROUTERS: usize = 2;
/// The DNS servers it keeps from advertisements, at most.
pub const DNS_SERVERS: usize = 3;
/// Its IPv6 addresses, at most: the link-local one, and a stable and two
/// temporary ones for each prefix, and as many static ones as fit.
pub const ADDRESSES: usize = 12;

/// The all-nodes multicast address, which routers advertise to.
pub const ALL_NODES: Ipv6Address = Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
/// The all-routers multicast address, which hosts solicit.
pub const ALL_ROUTERS: Ipv6Address = Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 2);

/// How an interface makes its IPv6 addresses: what `netd` gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The secret key stable addresses are made with: the machine's, and
    /// the same at every boot, so that they are.
    pub secret: [u8; 16],
    /// Whether it makes addresses from the prefixes routers advertise,
    /// and takes their default routes.
    pub autoconf: bool,
    /// Whether it makes temporary addresses too, and sends from them.
    pub temporary: bool,
}

/// What an IPv6 address of the interface is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Its link-local address, stable.
    LinkLocal,
    /// A stable address made from a router's prefix.
    Stable,
    /// A temporary address made from a router's prefix.
    Temporary,
    /// An address it was given.
    Static,
}

/// Where an address is in its duplicate address test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Test {
    /// Its neighbour solicitation is to be sent.
    Due,
    /// It was sent; the address is the interface's at `until` if nobody
    /// has said it is theirs by then.
    Waiting { until: u64 },
    /// It passed: the address is the interface's.
    Passed,
}

/// One of the interface's IPv6 addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    /// The address, with its prefix length.
    pub cidr: Ipv6Cidr,
    /// What it is.
    pub kind: Kind,
    /// When it stops being preferred for new connections, if it does.
    pub preferred_until: Option<u64>,
    /// When it goes, if it does.
    pub valid_until: Option<u64>,
    /// The duplicate address counter it was made with, or for a
    /// temporary address the tries so far.
    tries: u8,
    test: Test,
}

impl Address {
    /// Whether the address is the interface's: its duplicate address test
    /// passed.
    pub fn usable(&self) -> bool {
        self.test == Test::Passed
    }

    /// Whether it is still being tested (tentative, as Linux says).
    pub fn tentative(&self) -> bool {
        !self.usable()
    }

    /// Whether it is deprecated at `now`: valid, but not for new
    /// connections.
    pub fn deprecated(&self, now: u64) -> bool {
        self.preferred_until.is_some_and(|until| until <= now)
    }
}

/// A router the interface heard from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Router {
    /// Its link-local address.
    pub address: Ipv6Address,
    /// When its advertisement's router lifetime ends.
    pub until: u64,
}

/// A prefix a router advertised for addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Prefix {
    /// The prefix, a /64.
    cidr: Ipv6Cidr,
    preferred_until: u64,
    valid_until: u64,
}

/// A DNS server a router advertised (RDNSS).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DnsServer {
    /// Its address.
    pub address: Ipv6Address,
    /// When the advertisement's lifetime for it ends.
    pub until: u64,
}

/// A router advertisement, as read from a frame: only what this module
/// keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advert {
    router: Ipv6Address,
    lifetime: u64,
    /// The M and O flags: addresses, or only other settings, by DHCPv6.
    managed: bool,
    other: bool,
    prefixes: [Option<PrefixOption>; PREFIXES],
    dns: [Option<Ipv6Address>; DNS_SERVERS],
    dns_lifetime: u64,
    mtu: Option<u32>,
}

/// A prefix information option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PrefixOption {
    cidr: Ipv6Cidr,
    /// Whether addresses may be made from it (the A flag).
    autonomous: bool,
    valid: u64,
    preferred: u64,
}

/// What happened to the interface's IPv6, for the kernel's log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// An address passed its test and is the interface's.
    Assigned(Ipv6Cidr, Kind),
    /// Another machine has an address it tested, which it gave up.
    Duplicate(Ipv6Address, Kind),
    /// An address's valid lifetime ended.
    Expired(Ipv6Address, Kind),
    /// A router advertised itself.
    Router(Ipv6Address),
    /// A router's lifetime ended, or it said it is no router.
    RouterGone(Ipv6Address),
}

/// The events kept until they are taken, at most.
const EVENTS: usize = 8;

/// Something a received frame says that this module looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Heard {
    /// A router advertisement.
    Advert(Advert),
    /// A neighbour advertisement for `0`, or a solicitation for it from
    /// the unspecified address: someone else has it, or is testing it.
    Claimed(Ipv6Address),
}

/// Reads what an Ethernet frame says that this module looks for: a
/// router advertisement, or a claim on an address.
pub fn hear(frame: &[u8]) -> Option<Heard> {
    let ethernet = EthernetFrame::new_checked(frame).ok()?;
    if ethernet.ethertype() != EthernetProtocol::Ipv6 {
        return None;
    }
    let ip = Ipv6Packet::new_checked(ethernet.payload()).ok()?;
    let ip = Ipv6Repr::parse(&ip).ok().map(|repr| (repr, ip))?;
    let (repr, packet) = ip;
    // Neighbour discovery messages come from the link: a hop limit of
    // 255, with no extension header before ICMPv6.
    if repr.next_header != IpProtocol::Icmpv6 || repr.hop_limit != 255 {
        return None;
    }
    let payload = packet.payload();
    let icmp = Icmpv6Packet::new_checked(&payload[..payload.len().min(repr.payload_len)]).ok()?;
    if !icmp.verify_checksum(&repr.src_addr, &repr.dst_addr) {
        return None;
    }
    match icmp.msg_type().into() {
        // Router advertisement: read here, with every option.
        134u8 => advert(&repr, icmp.into_inner()).map(Heard::Advert),
        135 | 136 => {
            let ndisc = NdiscRepr::parse(&icmp).ok()?;
            match ndisc {
                NdiscRepr::NeighborAdvert { target_addr, .. } => Some(Heard::Claimed(target_addr)),
                NdiscRepr::NeighborSolicit { target_addr, .. }
                    if repr.src_addr == Ipv6Address::UNSPECIFIED =>
                {
                    Some(Heard::Claimed(target_addr))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Reads a router advertisement's message, from its ICMPv6 type on.
fn advert(ip: &Ipv6Repr, message: &[u8]) -> Option<Advert> {
    // RFC 4861, 6.1.2: from a link-local address, with code 0.
    if !link_local(&ip.src_addr) || message.len() < 16 || message[1] != 0 {
        return None;
    }
    let flags = message[5];
    let lifetime = u64::from(u16::from_be_bytes([message[6], message[7]])) * SECOND;
    let mut advert = Advert {
        router: ip.src_addr,
        lifetime,
        managed: flags & 0x80 != 0,
        other: flags & 0x40 != 0,
        prefixes: [None; PREFIXES],
        dns: [None; DNS_SERVERS],
        dns_lifetime: 0,
        mtu: None,
    };
    let mut options = &message[16..];
    while options.len() >= 2 {
        let len = usize::from(options[1]) * 8;
        if len == 0 || len > options.len() {
            // A zero length makes the whole message invalid (4.6).
            return None;
        }
        let (option, rest) = options.split_at(len);
        match option[0] {
            // Prefix information (4.6.2).
            3 if len == 32 => {
                let prefix_len = option[2];
                let on = option[3];
                let valid = lifetime_of(&option[4..8]);
                let preferred = lifetime_of(&option[8..12]);
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&option[16..32]);
                let cidr = Ipv6Cidr::new(Ipv6Address::from(octets), prefix_len);
                let option = PrefixOption { cidr, autonomous: on & 0x40 != 0, valid, preferred };
                if let Some(slot) = advert.prefixes.iter_mut().find(|slot| slot.is_none()) {
                    *slot = Some(option);
                }
            }
            // MTU (4.6.4).
            5 if len == 8 => {
                advert.mtu = Some(u32::from_be_bytes([option[4], option[5], option[6], option[7]]));
            }
            // Recursive DNS servers (RFC 8106, 5.1).
            25 if len >= 24 => {
                advert.dns_lifetime = lifetime_of(&option[4..8]);
                let servers = option[8..].as_chunks::<16>().0;
                for (slot, server) in advert.dns.iter_mut().zip(servers) {
                    *slot = Some(Ipv6Address::from(*server));
                }
            }
            _ => {}
        }
        options = rest;
    }
    Some(advert)
}

/// A lifetime in seconds, as an option gives it, in nanoseconds: all ones
/// is forever.
fn lifetime_of(bytes: &[u8]) -> u64 {
    let seconds = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if seconds == u32::MAX { u64::MAX } else { u64::from(seconds) * SECOND }
}

/// `now` and `lifetime` later, saturating: forever stays forever.
fn after(now: u64, lifetime: u64) -> u64 {
    now.saturating_add(lifetime)
}

/// Whether `address` is link-local unicast, fe80::/10.
pub fn link_local(address: &Ipv6Address) -> bool {
    address.segments()[0] & 0xffc0 == 0xfe80
}

/// The solicited-node multicast address of `address` (RFC 4291, 2.7.1).
fn solicited_node(address: &Ipv6Address) -> Ipv6Address {
    let o = address.octets();
    Ipv6Address::new(
        0xff02,
        0,
        0,
        0,
        0,
        1,
        0xff00 | u16::from(o[13]),
        u16::from_be_bytes([o[14], o[15]]),
    )
}

/// The Ethernet multicast address of IPv6 multicast address `address`
/// (RFC 2464, 7).
fn multicast_mac(address: &Ipv6Address) -> EthernetAddress {
    let o = address.octets();
    EthernetAddress([0x33, 0x33, o[12], o[13], o[14], o[15]])
}

/// Whether interface identifier `iid` is one that may not be used
/// (RFC 5453): all zeros, the subnet-router anycast one, the reserved
/// anycast ones, or the proxy-mobile one.
fn reserved(iid: [u8; 8]) -> bool {
    let value = u64::from_be_bytes(iid);
    value == 0
        || (0xfdff_ffff_ffff_ff80..=0xfdff_ffff_ffff_ffff).contains(&value)
        || (0x0200_5eff_fe00_0000..=0x0200_5eff_fe00_5212).contains(&value)
        || value == 0x0200_5eff_fe00_5213
        || (0x0200_5eff_fe00_5214..=0x0200_5eff_feff_ffff).contains(&value)
}

/// The address with prefix `prefix` and interface identifier `iid`.
fn with_iid(prefix: &Ipv6Cidr, iid: [u8; 8]) -> Ipv6Cidr {
    let mut octets = prefix.address().octets();
    octets[8..].copy_from_slice(&iid);
    Ipv6Cidr::new(Ipv6Address::from(octets), prefix.prefix_len())
}

/// The stable interface identifier of RFC 7217, 5: the first 64 bits of
/// SHA-256 over the prefix, the interface (its MAC address), the network
/// (none: RFC 7217 makes it optional), the duplicate address counter and
/// the secret key.
pub fn stable_iid(prefix: &Ipv6Cidr, mac: [u8; 6], counter: u8, secret: &[u8; 16]) -> [u8; 8] {
    let mut hash = Sha256::new();
    hash.update(&prefix.address().octets()[..8]);
    hash.update(mac);
    hash.update([counter]);
    hash.update(secret);
    let digest = hash.finalize();
    let mut iid = [0u8; 8];
    iid.copy_from_slice(&digest[..8]);
    iid
}

/// A small random number generator for temporary interface identifiers
/// and desynchronization: xorshift64*, seeded from the kernel's.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// The IPv6 state of one interface.
pub struct Ipv6 {
    mac: [u8; 6],
    /// Whether the interface has IPv6 at all.
    enabled: bool,
    settings: Option<Settings>,
    /// The default router it was given, which one advertised goes before.
    given_router: Option<Ipv6Address>,
    addresses: [Option<Address>; ADDRESSES],
    prefixes: [Option<Prefix>; PREFIXES],
    routers: [Option<Router>; ROUTERS],
    dns: [Option<DnsServer>; DNS_SERVERS],
    mtu: Option<u32>,
    managed: bool,
    other: bool,
    /// Router solicitations sent, and when the next is due.
    solicited: u8,
    solicit_at: u64,
    random: Random,
    /// The cut taken from temporary addresses' preferred lifetimes.
    desync: u64,
    /// Whether the addresses or routes changed since smoltcp's were set.
    changed: bool,
    /// When [`Ipv6::poll`] last ran.
    last: u64,
    /// What happened, oldest first, until taken.
    events: [Option<Event>; EVENTS],
}

impl Ipv6 {
    /// No IPv6 on the card with MAC address `mac` until it is given
    /// [`Settings`]; `seed` is a random number.
    pub fn new(mac: [u8; 6], seed: u64) -> Ipv6 {
        let mut random = Random(seed | 1);
        let desync = random.next() % MAX_DESYNC;
        Ipv6 {
            mac,
            enabled: false,
            settings: None,
            given_router: None,
            addresses: [None; ADDRESSES],
            prefixes: [None; PREFIXES],
            routers: [None; ROUTERS],
            dns: [None; DNS_SERVERS],
            mtu: None,
            managed: false,
            other: false,
            solicited: 0,
            solicit_at: 0,
            random,
            desync,
            changed: false,
            last: 0,
            events: [None; EVENTS],
        }
    }

    /// Sets how the interface makes its addresses, or turns its IPv6 off
    /// with `None`, and gives it `addresses` besides, and `router` as its
    /// default route while no router advertises one. What it made before
    /// is dropped and made again: a new key makes new addresses.
    pub fn configure(
        &mut self,
        settings: Option<Settings>,
        addresses: &[Ipv6Cidr],
        router: Option<Ipv6Address>,
    ) {
        let mac = self.mac;
        let seed = self.random.next();
        let last = self.last;
        let events = self.events;
        *self = Ipv6::new(mac, seed);
        self.last = last;
        self.events = events;
        self.settings = settings;
        self.changed = true;
        let Some(settings) = settings else { return };
        self.enabled = true;
        self.given_router = router;
        let link = Ipv6Cidr::new(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 64);
        self.add_stable(&link, Kind::LinkLocal, 0, &settings.secret, None, None);
        for &cidr in addresses {
            self.add(Address {
                cidr,
                kind: Kind::Static,
                preferred_until: None,
                valid_until: None,
                tries: 0,
                test: Test::Due,
            });
        }
    }

    /// Gives an interface that has no neighbours, the loopback one,
    /// `addresses` as its IPv6 addresses, with no test: they are its at
    /// once.
    pub fn assign(&mut self, addresses: &[Ipv6Cidr]) {
        self.enabled = true;
        self.addresses = [None; ADDRESSES];
        for &cidr in addresses {
            self.add(Address {
                cidr,
                kind: Kind::Static,
                preferred_until: None,
                valid_until: None,
                tries: 0,
                test: Test::Passed,
            });
        }
        self.changed = true;
    }

    /// Whether IPv6 is on.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// How it makes its addresses, if it does.
    pub fn settings(&self) -> Option<Settings> {
        self.settings
    }

    /// The card's MAC address changed to `mac`: a driver program gave
    /// its own (ADR 0198). The stable addresses depend on it, so they
    /// are made again.
    pub fn set_mac(&mut self, mac: [u8; 6]) {
        if mac == self.mac {
            return;
        }
        self.mac = mac;
        if self.settings.is_none() {
            return;
        }
        let mut given = [Ipv6Cidr::new(Ipv6Address::UNSPECIFIED, 0); ADDRESSES];
        let mut count = 0;
        for address in self.addresses.iter().flatten().filter(|a| a.kind == Kind::Static) {
            given[count] = address.cidr;
            count += 1;
        }
        self.configure(self.settings, &given[..count], self.given_router);
    }

    /// Keeps `event` to be taken, if there is room.
    fn note(&mut self, event: Event) {
        if let Some(slot) = self.events.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(event);
        }
    }

    /// The oldest event not yet taken.
    pub fn take_event(&mut self) -> Option<Event> {
        let first = self.events[0].take();
        self.events.rotate_left(1);
        first
    }

    /// Adds `address`, if there is room.
    fn add(&mut self, address: Address) {
        if self.addresses.iter().flatten().any(|a| a.cidr.address() == address.cidr.address()) {
            return;
        }
        if let Some(slot) = self.addresses.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(address);
            self.changed = true;
        }
    }

    /// Adds the stable address of `prefix` made with `counter`, or the
    /// next counter that gives no reserved identifier.
    fn add_stable(
        &mut self,
        prefix: &Ipv6Cidr,
        kind: Kind,
        mut counter: u8,
        secret: &[u8; 16],
        preferred_until: Option<u64>,
        valid_until: Option<u64>,
    ) {
        while counter <= IDGEN_RETRIES {
            let iid = stable_iid(prefix, self.mac, counter, secret);
            if !reserved(iid) {
                let cidr = with_iid(prefix, iid);
                self.add(Address {
                    cidr,
                    kind,
                    preferred_until,
                    valid_until,
                    tries: counter,
                    test: Test::Due,
                });
                return;
            }
            counter += 1;
        }
    }

    /// Adds a new temporary address for `prefix` (RFC 8981, 3.4), its
    /// `tries`th, whose lifetimes are its prefix's, cut to a temporary
    /// address's.
    fn add_temporary(&mut self, prefix: &Prefix, tries: u8, now: u64) {
        let iid = loop {
            let iid = self.random.next().to_be_bytes();
            if !reserved(iid) {
                break iid;
            }
        };
        let valid_until = prefix.valid_until.min(after(now, TEMP_VALID));
        let preferred_until =
            prefix.preferred_until.min(after(now, TEMP_PREFERRED.saturating_sub(self.desync)));
        // One whose preferred lifetime would end before its successor is
        // made is not made (3.4, 6).
        if preferred_until <= after(now, REGEN_ADVANCE) {
            return;
        }
        self.add(Address {
            cidr: with_iid(&prefix.cidr, iid),
            kind: Kind::Temporary,
            preferred_until: Some(preferred_until),
            valid_until: Some(valid_until),
            tries,
            test: Test::Due,
        });
    }

    /// Takes in what a received frame said.
    pub fn heard(&mut self, heard: Heard, now: u64) {
        let Some(settings) = self.settings else { return };
        match heard {
            Heard::Claimed(target) => self.claimed(target, &settings, now),
            Heard::Advert(advert) if settings.autoconf => self.advertised(&advert, &settings, now),
            Heard::Advert(_) => {}
        }
    }

    /// Someone else has, or is testing, `target`: an address of ours
    /// still being tested is dropped, and made again if it can be
    /// (RFC 4862, 5.4.5).
    fn claimed(&mut self, target: Ipv6Address, settings: &Settings, now: u64) {
        let Some(slot) = self
            .addresses
            .iter_mut()
            .find(|slot| slot.is_some_and(|a| a.cidr.address() == target && a.tentative()))
        else {
            return;
        };
        let Some(lost) = slot.take() else { return };
        self.changed = true;
        self.note(Event::Duplicate(target, lost.kind));
        let prefix = Ipv6Cidr::new(lost.cidr.address(), lost.cidr.prefix_len());
        match lost.kind {
            Kind::LinkLocal | Kind::Stable => {
                self.add_stable(
                    &prefix,
                    lost.kind,
                    lost.tries + 1,
                    &settings.secret,
                    lost.preferred_until,
                    lost.valid_until,
                );
            }
            Kind::Temporary if lost.tries < IDGEN_RETRIES => {
                let found = self.prefixes.iter().flatten().find(|p| p.cidr.contains_addr(&target));
                if let Some(prefix) = found.copied() {
                    self.add_temporary(&prefix, lost.tries + 1, now);
                }
            }
            Kind::Temporary | Kind::Static => {}
        }
    }

    /// Takes in a router advertisement (RFC 4861, 6.3.4; RFC 4862,
    /// 5.5.3).
    fn advertised(&mut self, advert: &Advert, settings: &Settings, now: u64) {
        // The router: kept, its lifetime renewed, or dropped at 0.
        let found = self.routers.iter().position(|r| r.is_some_and(|r| r.address == advert.router));
        match (found, advert.lifetime) {
            (Some(at), 0) => {
                self.routers[at] = None;
                self.note(Event::RouterGone(advert.router));
            }
            (Some(at), lifetime) => {
                self.routers[at] =
                    Some(Router { address: advert.router, until: after(now, lifetime) });
            }
            (None, 0) => {}
            (None, lifetime) => {
                if let Some(slot) = self.routers.iter_mut().find(|slot| slot.is_none()) {
                    *slot = Some(Router { address: advert.router, until: after(now, lifetime) });
                    self.note(Event::Router(advert.router));
                }
            }
        }
        self.changed = true;
        self.managed = advert.managed;
        self.other = advert.other;
        // RFC 4861, 6.3.4: an MTU below IPv6's least, 1280, is ignored.
        if let Some(mtu) = advert.mtu.filter(|&mtu| mtu >= 1280) {
            self.mtu = Some(mtu);
        }
        for server in advert.dns.iter().flatten() {
            let until = after(now, advert.dns_lifetime);
            match self.dns.iter_mut().find(|s| s.is_some_and(|s| s.address == *server)) {
                Some(slot) if advert.dns_lifetime == 0 => *slot = None,
                Some(slot) => *slot = Some(DnsServer { address: *server, until }),
                None if advert.dns_lifetime == 0 => {}
                None => {
                    if let Some(slot) = self.dns.iter_mut().find(|slot| slot.is_none()) {
                        *slot = Some(DnsServer { address: *server, until });
                    }
                }
            }
        }
        for option in advert.prefixes.iter().flatten() {
            self.prefix(option, settings, now);
        }
    }

    /// Takes in one prefix information option (RFC 4862, 5.5.3).
    fn prefix(&mut self, option: &PrefixOption, settings: &Settings, now: u64) {
        // b, c: not the link-local prefix, and a preferred lifetime no
        // longer than the valid one; only /64s make addresses from 64-bit
        // identifiers (RFC 4291, 2.5.1).
        if !option.autonomous
            || link_local(&option.cidr.address())
            || option.preferred > option.valid
            || option.cidr.prefix_len() != 64
        {
            return;
        }
        let cidr = Ipv6Cidr::new(with_iid(&option.cidr, [0; 8]).address(), 64);
        let at = self.prefixes.iter().position(|p| p.is_some_and(|p| p.cidr == cidr));
        let preferred_until = after(now, option.preferred);
        match at {
            None => {
                // d: a new prefix, with a valid lifetime.
                if option.valid == 0 {
                    return;
                }
                let prefix =
                    Prefix { cidr, preferred_until, valid_until: after(now, option.valid) };
                let Some(slot) = self.prefixes.iter_mut().find(|slot| slot.is_none()) else {
                    return;
                };
                *slot = Some(prefix);
                self.add_stable(
                    &cidr,
                    Kind::Stable,
                    0,
                    &settings.secret,
                    Some(prefix.preferred_until),
                    Some(prefix.valid_until),
                );
                if settings.temporary {
                    self.add_temporary(&prefix, 0, now);
                }
            }
            Some(at) => {
                let Some(prefix) = self.prefixes[at].as_mut() else { return };
                // e: the two-hour rule, against a forged advertisement
                // that would end the prefix's addresses at once.
                let remaining = prefix.valid_until.saturating_sub(now);
                let valid_until = if option.valid > TWO_HOURS || option.valid > remaining {
                    after(now, option.valid)
                } else if remaining <= TWO_HOURS {
                    prefix.valid_until
                } else {
                    after(now, TWO_HOURS)
                };
                prefix.valid_until = valid_until;
                prefix.preferred_until = preferred_until;
                for address in self.addresses.iter_mut().flatten() {
                    if !cidr.contains_addr(&address.cidr.address()) {
                        continue;
                    }
                    match address.kind {
                        Kind::Stable => {
                            address.valid_until = Some(valid_until);
                            address.preferred_until = Some(preferred_until);
                        }
                        // A temporary address's lifetimes only shrink to
                        // its prefix's (RFC 8981, 3.4, 4).
                        Kind::Temporary => {
                            address.valid_until = address.valid_until.map(|u| u.min(valid_until));
                            address.preferred_until =
                                address.preferred_until.map(|u| u.min(preferred_until));
                        }
                        Kind::LinkLocal | Kind::Static => {}
                    }
                }
                self.changed = true;
            }
        }
    }

    /// Does what is due at `now`: drops what has expired, makes new
    /// temporary addresses, sends what tests addresses and asks for
    /// routers, and takes the addresses whose test is over. `send` sends
    /// a frame, and gives `false` if it could not.
    pub fn poll(&mut self, now: u64, send: &mut dyn FnMut(&[u8]) -> bool) {
        let last = core::mem::replace(&mut self.last, now);
        let Some(settings) = self.settings else { return };
        self.expire(now, last);
        if settings.temporary {
            self.regenerate(now);
        }
        for at in 0..ADDRESSES {
            let Some(address) = self.addresses[at] else { continue };
            match address.test {
                Test::Due => {
                    let frame = &mut [0u8; 86];
                    let len = solicitation(frame, self.mac, address.cidr.address());
                    if send(&frame[..len]) {
                        self.set_test(at, Test::Waiting { until: after(now, RETRANS) });
                    }
                }
                Test::Waiting { until } if until <= now => {
                    self.set_test(at, Test::Passed);
                    self.changed = true;
                    self.note(Event::Assigned(address.cidr, address.kind));
                }
                Test::Waiting { .. } | Test::Passed => {}
            }
        }
        // Router solicitations, from the link-local address once it is
        // the interface's (RFC 4861, 6.3.7), until a router answers.
        if let Some(from) = self.soliciting(now) {
            let frame = &mut [0u8; 70];
            let len = router_solicitation(frame, self.mac, from);
            if send(&frame[..len]) {
                self.solicited += 1;
                self.solicit_at = after(now, RTR_SOLICITATION_INTERVAL);
            }
        }
    }

    /// Drops the routers, servers, prefixes and addresses whose lifetimes
    /// ended by `now`; an address deprecated since `last` changes which
    /// the interface sends from.
    fn expire(&mut self, now: u64, last: u64) {
        for at in 0..ROUTERS {
            if let Some(router) = self.routers[at].filter(|r| r.until <= now) {
                self.routers[at] = None;
                self.changed = true;
                self.note(Event::RouterGone(router.address));
            }
        }
        for slot in &mut self.dns {
            if slot.is_some_and(|s| s.until <= now) {
                *slot = None;
            }
        }
        for slot in &mut self.prefixes {
            if slot.is_some_and(|p| p.valid_until <= now) {
                *slot = None;
            }
        }
        for at in 0..ADDRESSES {
            let Some(address) = self.addresses[at] else { continue };
            if address.valid_until.is_some_and(|until| until <= now) {
                self.addresses[at] = None;
                self.changed = true;
                self.note(Event::Expired(address.cidr.address(), address.kind));
            } else if address.preferred_until.is_some_and(|until| until > last && until <= now) {
                self.changed = true;
            }
        }
    }

    /// Makes a new temporary address for each prefix whose newest one is
    /// about to be deprecated, or that has none (RFC 8981, 3.5).
    fn regenerate(&mut self, now: u64) {
        for at in 0..PREFIXES {
            let Some(prefix) = self.prefixes[at] else { continue };
            if prefix.preferred_until <= now {
                continue;
            }
            let newest = self
                .addresses
                .iter()
                .flatten()
                .filter(|a| {
                    a.kind == Kind::Temporary && prefix.cidr.contains_addr(&a.cidr.address())
                })
                .filter_map(|a| a.preferred_until)
                .max();
            if newest.is_none_or(|until| until <= after(now, REGEN_ADVANCE)) {
                self.add_temporary(&prefix, 0, now);
            }
        }
    }

    /// The address to send a router solicitation from at `now`, if one is
    /// due: the link-local one, once it passed its test.
    fn soliciting(&self, now: u64) -> Option<Ipv6Address> {
        let settings = self.settings?;
        if !settings.autoconf
            || self.solicited >= MAX_RTR_SOLICITATIONS
            || self.solicit_at > now
            || self.routers.iter().any(Option::is_some)
        {
            return None;
        }
        self.addresses().find(|a| a.kind == Kind::LinkLocal && a.usable()).map(|a| a.cidr.address())
    }

    fn set_test(&mut self, at: usize, test: Test) {
        if let Some(address) = self.addresses[at].as_mut() {
            address.test = test;
        }
    }

    /// When [`Ipv6::poll`] next has something to do, if ever.
    pub fn next_due(&self) -> Option<u64> {
        self.settings?;
        let mut due = u64::MAX;
        for address in self.addresses() {
            match address.test {
                Test::Due => return Some(0),
                Test::Waiting { until } => due = due.min(until),
                Test::Passed => {}
            }
            if let Some(until) = address.valid_until {
                due = due.min(until);
            }
            if let Some(until) = address.preferred_until.filter(|&until| until > self.last) {
                due = due.min(until);
                if address.kind == Kind::Temporary {
                    due = due.min(until.saturating_sub(REGEN_ADVANCE).max(self.last + 1));
                }
            }
        }
        for router in self.routers() {
            due = due.min(router.until);
        }
        for server in self.dns() {
            due = due.min(server.until);
        }
        for prefix in self.prefixes.iter().flatten() {
            due = due.min(prefix.valid_until);
        }
        if self.solicited < MAX_RTR_SOLICITATIONS
            && self.routers.iter().all(Option::is_none)
            && self.settings.is_some_and(|s| s.autoconf)
            && self.addresses().any(|a| a.kind == Kind::LinkLocal && a.usable())
        {
            due = due.min(self.solicit_at);
        }
        (due != u64::MAX).then_some(due)
    }

    /// Whether the addresses or default route changed since the last
    /// call, which clears it.
    pub fn take_changed(&mut self) -> bool {
        core::mem::take(&mut self.changed)
    }

    /// The interface's IPv6 addresses, tested or not.
    pub fn addresses(&self) -> impl Iterator<Item = &Address> + '_ {
        self.addresses.iter().flatten()
    }

    /// The addresses that are the interface's: those that passed their
    /// test.
    pub fn usable(&self) -> impl Iterator<Item = Ipv6Cidr> + '_ {
        self.addresses().filter(|a| a.usable()).map(|a| a.cidr)
    }

    /// Whether `address` is tentative on the interface: being tested.
    pub fn is_tentative(&self, address: &Ipv6Address) -> bool {
        self.addresses().any(|a| a.cidr.address() == *address && a.tentative())
    }

    /// The default router: the first that is still advertised, else the
    /// one the interface was given.
    pub fn router(&self) -> Option<Ipv6Address> {
        self.routers.iter().flatten().next().map(|r| r.address).or(self.given_router)
    }

    /// The routers heard from.
    pub fn routers(&self) -> impl Iterator<Item = &Router> + '_ {
        self.routers.iter().flatten()
    }

    /// The DNS servers routers advertised.
    pub fn dns(&self) -> impl Iterator<Item = &DnsServer> + '_ {
        self.dns.iter().flatten()
    }

    /// The MTU a router advertised.
    pub fn mtu(&self) -> Option<u32> {
        self.mtu
    }

    /// The M and O flags of the last advertisement: whether DHCPv6 gives
    /// addresses, or only other settings.
    pub fn dhcp(&self) -> (bool, bool) {
        (self.managed, self.other)
    }

    /// The addresses that are the interface's in the order smoltcp is to
    /// have them, which picks the first of those that match a destination
    /// as well: those not deprecated first, temporary ones before stable
    /// ones when the interface makes them (RFC 6724, 5, rules 3 and 7).
    pub fn ordered(&self) -> [Option<Ipv6Cidr>; ADDRESSES] {
        let prefer_temporary = self.settings.is_some_and(|s| s.temporary);
        let mut usable = [None; ADDRESSES];
        let found = self.addresses().filter(|a| a.usable()).enumerate();
        for (slot, (at, address)) in usable.iter_mut().zip(found) {
            *slot = Some((at, *address));
        }
        // Unstable, as core has no other, but by where each was as well.
        usable.sort_unstable_by_key(|a| {
            a.map_or((2, 2, 0), |(at, a)| {
                let deprecated = u8::from(a.deprecated(self.last));
                let temporary = u8::from(!(prefer_temporary && a.kind == Kind::Temporary));
                (deprecated, temporary, at)
            })
        });
        usable.map(|a| a.map(|(_, a)| a.cidr))
    }

    /// The address to send to `to` from (RFC 6724, 5, the rules that
    /// apply to one interface): the same address, then the same scope,
    /// then one not deprecated, then a temporary one if the interface
    /// makes them (rule 7), then the longest common prefix.
    pub fn source(&self, to: &Ipv6Address) -> Option<Ipv6Address> {
        let now = self.last;
        let prefer_temporary = self.settings.is_some_and(|s| s.temporary);
        let to_link = link_local(to) || (to.is_multicast() && to.segments()[0] & 0x000f <= 2);
        let rank = |a: &Address| {
            let address = a.cidr.address();
            let same = address == *to;
            let scope = link_local(&address) == to_link;
            let fresh = !a.deprecated(now);
            let temporary = prefer_temporary && a.kind == Kind::Temporary;
            let common = (u128::from_be_bytes(address.octets()) ^ u128::from_be_bytes(to.octets()))
                .leading_zeros();
            (same, scope, fresh, temporary, common)
        };
        self.addresses()
            .filter(|a| a.usable())
            // A link-local destination can only be reached from a
            // link-local address (RFC 6724, 4).
            .filter(|a| !to_link || link_local(&a.cidr.address()))
            .max_by_key(|a| rank(a))
            .map(|a| a.cidr.address())
    }
}

/// Writes into `frame` the neighbour solicitation that tests whether
/// `target` is free (RFC 4862, 5.4.2): from the unspecified address, to
/// its solicited-node address, with no source link-layer option. Returns
/// its length.
fn solicitation(frame: &mut [u8], mac: [u8; 6], target: Ipv6Address) -> usize {
    let to = solicited_node(&target);
    let icmp = Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit { target_addr: target, lladdr: None });
    emit(frame, mac, Ipv6Address::UNSPECIFIED, to, &icmp)
}

/// Writes into `frame` a router solicitation from `from`, with the card's
/// MAC address in its source link-layer option. Returns its length.
fn router_solicitation(frame: &mut [u8], mac: [u8; 6], from: Ipv6Address) -> usize {
    let lladdr = Some(RawHardwareAddress::from_bytes(&mac));
    let icmp = Icmpv6Repr::Ndisc(NdiscRepr::RouterSolicit { lladdr });
    emit(frame, mac, from, ALL_ROUTERS, &icmp)
}

/// Writes an Ethernet frame carrying `icmp` from `from` to multicast
/// address `to`, with a hop limit of 255. Returns its length.
fn emit(
    frame: &mut [u8],
    mac: [u8; 6],
    from: Ipv6Address,
    to: Ipv6Address,
    icmp: &Icmpv6Repr,
) -> usize {
    let ethernet = EthernetRepr {
        src_addr: EthernetAddress(mac),
        dst_addr: multicast_mac(&to),
        ethertype: EthernetProtocol::Ipv6,
    };
    let ip = Ipv6Repr {
        src_addr: from,
        dst_addr: to,
        next_header: IpProtocol::Icmpv6,
        payload_len: icmp.buffer_len(),
        hop_limit: 255,
    };
    let len = ethernet.buffer_len() + ip.buffer_len() + icmp.buffer_len();
    let frame = &mut frame[..len];
    let mut eth = EthernetFrame::new_unchecked(&mut *frame);
    ethernet.emit(&mut eth);
    let mut packet = Ipv6Packet::new_unchecked(eth.payload_mut());
    ip.emit(&mut packet);
    let mut message = Icmpv6Packet::new_unchecked(packet.payload_mut());
    icmp.emit(&from, &to, &mut message, &ChecksumCapabilities::default());
    len
}
