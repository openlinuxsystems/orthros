// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The DHCPv4 client of netd(8) (RFC 2131, ADR 0191): what a card says to
//! DHCP servers and when, with no I/O of its own, so that the tests can
//! drive it with time of their own.
//!
//! A card with no lease broadcasts a DHCPDISCOVER, requests the first
//! offer it gets, and is bound once the server acknowledges it. It
//! renews the lease from that server at T1, half the lease, asks any
//! server at T2, seven eighths, and gives the address up when the lease
//! ends or a server refuses it (DHCPNAK). A card with a lease from
//! before asks for its address again first (INIT-REBOOT). A message is
//! sent again after 4, 8, 16, 32 and then every 64 seconds, a second
//! either way at random, as RFC 2131 has it; while renewing or
//! rebinding, after half the time left, and at least a minute.
//!
//! The messages are read and written by smoltcp's DHCP parser, the
//! network stack's own (ADR 0180). What a server says is checked before
//! it is used: an address must be one a host may have, a router must be
//! on its network, and a domain name only the letters of one, since it
//! goes into resolv.conf.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use smoltcp::wire::{DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress};

use crate::ip::Cidr;

/// The options a client asks servers for: the subnet mask, the router,
/// the DNS servers, the domain name, the lease's times and the server's
/// identifier.
const REQUESTED: [u8; 8] = [1, 3, 6, 15, 51, 54, 58, 59];
const OPTION_ROUTER: u8 = 3;
const OPTION_DOMAIN: u8 = 15;

/// The shortest message sent, as BOOTP relays take them (RFC 1542).
const MESSAGE_MIN: usize = 300;
/// The longest message a client takes, as it tells servers.
pub const MESSAGE_MAX: u16 = 1500;

/// The tries of a request for an offer, and of one for the lease a card
/// had, before the client starts again.
const REQUEST_TRIES: u32 = 4;
const REBOOT_TRIES: u32 = 2;

/// The shortest lease the client keeps to: a server's shorter one would
/// have it ask again and again.
const LEASE_MIN: u32 = 60;
/// The lease when an acknowledgement gives none.
const LEASE_DEFAULT: u32 = 3600;
/// A lease of this many seconds lasts forever (RFC 2131, 3.3).
const FOREVER: u32 = u32::MAX;

/// While renewing or rebinding, the shortest wait before asking again.
const RETRY_MIN: Duration = Duration::from_secs(60);

/// What a lease gives a card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub address: Cidr,
    pub router: Option<Ipv4Addr>,
    /// Its DNS servers, at most three.
    pub dns: Vec<Ipv4Addr>,
    /// The domain names are looked up in.
    pub domain: Option<String>,
    /// The server that gave it.
    pub server: Ipv4Addr,
    /// How many seconds it lasts, [`FOREVER`] for ever, and after how
    /// many it is renewed and rebound.
    pub seconds: u32,
    pub renew: u32,
    pub rebind: u32,
}

impl Lease {
    /// When it ends, renews and rebinds, if it was given at `given`; none
    /// for a lease that lasts forever.
    fn times(&self, given: Instant) -> Option<[Instant; 3]> {
        (self.seconds != FOREVER).then(|| {
            [self.renew, self.rebind, self.seconds]
                .map(|seconds| given + Duration::from_secs(u64::from(seconds)))
        })
    }
}

/// Where a card is among RFC 2131's states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// About to ask for an offer.
    Init,
    /// Asking for an offer (DHCPDISCOVER).
    Selecting,
    /// Requesting an offer (DHCPREQUEST).
    Requesting,
    /// Asking for the address it had (INIT-REBOOT).
    Rebooting,
    /// Has a lease.
    Bound,
    /// Asking its server for more time.
    Renewing,
    /// Asking any server for more time.
    Rebinding,
}

/// What the client has netd do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send `message` to port 67 of `to`: a server, or 255.255.255.255.
    Send { to: Ipv4Addr, message: Vec<u8> },
    /// Give the card the lease's address, router and DNS servers, in
    /// place of an earlier lease's.
    Bind(Lease),
    /// Take the lease's address away: it ended, or a server refused it.
    Unbind,
}

/// One card's DHCP client.
#[derive(Debug)]
pub struct Client {
    mac: [u8; 6],
    phase: Phase,
    /// The exchange's transaction number, and when it began, for the
    /// messages' `secs`.
    xid: u32,
    began: Instant,
    /// When to act next, and the messages sent in this phase.
    next: Instant,
    tries: u32,
    /// The offer being requested.
    offer: Option<Lease>,
    /// The lease, when it was given, and whether its address is on the
    /// card.
    lease: Option<(Lease, Instant)>,
    applied: bool,
    /// Refusals since the last lease, which make it wait longer before
    /// it starts again.
    refused: u32,
    random: u64,
}

impl Client {
    /// A client for the card with the address `mac`, which starts at
    /// once; `seed` makes its numbers.
    pub fn new(mac: [u8; 6], seed: u64, now: Instant) -> Client {
        Client {
            mac,
            phase: Phase::Init,
            xid: 0,
            began: now,
            next: now,
            tries: 0,
            offer: None,
            lease: None,
            applied: false,
            refused: 0,
            // xorshift64* needs a state other than 0.
            random: seed | 1,
        }
    }

