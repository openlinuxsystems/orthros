// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! netd(8): sets the network cards up (ADR 0191).
//!
//! Each card gets its address, router and DNS servers by DHCP (the
//! client is [`crate::dhcp`]), unless `/system/settings/network`
//! (network(5)) gives it addresses of its own or says to leave it alone.
//! netd sets a card up through radix-network(2)'s `configure`, as ip(8)
//! does, and changes only the addresses it gave: those an administrator
//! added stay. It keeps each lease in `/system/state/netd`, so that a
//! card asks for the same address after a reboot and netd, started
//! again, goes on with the lease the card has; and it writes the DNS
//! servers into `/system/live/netd/resolv.conf`, which
//! `/etc/resolv.conf` leads to unless an administrator made it a file of
//! their own.
//!
//! A DHCP card has a socket of its own on port 68, bound to it, which the
//! kernel lets the sockets of other cards share. netd looks at the cards
//! every few seconds, for cards that come and links that come back, and
//! otherwise sleeps until a client has something to send. SIGHUP reads
//! the settings again; SIGTERM ends netd and leaves the cards as they
//! are, so that connections go on.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::args::Args;
use crate::dhcp::{self, Action, Client, Lease};
use crate::ip::{self, Cidr, Interface, Network};
use crate::util::{self, Fail};

/// The settings file, the leases' directory and the resolver's file.
pub const SETTINGS: &str = "/system/settings/network";
const LEASES: &str = "/system/state/netd";
const RESOLV: &str = "/system/live/netd/resolv.conf";

/// How often netd looks at the cards.
const SCAN: Duration = Duration::from_secs(5);

/// The most addresses and DNS servers settings give a card, and the most
/// DNS servers resolv.conf takes.
const ADDRESSES_MAX: usize = 4;
const DNS_MAX: usize = 3;

/// The ports of DHCP.
const CLIENT_PORT: u16 = 68;
const SERVER_PORT: u16 = 67;

/// How the settings file has netd set a card up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Setup {
    /// Whether it asks DHCP: unless it says no, or gives addresses of
    /// its own and does not say yes.
    pub dhcp: Option<bool>,
    pub addresses: Vec<Cidr>,
    pub router: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
}

impl Setup {
    fn dhcp(&self) -> bool {
        self.dhcp.unwrap_or(self.addresses.is_empty())
    }

    /// Whether netd leaves the card alone.
    fn alone(&self) -> bool {
        !self.dhcp() && self.addresses.is_empty()
    }
}