    /// A client whose card had `lease`, given at `given`: it asks for the
    /// address again, or, if `applied`, the card has it still and the
    /// client goes on with the lease. One that has ended is forgotten.
    pub fn with_lease(
        mac: [u8; 6],
        seed: u64,
        now: Instant,
        lease: Lease,
        given: Instant,
        applied: bool,
    ) -> Client {
        let mut client = Client::new(mac, seed, now);
        if lease.times(given).is_some_and(|[.., end]| end <= now) {
            return client;
        }
        client.lease = Some((lease, given));
        client.applied = applied;
        if applied {
            client.phase = Phase::Bound;
            client.next = client.renew_at().unwrap_or(now);
        } else {
            client.phase = Phase::Rebooting;
        }
        client
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// When [`Client::tick`] next has something to do.
    pub fn next(&self) -> Instant {
        self.next
    }

    /// The lease, if the card has one.
    pub fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref().filter(|_| self.applied).map(|(lease, _)| lease)
    }

    /// Its card's link came back: it may be on another network now, so
    /// the client asks again for the address it has, or for one.
    pub fn link_up(&mut self, now: Instant) {
        self.offer = None;
        self.tries = 0;
        self.next = now;
        self.phase = if self.lease.is_some() { Phase::Rebooting } else { Phase::Init };
    }