/// The settings file: how each card it names is set up, by its name or
/// its MAC address, and what is wrong in it, a line each.
pub fn parse_settings(text: &str) -> (Vec<(String, Setup)>, Vec<String>) {
    let mut cards: Vec<(String, Setup)> = Vec::new();
    let mut wrong = Vec::new();
    for (at, line) in text.lines().enumerate() {
        let at = at + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() || cards.iter().any(|(card, _)| *card == name) {
                wrong.push(format!(
                    "{SETTINGS}:{at}: '[{name}]' names no card, or one named before"
                ));
                // What follows, up to the next card, goes nowhere.
                cards.push((String::new(), Setup::default()));
            } else {
                cards.push((name, Setup::default()));
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            wrong.push(format!("{SETTINGS}:{at}: not `setting = value` or `[card]`"));
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let Some((_, setup)) = cards.last_mut() else {
            wrong.push(format!("{SETTINGS}:{at}: {key} before any `[card]`"));
            continue;
        };
        let address = || value.parse::<Ipv4Addr>().ok().filter(|&ip| dhcp::unicast(ip));
        let ok = match key {
            "dhcp" => match value {
                "yes" | "no" if setup.dhcp.is_none() => {
                    setup.dhcp = Some(value == "yes");
                    true
                }
                _ => false,
            },
            "address" => match Cidr::parse(value) {
                Some(cidr) if dhcp::unicast(cidr.ip) && setup.addresses.len() < ADDRESSES_MAX => {
                    setup.addresses.push(cidr);
                    true
                }
                _ => false,
            },
            "router" => match address() {
                Some(router) if setup.router.is_none() => {
                    setup.router = Some(router);
                    true
                }
                _ => false,
            },
            "dns" => match address() {
                Some(dns) if setup.dns.len() < DNS_MAX => {
                    setup.dns.push(dns);
                    true
                }
                _ => false,
            },
            _ => {
                wrong.push(format!("{SETTINGS}:{at}: no setting is called {key}"));
                continue;
            }
        };
        if !ok {
            wrong.push(format!("{SETTINGS}:{at}: '{value}' is not a {key} here"));
        }
    }
    for (name, setup) in &cards {
        if let Some(router) = setup.router
            && !setup.addresses.is_empty()
            && !setup.addresses.iter().any(|cidr| cidr.contains(router))
        {
            wrong
                .push(format!("{SETTINGS}: [{name}]'s router {router} is on none of its networks"));
        }
    }
    cards.retain(|(name, _)| !name.is_empty());
    (cards, wrong)
}

/// A MAC address, `52:54:00:12:34:56`.
fn mac_of(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0; 6];
    let mut parts = text.split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// A lease as netd keeps it, given at `obtained`, in seconds since 1970.
pub fn lease_text(lease: &Lease, card: &str, obtained: u64) -> String {
    let mut text = format!("# netd(8)'s lease for {card}.\naddress = {}\n", lease.address);
    if let Some(router) = lease.router {
        text += &format!("router = {router}\n");
    }
    for dns in &lease.dns {
        text += &format!("dns = {dns}\n");
    }
    if let Some(domain) = &lease.domain {
        text += &format!("domain = {domain}\n");
    }
    text += &format!(
        "server = {}\nobtained = {obtained}\nseconds = {}\nrenew = {}\nrebind = {}\n",
        lease.server, lease.seconds, lease.renew, lease.rebind
    );
    text
}

/// The lease a lease file holds, and when it was given, if it is whole.
pub fn parse_lease(text: &str) -> Option<(Lease, u64)> {
    let mut fields: HashMap<&str, Vec<&str>> = HashMap::new();
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let (key, value) = line.split_once('=')?;
        fields.entry(key.trim()).or_default().push(value.trim());
    }
    let one = |key: &str| fields.get(key).and_then(|values| values.first().copied());
    let number = |key: &str| one(key)?.parse::<u32>().ok();
    let address = Cidr::parse(one("address")?)?;
    let dns = fields.get("dns").into_iter().flatten().filter_map(|dns| dns.parse().ok());
    let lease = Lease {
        address,
        router: one("router").and_then(|router| router.parse().ok()),
        dns: dns.collect(),
        domain: one("domain").map(str::to_string),
        server: one("server")?.parse().ok()?,
        seconds: number("seconds")?,
        renew: number("renew")?,
        rebind: number("rebind")?,
    };
    Some((lease, one("obtained")?.parse().ok()?))
}

/// The resolv.conf of `servers` and the domains `search`, at most
/// [`DNS_MAX`] servers.
pub fn resolv_text(servers: &[Ipv4Addr], search: &[String]) -> String {
    let mut text = format!(
        "# Written by netd(8) from what DHCP and {SETTINGS} give.\n\
         # /etc/resolv.conf leads here; make it a file of your own for other servers.\n"
    );
    for server in servers.iter().take(DNS_MAX) {
        text += &format!("nameserver {server}\n");
    }
    if !search.is_empty() {
        text += &format!("search {}\n", search.join(" "));
    }
    text
}

/// The addresses a card has after netd gives it `ours` in place of
/// `before`, those it gave: ours first, then the others it has, at most
/// four.
pub fn merge(has: &[Cidr], before: &[Cidr], ours: &[Cidr]) -> Vec<Cidr> {
    let mut addresses = ours.to_vec();
    for cidr in has {
        if !before.contains(cidr) && !addresses.iter().any(|ours| ours.ip == cidr.ip) {
            addresses.push(*cidr);
        }
    }
    addresses.truncate(ADDRESSES_MAX);
    addresses
}

/// How netd sets a card up.
#[derive(Debug)]
enum How {
    /// By DHCP, through its own socket.
    Dhcp { socket: UdpSocket, client: Box<Client> },
    /// By DHCP once port 68 of the card is free: another socket held it.
    Waiting,
    /// With the settings' addresses.
    Fixed,
    /// Not at all.
    Alone,
}

/// A card netd knows.
#[derive(Debug)]
struct Card {
    name: String,
    index: u32,
    mac: [u8; 6],
    carrier: bool,
    how: How,
    /// What netd gave it: addresses, router, DNS servers and domain.
    given: Vec<Cidr>,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    domain: Option<String>,
    /// The last failure to send, so that each is said once.
    failure: Option<String>,
}

/// Set by SIGTERM: netd ends. Set by SIGHUP: it reads the settings
/// again.
static STOP: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

extern "C" fn on_hangup(_: libc::c_int) {
    RELOAD.store(true, Ordering::Relaxed);
}

/// The seconds since 1970.
fn wall() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

/// A number from the kernel's random source, else from the clock.
fn seed() -> u64 {
    let mut bytes = [0u8; 8];
    // SAFETY: the buffer is valid for its length.
    let got = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if got == 8 { u64::from_ne_bytes(bytes) } else { util::now_ns() }
}

/// A UDP socket on port 68 of the card named `name` that may broadcast,
/// which does not block.
fn client_socket(name: &str) -> io::Result<UdpSocket> {
    let check = |result: libc::c_int| {
        if result < 0 { Err(io::Error::last_os_error()) } else { Ok(result) }
    };
    // SAFETY: socket takes numbers; the descriptor is owned below.
    let fd =
        check(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;
    // SAFETY: fd is a socket of netd's own, which the UdpSocket closes.
    let socket = unsafe { UdpSocket::from_raw_fd(fd) };
    let on: libc::c_int = 1;
    // SAFETY: the option's values are valid for the lengths given.
    unsafe {
        check(libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr().cast(),
            name.len() as libc::socklen_t,
        ))?;
        check(libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BROADCAST,
            (&raw const on).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        ))?;
    }
    // SAFETY: all zeros is a valid sockaddr_in: 0.0.0.0, port 0.
    let mut address: libc::sockaddr_in = unsafe { util::zeroed() };
    address.sin_family = libc::AF_INET as libc::sa_family_t;
    address.sin_port = CLIENT_PORT.to_be();
    // SAFETY: the address is valid for its length.
    check(unsafe {
        libc::bind(
            fd,
            (&raw const address).cast(),
            size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    })?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

/// netd's state.
struct Netd {
    cards: Vec<Card>,
    settings: Vec<(String, Setup)>,
    /// When it last looked at the cards.
    scanned: Option<Instant>,
    network: &'static dyn Network,
}

impl Netd {
    /// How the settings have the card named `name`, with the address
    /// `mac`, set up.
    fn setup(&self, name: &str, mac: &str) -> Setup {
        let mac = mac.to_ascii_lowercase();
        self.settings
            .iter()
            .find(|(card, _)| *card == name || *card == mac)
            .map_or_else(Setup::default, |(_, setup)| setup.clone())
    }

    fn read_settings(&mut self) {
        let text = fs::read_to_string(SETTINGS).unwrap_or_default();
        let (settings, wrong) = parse_settings(&text);
        for line in wrong {
            eprintln!("netd: {line}");
        }
        self.settings = settings;
    }

    /// Looks at the cards: sets up those that came, forgets those that
    /// went, and has those whose link came back ask again.
    fn scan(&mut self, now: Instant) {
        let first = self.scanned.is_none();
        self.scanned = Some(now);
        let Ok(interfaces) = ip::read_interfaces(Path::new(ip::INTERFACES), Path::new(ip::ROUTES))
        else {
            return;
        };
        let known = self.cards.len();
        self.cards.retain(|card| {
            let here = interfaces.iter().any(|i| i.index == card.index && i.name == card.name);
            if !here {
                eprintln!("netd: {} went", card.name);
            }
            here
        });
        let mut changed = self.cards.len() != known;
        for interface in interfaces.iter().filter(|interface| !interface.loopback) {
            if let Some(card) = self.cards.iter_mut().find(|card| card.index == interface.index) {
                let back = interface.carrier && !card.carrier;
                card.carrier = interface.carrier;
                if back && let How::Dhcp { client, .. } = &mut card.how {
                    eprintln!("netd: {}'s link is up again; asking for its address", card.name);
                    client.link_up(now);
                }
                if matches!(card.how, How::Waiting) {
                    Netd::ask(card, interface, now, false);
                }
                continue;
            }
            if let Some(card) = self.card(interface, now, first) {
                self.cards.push(card);
                changed = true;
            }
        }
        if changed {
            self.write_resolv();
        }
    }

    /// Sets a card that came up as the settings say: at netd's `first`
    /// look, one set up already is left as it is.
    fn card(&self, interface: &Interface, now: Instant, first: bool) -> Option<Card> {
        let mac = mac_of(&interface.mac)?;
        let setup = self.setup(&interface.name, &interface.mac);
        let mut card = Card {
            name: interface.name.clone(),
            index: interface.index,
            mac,
            carrier: interface.carrier,
            how: How::Alone,
            given: Vec::new(),
            router: None,
            dns: setup.dns.clone(),
            domain: None,
            failure: None,
        };
        if setup.alone() {
            eprintln!("netd: {} is left as it is, as {SETTINGS} says", card.name);
            return Some(card);
        }
        if !setup.addresses.is_empty() {
            self.give(&mut card, interface, &setup.addresses, setup.router);
            card.how = How::Fixed;
        }
        if !setup.dhcp() {
            return Some(card);
        }
        if first
            && setup.addresses.is_empty()
            && !interface.addresses.is_empty()
            && read_lease(card.mac).is_none()
        {
            // Set up already: by the kernel's ip= flag, or by hand.
            let addresses: Vec<String> = interface.addresses.iter().map(Cidr::to_string).collect();
            eprintln!(
                "netd: {} has {} already; it is left as it is",
                card.name,
                addresses.join(", ")
            );
            return Some(card);
        }
        Netd::ask(&mut card, interface, now, true);
        Some(card)
    }

    /// Gives `card` a DHCP client on a socket of its own on port 68, going
    /// on with the lease it keeps; while another socket holds the port,
    /// the card waits for it, which is said once, when `say`.
    fn ask(card: &mut Card, interface: &Interface, now: Instant, say: bool) {
        let socket = match client_socket(&card.name) {
            Ok(socket) => socket,
            Err(err) => {
                if say {
                    eprintln!(
                        "netd: {}: no socket on port {CLIENT_PORT}, for now: {err}",
                        card.name
                    );
                }
                card.how = How::Waiting;
                return;
            }
        };
        let client = match read_lease(card.mac) {
            Some((lease, obtained)) => {
                let ago = Duration::from_secs(wall().saturating_sub(obtained));
                let given = now.checked_sub(ago).unwrap_or(now);
                let applied = interface.addresses.contains(&lease.address);
                if applied {
                    card.given = vec![lease.address];
                    card.router = lease.router;
                    card.dns.extend(&lease.dns);
                    card.domain.clone_from(&lease.domain);
                }
                let client = Client::with_lease(card.mac, seed(), now, lease, given, applied);
                match (client.lease(), client.phase()) {
                    (Some(lease), _) => eprintln!(
                        "netd: {} goes on with its lease of {} from {}",
                        card.name, lease.address, lease.server
                    ),
                    (None, dhcp::Phase::Rebooting) => {
                        eprintln!("netd: {}: asking for the address it had", card.name);
                    }
                    (None, _) => forget_lease(card.mac),
                }
                client
            }
            None => {
                eprintln!("netd: {}: asking for an address by DHCP", card.name);
                Client::new(card.mac, seed(), now)
            }
        };
        card.how = How::Dhcp { socket, client: Box::new(client) };
    }

    /// Gives `card` the addresses `ours` and `router` in place of those
    /// netd gave it, keeping others' addresses: true if the kernel took
    /// them.
    fn give(
        &self,
        card: &mut Card,
        interface: &Interface,
        ours: &[Cidr],
        router: Option<Ipv4Addr>,
    ) -> bool {
        let addresses = merge(&interface.addresses, &card.given, ours);
        // Another's router stays while it is on one of the networks.
        let kept = interface.router.filter(|&router| {
            Some(router) != card.router && addresses.iter().any(|cidr| cidr.contains(router))
        });
        match self.network.configure(card.index, &addresses, router.or(kept)) {
            Ok(()) => {
                card.given = ours.to_vec();
                card.router = router;
                true
            }
            Err(status) => {
                eprintln!(
                    "netd: {}: the kernel refused its addresses: {}",
                    card.name,
                    crate::sys::status_name(status.0)
                );
                false
            }
        }
    }

    /// The card numbered `index`'s interface now.
    fn interface(index: u32) -> Option<Interface> {
        let interfaces =
            ip::read_interfaces(Path::new(ip::INTERFACES), Path::new(ip::ROUTES)).ok()?;
        interfaces.into_iter().find(|interface| interface.index == index)
    }

    /// Does what card `at`'s client asks.
    fn act(&mut self, at: usize, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Send { to, message } => self.send(at, to, &message),
                Action::Bind(lease) => self.bind(at, &lease),
                Action::Unbind => self.unbind(at),
            }
        }
    }

    fn send(&mut self, at: usize, to: Ipv4Addr, message: &[u8]) {
        let card = &mut self.cards[at];
        let How::Dhcp { socket, .. } = &card.how else { return };
        let failure = socket
            .send_to(message, SocketAddrV4::new(to, SERVER_PORT))
            .err()
            .map(|err| util::strip_os_error(&err));
        if failure.is_some() && failure != card.failure {
            eprintln!(
                "netd: {}: cannot send to {to}: {}",
                card.name,
                failure.as_deref().unwrap_or("")
            );
        }
        card.failure = failure;
    }

    fn bind(&mut self, at: usize, lease: &Lease) {
        let Some(interface) = Netd::interface(self.cards[at].index) else { return };
        let mut card = std::mem::replace(&mut self.cards[at], placeholder());
        let setup = self.setup(&card.name, &interface.mac);
        let changed = card.given.first() != Some(&lease.address)
            || card.router != lease.router
            || !interface.addresses.contains(&lease.address);
        let mut ours = vec![lease.address];
        ours.extend(setup.addresses.iter().filter(|cidr| cidr.ip != lease.address.ip));
        let name = format!("{} ({})", card.name, interface.mac);
        if let Err(err) = write_lease(card.mac, &lease_text(lease, &name, wall())) {
            eprintln!("netd: {}: cannot keep its lease: {err}", card.name);
        }
        if !changed || self.give(&mut card, &interface, &ours, lease.router) {
            let mut dns = setup.dns.clone();
            dns.extend(lease.dns.iter().filter(|dns| !setup.dns.contains(dns)));
            card.dns = dns;
            card.domain.clone_from(&lease.domain);
            let router = lease.router.map_or(String::new(), |router| format!(", router {router}"));
            let dns: Vec<String> = lease.dns.iter().map(Ipv4Addr::to_string).collect();
            let dns =
                if dns.is_empty() { String::new() } else { format!(", DNS {}", dns.join(" ")) };
            let verb = if changed { "has" } else { "keeps" };
            eprintln!(
                "netd: {} {verb} {} from {} for {}{router}{dns}",
                card.name,
                lease.address,
                lease.server,
                lasting(lease.seconds),
            );
        }
        self.cards[at] = card;
        self.write_resolv();
    }

    fn unbind(&mut self, at: usize) {
        let mut card = std::mem::replace(&mut self.cards[at], placeholder());
        forget_lease(card.mac);
        if let Some(interface) = Netd::interface(card.index) {
            let setup = self.setup(&card.name, &interface.mac);
            let lost: Vec<String> = card.given.iter().map(Cidr::to_string).collect();
            if self.give(&mut card, &interface, &setup.addresses, setup.router) {
                eprintln!("netd: {} gave up {}; asking for another", card.name, lost.join(", "));
            }
            card.dns = setup.dns;
            card.domain = None;
        }
        self.cards[at] = card;
        self.write_resolv();
    }

    /// Writes the cards' DNS servers into resolv.conf, or takes it away
    /// when there are none.
    fn write_resolv(&self) {
        let mut servers: Vec<Ipv4Addr> = Vec::new();
        let mut search: Vec<String> = Vec::new();
        for card in &self.cards {
            for dns in &card.dns {
                if !servers.contains(dns) {
                    servers.push(*dns);
                }
            }
            if let Some(domain) = &card.domain
                && !search.contains(domain)
            {
                search.push(domain.clone());
            }
        }
        if servers.is_empty() && search.is_empty() {
            let _ = fs::remove_file(RESOLV);
            return;
        }
        let text = resolv_text(&servers, &search);
        if fs::read_to_string(RESOLV).is_ok_and(|old| old == text) {
            return;
        }
        let new = format!("{RESOLV}.new");
        let written = Path::new(RESOLV)
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&new, &text))
            .and_then(|()| fs::rename(&new, RESOLV));
        if let Err(err) = written {
            eprintln!("netd: cannot write {RESOLV}: {}", util::strip_os_error(&err));
        }
    }

    fn run(&mut self) -> io::Result<()> {
        loop {
            if STOP.load(Ordering::Relaxed) {
                return Ok(());
            }
            if RELOAD.swap(false, Ordering::Relaxed) {
                self.reload();
            }
            let now = Instant::now();
            if self.scanned.is_none_or(|scanned| now >= scanned + SCAN) {
                self.scan(now);
            }
            for at in 0..self.cards.len() {
                let actions = match &mut self.cards[at].how {
                    How::Dhcp { client, .. } => client.tick(now),
                    _ => continue,
                };
                self.act(at, actions);
            }
            let mut fds: Vec<libc::pollfd> = Vec::new();
            let mut deadline = self.scanned.map_or(now, |scanned| scanned + SCAN);
            for card in &self.cards {
                if let How::Dhcp { socket, client } = &card.how {
                    fds.push(libc::pollfd {
                        fd: socket.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    });
                    deadline = deadline.min(client.next());
                }
            }
            let timeout = deadline.saturating_duration_since(now).as_millis().min(60_000);
            // SAFETY: `fds` holds valid pollfds.
            let ready = unsafe {
                libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout as libc::c_int + 1)
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
                continue;
            }
            let now = Instant::now();
            let mut buffer = [0u8; dhcp::MESSAGE_MAX as usize];
            for at in 0..self.cards.len() {
                while let How::Dhcp { socket, client } = &mut self.cards[at].how
                    && let Ok((len, from)) = socket.recv_from(&mut buffer)
                {
                    if from.port() != SERVER_PORT {
                        continue;
                    }
                    let actions = client.receive(&buffer[..len], now);
                    self.act(at, actions);
                }
            }
        }
    }

    /// Reads the settings again and sets each card up afresh as they say.
    fn reload(&mut self) {
        eprintln!("netd: reading {SETTINGS} again");
        self.read_settings();
        let now = Instant::now();
        let cards = std::mem::take(&mut self.cards);
        for mut card in cards {
            // Its addresses from the old settings go; a lease's stay with
            // the lease, which the new client goes on with.
            if let Some(interface) = Netd::interface(card.index) {
                let leased: Vec<Cidr> = match &card.how {
                    How::Dhcp { client, .. } => {
                        client.lease().map(|l| l.address).into_iter().collect()
                    }
                    _ => Vec::new(),
                };
                let setup = self.setup(&card.name, &interface.mac);
                if !setup.dhcp() && !leased.is_empty() {
                    forget_lease(card.mac);
                }
                let keep = if setup.dhcp() { leased } else { Vec::new() };
                let router = card.router.filter(|_| !keep.is_empty());
                if card.given != keep && !matches!(card.how, How::Alone) {
                    self.give(&mut card, &interface, &keep, router);
                }
            }
            drop(card);
        }
        self.scan(now);
        self.write_resolv();
    }
}