    /// What to do at `now`: nothing before [`Client::next`].
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        if now < self.next {
            return Vec::new();
        }
        match self.phase {
            Phase::Init => {
                self.start(now);
                self.phase = Phase::Selecting;
                self.discover(now)
            }
            Phase::Selecting => self.discover(now),
            Phase::Requesting if self.tries >= REQUEST_TRIES => self.again(now, Vec::new()),
            Phase::Requesting => self.request(now),
            Phase::Rebooting if self.tries >= REBOOT_TRIES => {
                if self.applied {
                    // No server answered; the lease holds until it ends.
                    self.phase = Phase::Bound;
                    self.next = now;
                    return self.tick(now);
                }
                self.lease = None;
                self.again(now, Vec::new())
            }
            Phase::Rebooting => {
                if self.tries == 0 {
                    self.start(now);
                }
                self.request(now)
            }
            Phase::Bound | Phase::Renewing | Phase::Rebinding => self.extend(now),
        }
    }

    /// What to do with `bytes`, a message to the client's port.
    pub fn receive(&mut self, bytes: &[u8], now: Instant) -> Vec<Action> {
        let Ok(packet) = DhcpPacket::new_checked(bytes) else { return Vec::new() };
        let Ok(repr) = DhcpRepr::parse(&packet) else { return Vec::new() };
        if repr.transaction_id != self.xid || repr.client_hardware_address.0 != self.mac {
            return Vec::new();
        }
        let waiting = matches!(
            self.phase,
            Phase::Requesting | Phase::Rebooting | Phase::Renewing | Phase::Rebinding
        );
        match repr.message_type {
            DhcpMessageType::Offer if self.phase == Phase::Selecting => {
                let Some(offer) = lease_of(&repr, &packet, None) else { return Vec::new() };
                self.offer = Some(offer);
                self.phase = Phase::Requesting;
                self.tries = 0;
                self.request(now)
            }
            DhcpMessageType::Ack if waiting => {
                // A server may leave its identifier out of an
                // acknowledgement; it is the one asked.
                let asked = self.offer.as_ref().or(self.lease.as_ref().map(|(lease, _)| lease));
                let Some(lease) = lease_of(&repr, &packet, asked.map(|lease| lease.server)) else {
                    return Vec::new();
                };
                self.offer = None;
                self.refused = 0;
                self.applied = true;
                self.lease = Some((lease.clone(), now));
                self.phase = Phase::Bound;
                self.next = self.renew_at().unwrap_or(now);
                if self.lease.as_ref().is_some_and(|(lease, _)| lease.seconds == FOREVER) {
                    // Nothing more to do.
                    self.next = now + Duration::from_secs(u64::from(u32::MAX));
                }
                vec![Action::Bind(lease)]
            }
            DhcpMessageType::Nak if waiting => {
                // While requesting an offer, only its server's refusal.
                let server = self.offer.as_ref().map(|offer| offer.server);
                if server.is_some() && repr.server_identifier != server {
                    return Vec::new();
                }
                self.refused += 1;
                self.lease = None;
                let actions = if core::mem::take(&mut self.applied) {
                    vec![Action::Unbind]
                } else {
                    Vec::new()
                };
                self.again(now, actions)
            }
            _ => Vec::new(),
        }
    }

    /// Starts an exchange with a new transaction number.
    fn start(&mut self, now: Instant) {
        self.xid = self.random() as u32;
        self.began = now;
        self.tries = 0;
    }

    /// Starts again from INIT, after `actions`: at once, or after a while
    /// once servers have refused it.
    fn again(&mut self, now: Instant, mut actions: Vec<Action>) -> Vec<Action> {
        self.phase = Phase::Init;
        self.offer = None;
        self.tries = 0;
        self.next = now + Duration::from_secs(1 << self.refused.min(6)) - Duration::from_secs(1);
        actions.extend(self.tick(now));
        actions
    }

    fn discover(&mut self, now: Instant) -> Vec<Action> {
        let message = self.message(DhcpMessageType::Discover, now, None, None, None);
        self.back_off(now);
        Client::send(Ipv4Addr::BROADCAST, message)
    }

    /// Requests the offer, or the address of the lease the card had.
    fn request(&mut self, now: Instant) -> Vec<Action> {
        let (requested, server) = match (&self.offer, &self.lease) {
            (Some(offer), _) => (offer.address.ip, Some(offer.server)),
            (None, Some((lease, _))) => (lease.address.ip, None),
            (None, None) => return self.again(now, Vec::new()),
        };
        let message = self.message(DhcpMessageType::Request, now, None, Some(requested), server);
        self.back_off(now);
        Client::send(Ipv4Addr::BROADCAST, message)
    }

    /// Asks for more time for the lease, from its server until T2 and
    /// from any after, or gives it up once it has ended.
    fn extend(&mut self, now: Instant) -> Vec<Action> {
        let Some((lease, given)) = self.lease.clone() else { return self.again(now, Vec::new()) };
        let Some([renew, rebind, end]) = lease.times(given) else {
            self.next = now + Duration::from_secs(u64::from(u32::MAX));
            return Vec::new();
        };
        if now >= end {
            self.lease = None;
            self.applied = false;
            return self.again(now, vec![Action::Unbind]);
        }
        let (phase, to, until) = if now >= rebind {
            (Phase::Rebinding, Ipv4Addr::BROADCAST, end)
        } else if now >= renew {
            (Phase::Renewing, lease.server, rebind)
        } else {
            self.phase = Phase::Bound;
            self.next = renew;
            return Vec::new();
        };
        if phase != self.phase {
            self.start(now);
            self.phase = phase;
        }
        let message =
            self.message(DhcpMessageType::Request, now, Some(lease.address.ip), None, None);
        self.tries += 1;
        self.next = (now + ((until - now) / 2).max(RETRY_MIN)).min(until);
        Client::send(to, message)
    }

    fn send(to: Ipv4Addr, message: Option<Vec<u8>>) -> Vec<Action> {
        message.map(|message| Action::Send { to, message }).into_iter().collect()
    }

    /// Sets when to send again: 4 seconds after the first, doubling up to
    /// 64, a second either way at random.
    fn back_off(&mut self, now: Instant) {
        let wait = Duration::from_secs(4 << self.tries.min(4));
        let jitter = Duration::from_millis(self.random() % 2001);
        self.next = now + wait + jitter - Duration::from_secs(1);
        self.tries += 1;
    }

    /// A message of `kind`, from `from` (none before the card has an
    /// address), asking for `requested` from `server`.
    fn message(
        &self,
        kind: DhcpMessageType,
        now: Instant,
        from: Option<Ipv4Addr>,
        requested: Option<Ipv4Addr>,
        server: Option<Ipv4Addr>,
    ) -> Option<Vec<u8>> {
        let secs = u16::try_from(now.saturating_duration_since(self.began).as_secs());
        let repr = DhcpRepr {
            message_type: kind,
            transaction_id: self.xid,
            secs: secs.unwrap_or(u16::MAX),
            client_hardware_address: EthernetAddress(self.mac),
            client_ip: from.unwrap_or(Ipv4Addr::UNSPECIFIED),
            your_ip: Ipv4Addr::UNSPECIFIED,
            server_ip: Ipv4Addr::UNSPECIFIED,
            router: None,
            subnet_mask: None,
            relay_agent_ip: Ipv4Addr::UNSPECIFIED,
            // A card with no address cannot take a reply to one.
            broadcast: from.is_none(),
            requested_ip: requested,
            client_identifier: Some(EthernetAddress(self.mac)),
            server_identifier: server,
            parameter_request_list: Some(&REQUESTED[..]),
            dns_servers: None,
            max_size: Some(MESSAGE_MAX),
            lease_duration: None,
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        let mut bytes = vec![0; repr.buffer_len().max(MESSAGE_MIN)];
        repr.emit(&mut DhcpPacket::new_unchecked(&mut bytes[..])).ok()?;
        Some(bytes)
    }

    /// When to renew the lease.
    fn renew_at(&self) -> Option<Instant> {
        let (lease, given) = self.lease.as_ref()?;
        lease.times(*given).map(|[renew, ..]| renew)
    }

    /// The next of its numbers (xorshift64*).
    fn random(&mut self) -> u64 {
        self.random ^= self.random >> 12;
        self.random ^= self.random << 25;
        self.random ^= self.random >> 27;
        self.random.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// Whether a host may have `ip`: not in 0.0.0.0/8, loopback, multicast,
/// or the reserved 240.0.0.0/4, the broadcast address among them.
pub fn unicast(ip: Ipv4Addr) -> bool {
    let first = ip.octets()[0];
    !(first == 0 || ip.is_loopback() || ip.is_multicast() || first >= 240)
}

/// The prefix of the subnet mask `mask`, if its ones come first; else, or
/// with none, that of `ip`'s class, as before masks were given.
fn prefix(mask: Option<Ipv4Addr>, ip: Ipv4Addr) -> u8 {
    let mask = mask.map(u32::from);
    if let Some(mask) = mask
        && mask.leading_ones() + mask.trailing_zeros() == 32
        && mask != 0
    {
        return mask.leading_ones() as u8;
    }
    match ip.octets()[0] {
        0..=127 => 8,
        128..=191 => 16,
        _ => 24,
    }
}

/// A domain name a server gave, if it is one: letters, digits, `-`, `_`
/// and dots between labels, at most 253 of them.
fn domain(data: &[u8]) -> Option<String> {
    // Some servers end it with a NUL, which is no part of it.
    let end = data.iter().rposition(|&byte| byte != 0)? + 1;
    let name = std::str::from_utf8(&data[..end]).ok()?;
    let name = name.strip_suffix('.').unwrap_or(name);
    let label_ok = |label: &str| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    };
    (name.len() <= 253 && name.split('.').all(label_ok)).then(|| name.to_string())
}

/// The lease an offer or acknowledgement gives, if it is one a card may
/// take: from `server`, if the message does not say which server it is.
fn lease_of(
    repr: &DhcpRepr,
    packet: &DhcpPacket<&[u8]>,
    server: Option<Ipv4Addr>,
) -> Option<Lease> {
    let ip = repr.your_ip;
    if !unicast(ip) {
        return None;
    }
    let server = repr.server_identifier.or(server).filter(|&server| unicast(server))?;
    let address = Cidr { ip, prefix: prefix(repr.subnet_mask, ip) };
    let broadcast = address.broadcast();
    let mut router = None;
    let mut name = None;
    for option in packet.options() {
        match option.kind {
            // The first of the routers, which smoltcp takes only alone.
            OPTION_ROUTER if router.is_none() && option.data.len() >= 4 => {
                let octets: [u8; 4] = option.data[..4].try_into().ok()?;
                router = Some(Ipv4Addr::from(octets));
            }
            OPTION_DOMAIN => name = domain(option.data),
            _ => {}
        }
    }
    let router = router.filter(|&router| {
        unicast(router) && router != ip && address.contains(router) && Some(router) != broadcast
    });
    let dns = repr
        .dns_servers
        .iter()
        .flatten()
        .copied()
        .filter(|&dns| unicast(dns) && Some(dns) != broadcast)
        .collect();
    let seconds = repr.lease_duration.unwrap_or(LEASE_DEFAULT).max(LEASE_MIN);
    let (renew, rebind) = if seconds == FOREVER {
        (FOREVER, FOREVER)
    } else {
        let renew = repr.renew_duration.filter(|&renew| renew > 0 && renew < seconds);
        let renew = renew.unwrap_or(seconds / 2);
        let rebind = repr.rebind_duration.filter(|&rebind| rebind > renew && rebind < seconds);
        (renew, rebind.unwrap_or_else(|| (u64::from(seconds) * 7 / 8) as u32).max(renew))
    };
    Some(Lease { address, router, dns, domain: name, server, seconds, renew, rebind })
}

#[cfg(test)]
mod tests {
    use smoltcp::wire::DhcpOption;

    use super::*;

    const MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x56];
    const SERVER: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
    const OFFERED: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// The one message in `actions`, read, and where it goes.
    fn sent(actions: &[Action]) -> (Ipv4Addr, Vec<u8>) {
        let sends: Vec<_> = actions
            .iter()
            .filter_map(|action| match action {
                Action::Send { to, message } => Some((*to, message.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(sends.len(), 1, "{actions:?}");
        sends.into_iter().next().unwrap()
    }

    fn read(message: &[u8]) -> DhcpRepr<'_> {
        DhcpRepr::parse(Box::leak(Box::new(DhcpPacket::new_checked(message).unwrap()))).unwrap()
    }

    /// What a server answers `to` with: an offer or acknowledgement of
    /// 10.0.2.15/24 for an hour, through `edit`.
    fn reply(kind: DhcpMessageType, to: &DhcpRepr, edit: impl FnOnce(&mut DhcpRepr)) -> Vec<u8> {
        reply_with(kind, to, &[], edit)
    }

    fn reply_with(
        kind: DhcpMessageType,
        to: &DhcpRepr,
        options: &[DhcpOption],
        edit: impl FnOnce(&mut DhcpRepr),
    ) -> Vec<u8> {
        let mut repr = DhcpRepr {
            message_type: kind,
            transaction_id: to.transaction_id,
            secs: 0,
            client_hardware_address: to.client_hardware_address,
            client_ip: Ipv4Addr::UNSPECIFIED,
            your_ip: OFFERED,
            server_ip: SERVER,
            router: Some(SERVER),
            subnet_mask: Some(Ipv4Addr::new(255, 255, 255, 0)),
            relay_agent_ip: Ipv4Addr::UNSPECIFIED,
            broadcast: to.broadcast,
            requested_ip: None,
            client_identifier: None,
            server_identifier: Some(SERVER),
            parameter_request_list: None,
            dns_servers: Some([Ipv4Addr::new(10, 0, 2, 3)].into_iter().collect()),
            max_size: None,
            lease_duration: Some(3600),
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        edit(&mut repr);
        repr.additional_options = options;
        let mut bytes = vec![0; repr.buffer_len()];
        repr.emit(&mut DhcpPacket::new_unchecked(&mut bytes[..])).unwrap();
        bytes
    }

    /// A client bound to the offer at `now`, and the request it sent.
    fn bound(now: Instant) -> (Client, Vec<u8>) {
        let mut client = Client::new(MAC, 7, now);
        let (_, discover) = sent(&client.tick(now));
        let offer = reply(DhcpMessageType::Offer, &read(&discover), |_| {});
        let (_, request) = sent(&client.receive(&offer, now));
        let ack = reply(DhcpMessageType::Ack, &read(&request), |_| {});
        let actions = client.receive(&ack, now);
        assert!(matches!(&actions[..], [Action::Bind(_)]), "{actions:?}");
        (client, request)
    }

    #[test]
    fn a_card_discovers_requests_the_offer_and_is_bound() {
        let now = Instant::now();
        let mut client = Client::new(MAC, 7, now);
        assert_eq!(client.phase(), Phase::Init);
        let (to, discover) = sent(&client.tick(now));
        assert_eq!(to, Ipv4Addr::BROADCAST);
        assert!(discover.len() >= MESSAGE_MIN);
        let asked = read(&discover);
        assert_eq!(asked.message_type, DhcpMessageType::Discover);
        assert_eq!(asked.client_hardware_address.0, MAC);
        assert_eq!(asked.client_identifier, Some(EthernetAddress(MAC)));
        assert!(asked.broadcast);
        assert_eq!(asked.client_ip, Ipv4Addr::UNSPECIFIED);
        assert_eq!(asked.parameter_request_list, Some(&REQUESTED[..]));
        assert_eq!(asked.max_size, Some(MESSAGE_MAX));
        assert_eq!(client.phase(), Phase::Selecting);

        let offer = reply(DhcpMessageType::Offer, &asked, |_| {});
        let (to, request) = sent(&client.receive(&offer, now + secs(1)));
        assert_eq!(to, Ipv4Addr::BROADCAST);
        let request = read(&request);
        assert_eq!(request.message_type, DhcpMessageType::Request);
        assert_eq!(request.transaction_id, asked.transaction_id);
        assert_eq!(request.requested_ip, Some(OFFERED));
        assert_eq!(request.server_identifier, Some(SERVER));
        assert_eq!(request.secs, 1);
        assert_eq!(client.phase(), Phase::Requesting);
        assert_eq!(client.lease(), None);

        let ack = reply(DhcpMessageType::Ack, &request, |_| {});
        let lease = Lease {
            address: Cidr { ip: OFFERED, prefix: 24 },
            router: Some(SERVER),
            dns: vec![Ipv4Addr::new(10, 0, 2, 3)],
            domain: None,
            server: SERVER,
            seconds: 3600,
            renew: 1800,
            rebind: 3150,
        };
        let given = now + secs(2);
        assert_eq!(client.receive(&ack, given), [Action::Bind(lease.clone())]);
        assert_eq!(client.phase(), Phase::Bound);
        assert_eq!(client.lease(), Some(&lease));
        assert_eq!(client.next(), given + secs(1800));
        // Another acknowledgement changes nothing.
        assert_eq!(client.receive(&ack, given), []);
        assert_eq!(client.tick(given + secs(1799)), []);
    }

    #[test]
    fn messages_are_sent_again_after_4_8_16_32_and_64_seconds() {
        let start = Instant::now();
        let mut client = Client::new(MAC, 99, start);
        let (_, first) = sent(&client.tick(start));
        let xid = read(&first).transaction_id;
        let mut now = start;
        for wait in [4, 8, 16, 32, 64, 64, 64] {
            let next = client.next() - now;
            assert!(next >= secs(wait - 1) && next <= secs(wait + 1), "{next:?} for {wait}");
            assert_eq!(client.tick(client.next() - Duration::from_millis(1)), []);
            now = client.next();
            let (to, again) = sent(&client.tick(now));
            assert_eq!(to, Ipv4Addr::BROADCAST);
            let again = read(&again);
            assert_eq!(again.message_type, DhcpMessageType::Discover);
            assert_eq!(again.transaction_id, xid);
            assert_eq!(u64::from(again.secs), (now - start).as_secs());
        }
    }

    #[test]
    fn replies_to_others_and_messages_out_of_turn_are_ignored() {
        let now = Instant::now();
        let mut client = Client::new(MAC, 7, now);
        let (_, discover) = sent(&client.tick(now));
        let asked = read(&discover);
        let other_xid = reply(DhcpMessageType::Offer, &asked, |r| r.transaction_id ^= 1);
        let other_mac = reply(DhcpMessageType::Offer, &asked, |r| {
            r.client_hardware_address = EthernetAddress([2, 0, 0, 0, 0, 1]);
        });
        let early_ack = reply(DhcpMessageType::Ack, &asked, |_| {});
        let early_nak = reply(DhcpMessageType::Nak, &asked, |_| {});
        for message in [other_xid, other_mac, early_ack, early_nak] {
            assert_eq!(client.receive(&message, now), []);
            assert_eq!(client.phase(), Phase::Selecting);
        }
        // Offers of addresses no host may have, or from no server.
        for edit in [
            (|r: &mut DhcpRepr| r.your_ip = Ipv4Addr::UNSPECIFIED) as fn(&mut DhcpRepr),
            |r| r.your_ip = Ipv4Addr::BROADCAST,
            |r| r.your_ip = Ipv4Addr::new(127, 0, 0, 1),
            |r| r.your_ip = Ipv4Addr::new(224, 0, 0, 1),
            |r| r.server_identifier = None,
        ] {
            let offer = reply(DhcpMessageType::Offer, &asked, edit);
            assert_eq!(client.receive(&offer, now), []);
            assert_eq!(client.phase(), Phase::Selecting);
        }
        // A client's own message, as a server's port might hear it.
        assert_eq!(client.receive(&discover, now), []);
    }

    #[test]
    fn a_lease_is_renewed_from_its_server_then_from_any_and_then_given_up() {
        let given = Instant::now();
        let (mut client, _) = bound(given);
        // At T1, from its server, from the address it has.
        let (to, renew) = sent(&client.tick(given + secs(1800)));
        assert_eq!(to, SERVER);
        let renew = read(&renew);
        assert_eq!(renew.message_type, DhcpMessageType::Request);
        assert_eq!(renew.client_ip, OFFERED);
        assert!(!renew.broadcast);
        assert_eq!((renew.requested_ip, renew.server_identifier), (None, None));
        assert_eq!(client.phase(), Phase::Renewing);
        // Again after half the time to T2, at least a minute.
        assert_eq!(client.next(), given + secs(1800 + 675));
        let (_, again) = sent(&client.tick(client.next()));
        assert_eq!(read(&again).transaction_id, renew.transaction_id);
        let mut last = client.next();
        while client.next() < given + secs(3150) {
            last = client.next();
            let (to, _) = sent(&client.tick(last));
            assert_eq!(to, SERVER);
        }
        assert!(given + secs(3150) - last <= RETRY_MIN);
        // At T2, from any server.
        let (to, rebind) = sent(&client.tick(given + secs(3150)));
        assert_eq!(to, Ipv4Addr::BROADCAST);
        let rebind = read(&rebind);
        assert_eq!(rebind.client_ip, OFFERED);
        assert_ne!(rebind.transaction_id, renew.transaction_id);
        assert_eq!(client.phase(), Phase::Rebinding);
        assert_eq!(client.next(), given + secs(3150 + 225));
        while client.next() < given + secs(3600) {
            sent(&client.tick(client.next()));
        }
        // At its end it goes, and the client starts again.
        let actions = client.tick(given + secs(3600));
        assert_eq!(actions[0], Action::Unbind);
        let (_, discover) = sent(&actions);
        assert_eq!(read(&discover).message_type, DhcpMessageType::Discover);
        assert_eq!(client.lease(), None);
        assert_eq!(client.phase(), Phase::Selecting);
    }

    #[test]
    fn an_acknowledged_renewal_extends_the_lease() {
        let given = Instant::now();
        let (mut client, _) = bound(given);
        let renewed = given + secs(1800);
        let (_, renew) = sent(&client.tick(renewed));
        // smoltcp writes no T1 or T2 of its own.
        let (t1, t2) = (600_u32.to_be_bytes(), 6000_u32.to_be_bytes());
        let times = [DhcpOption { kind: 58, data: &t1 }, DhcpOption { kind: 59, data: &t2 }];
        let ack = reply_with(DhcpMessageType::Ack, &read(&renew), &times, |r| {
            r.lease_duration = Some(7200);
        });
        let actions = client.receive(&ack, renewed);
        let [Action::Bind(lease)] = &actions[..] else { panic!("{actions:?}") };
        assert_eq!((lease.seconds, lease.renew, lease.rebind), (7200, 600, 6000));
        assert_eq!(client.phase(), Phase::Bound);
        assert_eq!(client.next(), renewed + secs(600));
    }

    #[test]
    fn a_refusal_takes_the_address_away_and_the_client_waits_longer_each_time() {
        let given = Instant::now();
        let (mut client, _) = bound(given);
        let now = given + secs(1800);
        let (_, renew) = sent(&client.tick(now));
        let nak = reply(DhcpMessageType::Nak, &read(&renew), |_| {});
        let actions = client.receive(&nak, now);
        // Unbound, and starting again a second later.
        assert_eq!(actions, [Action::Unbind]);
        assert_eq!(client.phase(), Phase::Init);
        assert_eq!(client.next(), now + secs(1));
        assert_eq!(client.lease(), None);
        // Refused again while requesting an offer: two seconds, then four.
        let mut now = client.next();
        for wait in [3, 7] {
            let (_, discover) = sent(&client.tick(now));
            let offer = reply(DhcpMessageType::Offer, &read(&discover), |_| {});
            let (_, request) = sent(&client.receive(&offer, now));
            // Not from the offer's server: not its refusal.
            let other = reply(DhcpMessageType::Nak, &read(&request), |r| {
                r.server_identifier = Some(Ipv4Addr::new(10, 0, 2, 9));
            });
            assert_eq!(client.receive(&other, now), []);
            let nak = reply(DhcpMessageType::Nak, &read(&request), |_| {});
            assert_eq!(client.receive(&nak, now), []);
            assert_eq!(client.next(), now + secs(wait));
            now = client.next();
        }
    }

    #[test]
    fn an_offer_unanswered_four_times_is_given_up() {
        let now = Instant::now();
        let mut client = Client::new(MAC, 7, now);
        let (_, discover) = sent(&client.tick(now));
        let offer = reply(DhcpMessageType::Offer, &read(&discover), |_| {});
        sent(&client.receive(&offer, now));
        for _ in 1..REQUEST_TRIES {
            let (_, request) = sent(&client.tick(client.next()));
            assert_eq!(read(&request).message_type, DhcpMessageType::Request);
        }
        let (_, discover) = sent(&client.tick(client.next()));
        assert_eq!(read(&discover).message_type, DhcpMessageType::Discover);
        assert_eq!(client.phase(), Phase::Selecting);
    }

    fn known() -> Lease {
        Lease {
            address: Cidr { ip: OFFERED, prefix: 24 },
            router: Some(SERVER),
            dns: Vec::new(),
            domain: None,
            server: SERVER,
            seconds: 3600,
            renew: 1800,
            rebind: 3150,
        }
    }

    #[test]
    fn a_card_asks_again_for_the_address_it_had() {
        let now = Instant::now();
        let mut client = Client::with_lease(MAC, 7, now, known(), now - secs(600), false);
        assert_eq!(client.phase(), Phase::Rebooting);
        assert_eq!(client.lease(), None);
        let (to, request) = sent(&client.tick(now));
        assert_eq!(to, Ipv4Addr::BROADCAST);
        let request = read(&request);
        assert_eq!(request.message_type, DhcpMessageType::Request);
        assert_eq!(request.requested_ip, Some(OFFERED));
        assert_eq!(request.server_identifier, None);
        assert_eq!(request.client_ip, Ipv4Addr::UNSPECIFIED);
        let ack = reply(DhcpMessageType::Ack, &request, |r| r.server_identifier = None);
        let actions = client.receive(&ack, now);
        let [Action::Bind(lease)] = &actions[..] else { panic!("{actions:?}") };
        assert_eq!(lease.server, SERVER);

        // Unanswered twice, it asks for any address.
        let mut client = Client::with_lease(MAC, 7, now, known(), now - secs(600), false);
        sent(&client.tick(now));
        sent(&client.tick(client.next()));
        let (_, discover) = sent(&client.tick(client.next()));
        assert_eq!(read(&discover).message_type, DhcpMessageType::Discover);

        // A lease that has ended is forgotten.
        let client = Client::with_lease(MAC, 7, now, known(), now - secs(3600), false);
        assert_eq!(client.phase(), Phase::Init);
    }

    #[test]
    fn a_card_that_still_has_its_address_keeps_it_when_no_server_answers() {
        let now = Instant::now();
        // netd started again, with the card set up by the lease.
        let mut client = Client::with_lease(MAC, 7, now, known(), now - secs(600), true);
        assert_eq!(client.phase(), Phase::Bound);
        assert_eq!(client.lease(), Some(&known()));
        assert_eq!(client.next(), now + secs(1200));
        // Its link went down and came back.
        client.link_up(now);
        assert_eq!(client.phase(), Phase::Rebooting);
        sent(&client.tick(now));
        sent(&client.tick(client.next()));
        assert_eq!(client.tick(client.next()), []);
        assert_eq!(client.phase(), Phase::Bound);
        assert_eq!(client.lease(), Some(&known()));
        // And loses it when a server refuses it.
        client.link_up(now);
        let (_, request) = sent(&client.tick(now));
        let nak = reply(DhcpMessageType::Nak, &read(&request), |_| {});
        assert_eq!(client.receive(&nak, now), [Action::Unbind]);
        assert_eq!(client.lease(), None);
    }

    #[test]
    fn what_a_server_says_is_checked() {
        let now = Instant::now();
        let lease = |options: &[DhcpOption], edit: fn(&mut DhcpRepr)| {
            let mut client = Client::new(MAC, 7, now);
            let (_, discover) = sent(&client.tick(now));
            let offer = reply_with(DhcpMessageType::Offer, &read(&discover), options, |r| {
                r.router = None;
                edit(r);
            });
            sent(&client.receive(&offer, now));
            client.offer.clone().unwrap()
        };
        let plain = lease(&[], |_| {});
        assert_eq!(plain.router, None);
        assert_eq!((plain.seconds, plain.renew, plain.rebind), (3600, 1800, 3150));
        // The first of two routers; one off the network is no router.
        let routers = [10, 0, 2, 2, 10, 0, 2, 3];
        let two = lease(&[DhcpOption { kind: OPTION_ROUTER, data: &routers }], |_| {});
        assert_eq!(two.router, Some(SERVER));
        let far = [192, 168, 1, 1];
        assert_eq!(lease(&[DhcpOption { kind: OPTION_ROUTER, data: &far }], |_| {}).router, None);
        let own = OFFERED.octets();
        assert_eq!(lease(&[DhcpOption { kind: OPTION_ROUTER, data: &own }], |_| {}).router, None);
        // A domain name, and what is not one.
        let name = |data: &[u8]| lease(&[DhcpOption { kind: OPTION_DOMAIN, data }], |_| {}).domain;
        assert_eq!(name(b"example.org"), Some("example.org".into()));
        assert_eq!(name(b"example.org.\0"), Some("example.org".into()));
        for bad in [&b"evil\nnameserver 6.6.6.6"[..], b"a b", b"..", b"", b"x..y", &[b'a'; 254]] {
            assert_eq!(name(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
        // A mask whose ones are not first: the address's class's.
        let odd = lease(&[], |r| r.subnet_mask = Some(Ipv4Addr::new(255, 0, 255, 0)));
        assert_eq!(odd.address.prefix, 8);
        let none = lease(&[], |r| r.subnet_mask = None);
        assert_eq!(none.address.prefix, 8);
        // DNS servers no host may be.
        let dns = lease(&[], |r| {
            r.dns_servers = Some(
                [Ipv4Addr::UNSPECIFIED, Ipv4Addr::new(10, 0, 2, 255), Ipv4Addr::new(1, 1, 1, 1)]
                    .into_iter()
                    .collect(),
            );
        });
        assert_eq!(dns.dns, [Ipv4Addr::new(1, 1, 1, 1)]);
        // Lease times out of order are the defaults; a short lease is a
        // minute; one for ever is never renewed.
        let (t1, t2) = (4000_u32.to_be_bytes(), 100_u32.to_be_bytes());
        let times = [DhcpOption { kind: 58, data: &t1 }, DhcpOption { kind: 59, data: &t2 }];
        let times = lease(&times, |_| {});
        assert_eq!((times.renew, times.rebind), (1800, 3150));
        let (t1, t2) = (1000_u32.to_be_bytes(), 2000_u32.to_be_bytes());
        let times = [DhcpOption { kind: 58, data: &t1 }, DhcpOption { kind: 59, data: &t2 }];
        let times = lease(&times, |_| {});
        assert_eq!((times.renew, times.rebind), (1000, 2000));
        let short = lease(&[], |r| r.lease_duration = Some(5));
        assert_eq!((short.seconds, short.renew, short.rebind), (60, 30, 52));
        let ever = lease(&[], |r| r.lease_duration = Some(u32::MAX));
        assert_eq!((ever.renew, ever.rebind), (FOREVER, FOREVER));
    }

    #[test]
    fn a_lease_for_ever_is_never_renewed() {
        let now = Instant::now();
        let mut client = Client::new(MAC, 7, now);
        let (_, discover) = sent(&client.tick(now));
        let ever = |r: &mut DhcpRepr| r.lease_duration = Some(u32::MAX);
        let offer = reply(DhcpMessageType::Offer, &read(&discover), ever);
        let (_, request) = sent(&client.receive(&offer, now));
        let ack = reply(DhcpMessageType::Ack, &read(&request), ever);
        assert!(matches!(&client.receive(&ack, now)[..], [Action::Bind(_)]));
        assert_eq!(client.tick(now + secs(10 * 365 * 86_400)), []);
        assert_eq!(client.phase(), Phase::Bound);
    }

    #[test]
    fn garbage_is_ignored() {
        let now = Instant::now();
        let mut client = Client::new(MAC, 7, now);
        let (_, discover) = sent(&client.tick(now));
        let offer = reply(DhcpMessageType::Offer, &read(&discover), |_| {});
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..20_000 {
            let mut bytes = offer.clone();
            if round % 3 == 0 {
                bytes.truncate(next() as usize % (offer.len() + 1));
            }
            for _ in 0..=(next() % 8) {
                let at = next() as usize % bytes.len().max(1);
                if let Some(byte) = bytes.get_mut(at) {
                    *byte = next() as u8;
                }
            }
            let mut copy = Client::new(MAC, 7, now);
            copy.tick(now);
            copy.xid = client.xid;
            // Whatever it takes, it neither panics nor takes a lease no
            // host may have.
            copy.receive(&bytes, now);
            if let Some(offer) = &copy.offer {
                assert!(unicast(offer.address.ip) && unicast(offer.server));
                assert!(offer.domain.as_deref().is_none_or(|d| !d.contains(char::is_whitespace)));
            }
        }
        assert_eq!(client.phase(), Phase::Selecting);
    }
}