/// A card for the moment one is out of its place.
fn placeholder() -> Card {
    Card {
        name: String::new(),
        index: 0,
        mac: [0; 6],
        carrier: false,
        how: How::Alone,
        given: Vec::new(),
        router: None,
        dns: Vec::new(),
        domain: None,
        failure: None,
    }
}

/// `3600 s` as `1 hour`, and the rest so.
fn lasting(seconds: u32) -> String {
    match seconds {
        u32::MAX => "ever".into(),
        s if s % 86_400 == 0 => plural(s / 86_400, "day"),
        s if s % 3600 == 0 => plural(s / 3600, "hour"),
        s if s % 60 == 0 => plural(s / 60, "minute"),
        s => plural(s, "second"),
    }
}

fn plural(count: u32, unit: &str) -> String {
    if count == 1 { format!("1 {unit}") } else { format!("{count} {unit}s") }
}

/// The file of the lease of the card with the address `mac`.
fn lease_path(mac: [u8; 6]) -> String {
    let name: Vec<String> = mac.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{LEASES}/{}.lease", name.join("-"))
}

fn read_lease(mac: [u8; 6]) -> Option<(Lease, u64)> {
    parse_lease(&fs::read_to_string(lease_path(mac)).ok()?)
}

fn write_lease(mac: [u8; 6], text: &str) -> io::Result<()> {
    fs::create_dir_all(LEASES)?;
    let path = lease_path(mac);
    let new = format!("{path}.new");
    let mut file = fs::File::create(&new)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(new, path)
}

fn forget_lease(mac: [u8; 6]) {
    let _ = fs::remove_file(lease_path(mac));
}

/// netd(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    for (signal, handler) in
        [(libc::SIGTERM, on_term as extern "C" fn(libc::c_int)), (libc::SIGHUP, on_hangup)]
    {
        // SAFETY: all zeros is a valid sigaction: numbers, a signal set
        // and a function address.
        let mut action: libc::sigaction = unsafe { util::zeroed() };
        action.sa_sigaction = handler as *const () as usize;
        // SAFETY: the handlers only store to an atomic.
        unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) };
    }
    let mut netd =
        Netd { cards: Vec::new(), settings: Vec::new(), scanned: None, network: &ip::Slots };
    netd.read_settings();
    netd.run().map_err(|e| util::failed(&"netd", e))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(text: &str) -> Cidr {
        Cidr::parse(text).unwrap()
    }

    #[test]
    fn the_settings_name_cards_and_what_they_have() {
        let (cards, wrong) = parse_settings(
            "# a comment\n\
             [net0]\n\
             address = 192.168.1.10/24\n\
             address = 192.168.1.11/24\n\
             router = 192.168.1.1\n\
             dns = 192.168.1.1\n\
             dns = 9.9.9.9\n\
             \n\
             [52:54:00:AB:CD:EF]\n\
             dhcp = no\n\
             [net2]\n\
             dhcp = yes\n\
             address = 10.1.0.1/16\n",
        );
        assert_eq!(wrong, Vec::<String>::new());
        let net0 = Setup {
            dhcp: None,
            addresses: vec![cidr("192.168.1.10/24"), cidr("192.168.1.11/24")],
            router: Some(Ipv4Addr::new(192, 168, 1, 1)),
            dns: vec![Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(9, 9, 9, 9)],
        };
        assert_eq!(cards[0], ("net0".to_string(), net0.clone()));
        assert!(!net0.dhcp() && !net0.alone());
        assert_eq!(cards[1].0, "52:54:00:ab:cd:ef");
        assert!(cards[1].1.alone());
        assert!(cards[2].1.dhcp() && !cards[2].1.alone());
        assert!(Setup::default().dhcp());
    }

    #[test]
    fn what_is_wrong_in_the_settings_is_said_by_line() {
        let (cards, wrong) = parse_settings(
            "dns = 1.1.1.1\n\
             [net0]\n\
             address = 300.1.1.1/24\n\
             address = 127.0.0.1/8\n\
             router = 10.0.0.1\n\
             router = 10.0.0.2\n\
             dhcp = maybe\n\
             colour = blue\n\
             just words\n\
             address = 192.168.1.10/24\n\
             [net0]\n\
             dns = 2.2.2.2\n\
             []\n",
        );
        assert_eq!(
            wrong,
            [
                "/system/settings/network:1: dns before any `[card]`",
                "/system/settings/network:3: '300.1.1.1/24' is not a address here",
                "/system/settings/network:4: '127.0.0.1/8' is not a address here",
                "/system/settings/network:6: '10.0.0.2' is not a router here",
                "/system/settings/network:7: 'maybe' is not a dhcp here",
                "/system/settings/network:8: no setting is called colour",
                "/system/settings/network:9: not `setting = value` or `[card]`",
                "/system/settings/network:11: '[net0]' names no card, or one named before",
                "/system/settings/network:13: '[]' names no card, or one named before",
                "/system/settings/network: [net0]'s router 10.0.0.1 is on none of its networks",
            ]
        );
        // The second [net0]'s lines went nowhere.
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].1.dns, Vec::<Ipv4Addr>::new());
        assert_eq!(cards[0].1.addresses, [cidr("192.168.1.10/24")]);
    }

    #[test]
    fn a_lease_file_reads_back_as_it_was_written() {
        let lease = Lease {
            address: cidr("10.0.2.15/24"),
            router: Some(Ipv4Addr::new(10, 0, 2, 2)),
            dns: vec![Ipv4Addr::new(10, 0, 2, 3), Ipv4Addr::new(1, 1, 1, 1)],
            domain: Some("example.org".into()),
            server: Ipv4Addr::new(10, 0, 2, 2),
            seconds: 86_400,
            renew: 43_200,
            rebind: 75_600,
        };
        let text = lease_text(&lease, "net0 (52:54:00:12:34:56)", 1_761_900_000);
        assert_eq!(
            text,
            "# netd(8)'s lease for net0 (52:54:00:12:34:56).\n\
             address = 10.0.2.15/24\n\
             router = 10.0.2.2\n\
             dns = 10.0.2.3\n\
             dns = 1.1.1.1\n\
             domain = example.org\n\
             server = 10.0.2.2\n\
             obtained = 1761900000\n\
             seconds = 86400\n\
             renew = 43200\n\
             rebind = 75600\n"
        );
        assert_eq!(parse_lease(&text), Some((lease.clone(), 1_761_900_000)));
        let bare = Lease { router: None, dns: Vec::new(), domain: None, ..lease };
        assert_eq!(parse_lease(&lease_text(&bare, "net0", 5)), Some((bare, 5)));
        // A file cut short is no lease.
        assert_eq!(parse_lease(&text[..text.len() - 14]), None);
        assert_eq!(parse_lease("address = 10.0.2.15/24\n"), None);
    }

    #[test]
    fn resolv_conf_has_three_servers_and_the_domains() {
        let servers = [[10, 0, 2, 3], [1, 1, 1, 1], [9, 9, 9, 9], [8, 8, 8, 8]].map(Ipv4Addr::from);
        assert_eq!(
            resolv_text(&servers, &["example.org".into(), "lan".into()]),
            "# Written by netd(8) from what DHCP and /system/settings/network give.\n\
             # /etc/resolv.conf leads here; make it a file of your own for other servers.\n\
             nameserver 10.0.2.3\n\
             nameserver 1.1.1.1\n\
             nameserver 9.9.9.9\n\
             search example.org lan\n"
        );
        assert!(resolv_text(&servers[..1], &[]).ends_with("\nnameserver 10.0.2.3\n"));
        assert!(!resolv_text(&servers[..1], &[]).contains("search"));
    }

    #[test]
    fn netd_changes_only_the_addresses_it_gave() {
        let (a, b, c, d, e) = (
            cidr("10.0.2.15/24"),
            cidr("10.0.2.16/24"),
            cidr("192.168.5.1/24"),
            cidr("172.16.0.1/12"),
            cidr("172.17.0.1/12"),
        );
        // An administrator's address stays when the lease's changes.
        assert_eq!(merge(&[a, c], &[a], &[b]), [b, c]);
        // Given up.
        assert_eq!(merge(&[a, c], &[a], &[]), [c]);
        // Given again, with the same address only once.
        assert_eq!(merge(&[c, a], &[], &[a]), [a, c]);
        // At most four.
        assert_eq!(merge(&[c, d, e], &[], &[a, b]), [a, b, c, d]);
    }

    #[test]
    fn mac_addresses_and_lease_files_are_named_alike() {
        assert_eq!(mac_of("52:54:00:12:34:56"), Some([0x52, 0x54, 0, 0x12, 0x34, 0x56]));
        assert_eq!(mac_of("52:54:00:12:34"), None);
        assert_eq!(mac_of("52:54:00:12:34:56:78"), None);
        assert_eq!(mac_of("zz:54:00:12:34:56"), None);
        assert_eq!(
            lease_path([0x52, 0x54, 0, 0x12, 0x34, 0x56]),
            "/system/state/netd/52-54-00-12-34-56.lease"
        );
        assert_eq!(lasting(86_400), "1 day");
        assert_eq!(lasting(7200), "2 hours");
        assert_eq!(lasting(90), "90 seconds");
        assert_eq!(lasting(u32::MAX), "ever");
    }
}
