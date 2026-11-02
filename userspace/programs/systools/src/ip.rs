// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ip(8): the network interfaces, their addresses and their routes, shown
//! as iproute2's ip shows them and changed with its syntax (ADR 0190).
//!
//! `ip link`, `ip address` and `ip route` read `/kernel/class/net`
//! (radix-sysfs(5)) and `/processes/net/route` (radix-proc(5)), which
//! anyone may. `ip address add`, `del` and `flush`, and `ip route add`,
//! `replace` and `del` of the default route, set an interface up through
//! radix-network(2)'s `configure`, which needs a network capability with
//! the control right, as a program has when it was given admin.network.
//! Each change reads what the interface has, changes it and gives it the
//! whole again.
//!
//! IPv6 (ADR 0332): `ip address` shows the addresses each interface has
//! (its `ipv6` file), those it made from router adverts with their
//! lifetimes, and `ip -6 route` its networks and routers. Adding or
//! deleting an IPv6 address, or the default IPv6 route, changes only
//! those it was given (`configure_ipv6`, given_only): how it makes the
//! others is netd(8)'s.
//!
//! Errors are iproute2's: a mistake in the command line ends with status
//! 1, and what the kernel refuses with 2, said as `RTNETLINK answers:`
//! and the C library's text, which scripts look for.

use std::fs;
use std::io::{self, Write};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

use radix_abi::Status;
use radix_abi::errors::{File as FileError, Kernel};
use radix_abi::network::{self, InterfaceAddress, InterfaceAddressIpv6, Ipv6Flags, Ipv6Settings};
use radix_abi::startup::{NETWORK_SLOTS, SLOT_NETWORK};

use crate::args::Args;
use crate::sys::SYS;
use crate::util::{Fail, fail_with, usage};

/// Where the kernel shows the interfaces and the routes.
pub const INTERFACES: &str = "/kernel/class/net";
pub const ROUTES: &str = "/processes/net/route";

/// The most addresses an interface takes (radix-network(2)).
const ADDRESSES_MAX: usize = network::ADDRESSES_MAX as usize;
/// The most IPv6 addresses it is given besides those it makes.
const ADDRESSES_IPV6_MAX: usize = network::ADDRESSES_IPV6_MAX as usize;

/// An IPv4 address and the length of its network's prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    pub ip: Ipv4Addr,
    pub prefix: u8,
}

impl Cidr {
    /// `10.0.2.15/24`, or an address alone, which is a /32.
    pub fn parse(text: &str) -> Option<Cidr> {
        let (ip, prefix) = match text.split_once('/') {
            Some((ip, prefix)) => (ip, prefix.parse().ok().filter(|&prefix| prefix <= 32)?),
            None => (text, 32),
        };
        Some(Cidr { ip: ip.parse().ok()?, prefix })
    }

    pub fn mask(self) -> u32 {
        u32::MAX.checked_shl(32 - u32::from(self.prefix)).unwrap_or(0)
    }

    /// The network's first address.
    pub fn network(self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.ip) & self.mask())
    }

    /// The address that reaches every host of the network: none for a
    /// /31 or /32, which have no such address.
    pub fn broadcast(self) -> Option<Ipv4Addr> {
        (self.prefix < 31).then(|| Ipv4Addr::from(u32::from(self.ip) | !self.mask()))
    }

    pub fn contains(self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == u32::from(self.network())
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.ip, self.prefix)
    }
}

/// An IPv6 address and the length of its network's prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr6 {
    pub ip: Ipv6Addr,
    pub prefix: u8,
}

impl Cidr6 {
    /// `2001:db8::5/64`, or an address alone, which is a /128.
    pub fn parse(text: &str) -> Option<Cidr6> {
        let (ip, prefix) = match text.split_once('/') {
            Some((ip, prefix)) => (ip, prefix.parse().ok().filter(|&prefix| prefix <= 128)?),
            None => (text, 128),
        };
        Some(Cidr6 { ip: ip.parse().ok()?, prefix })
    }

    fn mask(self) -> u128 {
        u128::MAX.checked_shl(128 - u32::from(self.prefix)).unwrap_or(0)
    }

    /// The network's first address.
    pub fn network(self) -> Ipv6Addr {
        Ipv6Addr::from(u128::from(self.ip) & self.mask())
    }

    pub fn contains(self, ip: Ipv6Addr) -> bool {
        u128::from(ip) & self.mask() == u128::from(self.network())
    }
}

impl std::fmt::Display for Cidr6 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.ip, self.prefix)
    }
}

/// How an interface came by an IPv6 address (radix-sysfs(5)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Its fe80::/64 one, made at once.
    LinkLocal,
    /// Made from a prefix a router advertised, the same at each boot.
    Stable,
    /// Made from one too, for a day, to send from.
    Temporary,
    /// Given to it, by ip(8) or netd(8)'s settings.
    Given,
}

/// One of an interface's IPv6 addresses, as its `ipv6` file shows it: its
/// lifetimes in seconds, `None` for forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address6 {
    pub cidr: Cidr6,
    pub origin: Origin,
    pub preferred: Option<u64>,
    pub valid: Option<u64>,
    /// It is being tested, and is not the interface's yet.
    pub tentative: bool,
}

/// What an interface's `ipv6` file says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ipv6 {
    pub addresses: Vec<Address6>,
    /// The routers that advertised themselves, and for how many seconds
    /// more.
    pub routers: Vec<(Ipv6Addr, u64)>,
    /// The one it was given, the default route while none advertises.
    pub given_router: Option<Ipv6Addr>,
    /// The DNS servers routers advertised, and for how many seconds.
    pub dns: Vec<(Ipv6Addr, u64)>,
    pub mtu: Option<u32>,
    /// Routers said that DHCPv6 gives addresses (`managed`), or other
    /// settings (`other`).
    pub managed: bool,
    pub other: bool,
}

impl Ipv6 {
    /// The `ipv6` file's lines: unknown ones are passed over.
    pub fn parse(text: &str) -> Ipv6 {
        let mut ipv6 = Ipv6::default();
        let seconds = |word: Option<&str>| match word {
            Some("forever") => Some(None),
            Some(number) => number.parse().ok().map(Some),
            None => None,
        };
        for line in text.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            match words[..] {
                ["address", cidr, kind, "preferred", preferred, "valid", valid, ref rest @ ..] => {
                    let origin = match kind {
                        "link-local" => Origin::LinkLocal,
                        "stable" => Origin::Stable,
                        "temporary" => Origin::Temporary,
                        "given" => Origin::Given,
                        _ => continue,
                    };
                    let (Some(cidr), Some(preferred), Some(valid)) =
                        (Cidr6::parse(cidr), seconds(Some(preferred)), seconds(Some(valid)))
                    else {
                        continue;
                    };
                    let tentative = rest.contains(&"tentative");
                    ipv6.addresses.push(Address6 { cidr, origin, preferred, valid, tentative });
                }
                ["router", router, "given"] => ipv6.given_router = router.parse().ok(),
                ["router", router, "lifetime", lifetime] => {
                    if let (Ok(router), Ok(lifetime)) = (router.parse(), lifetime.parse()) {
                        ipv6.routers.push((router, lifetime));
                    }
                }
                ["dns", server, "lifetime", lifetime] => {
                    if let (Ok(server), Ok(lifetime)) = (server.parse(), lifetime.parse()) {
                        ipv6.dns.push((server, lifetime));
                    }
                }
                ["mtu", mtu] => ipv6.mtu = mtu.parse().ok(),
                ["dhcp", "managed"] => ipv6.managed = true,
                ["dhcp", "other"] => ipv6.other = true,
                _ => {}
            }
        }
        ipv6
    }

    /// The addresses it was given, with their prefixes.
    pub fn given(&self) -> Vec<Cidr6> {
        self.addresses.iter().filter(|a| a.origin == Origin::Given).map(|a| a.cidr).collect()
    }

    /// Its default route: the first router that advertised itself, else
    /// the one it was given.
    pub fn router(&self) -> Option<Ipv6Addr> {
        self.routers.first().map(|(router, _)| *router).or(self.given_router)
    }
}

/// Whether `ip` is a link-local IPv6 address, fe80::/10.
pub fn link_local(ip: Ipv6Addr) -> bool {
    ip.segments()[0] & 0xffc0 == 0xfe80
}

/// An interface, as `/kernel/class/net` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub index: u32,
    pub mtu: u32,
    pub mac: String,
    pub loopback: bool,
    /// Its link is up.
    pub carrier: bool,
    /// `up`, `down` or `unknown`.
    pub operstate: String,
    pub addresses: Vec<Cidr>,
    pub router: Option<Ipv4Addr>,
    pub ipv6: Ipv6,
    /// Bytes, packets, errors and dropped, received then sent.
    pub received: [u64; 4],
    pub sent: [u64; 4],
}

/// What a file of a directory says, without its newline.
fn read(dir: &Path, file: &str) -> Option<String> {
    let text = fs::read_to_string(dir.join(file)).ok()?;
    Some(text.trim_end_matches('\n').to_string())
}

fn read_number<T: std::str::FromStr>(dir: &Path, file: &str) -> Option<T> {
    read(dir, file)?.trim().parse().ok()
}

/// The routers in the routes file at `routes`, by interface: its default
/// routes' gateways.
fn read_routers(routes: &Path) -> Vec<(String, Ipv4Addr)> {
    let Ok(text) = fs::read_to_string(routes) else { return Vec::new() };
    // An address is its bytes, in network order, read as a little-endian
    // number.
    let hex = |field: &str| {
        u32::from_str_radix(field, 16).ok().map(|number| Ipv4Addr::from(number.to_le_bytes()))
    };
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').map(str::trim).collect();
            let (name, destination, gateway, mask) =
                (fields.first()?, hex(fields.get(1)?)?, hex(fields.get(2)?)?, hex(fields.get(7)?)?);
            (destination.is_unspecified() && mask.is_unspecified() && !gateway.is_unspecified())
                .then(|| ((*name).to_string(), gateway))
        })
        .collect()
}

/// The interfaces under `dir`, in the order of their numbers, with the
/// routers the routes file at `routes` gives them.
pub fn read_interfaces(dir: &Path, routes: &Path) -> io::Result<Vec<Interface>> {
    let routers = read_routers(routes);
    let mut interfaces = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
        let path = entry.path();
        let Some(index) = read_number(&path, "ifindex") else { continue };
        let statistic =
            |file: &str| -> u64 { read_number(&path.join("statistics"), file).unwrap_or(0) };
        interfaces.push(Interface {
            index,
            mtu: read_number(&path, "mtu").unwrap_or(0),
            mac: read(&path, "address").unwrap_or_default(),
            loopback: read_number::<u16>(&path, "type") == Some(772),
            carrier: read_number::<u8>(&path, "carrier") == Some(1),
            operstate: read(&path, "operstate").unwrap_or_default(),
            addresses: read(&path, "ipv4")
                .unwrap_or_default()
                .lines()
                .filter_map(Cidr::parse)
                .collect(),
            router: routers.iter().find(|(card, _)| *card == name).map(|(_, router)| *router),
            ipv6: Ipv6::parse(&read(&path, "ipv6").unwrap_or_default()),
            received: ["rx_bytes", "rx_packets", "rx_errors", "rx_dropped"].map(statistic),
            sent: ["tx_bytes", "tx_packets", "tx_errors", "tx_dropped"].map(statistic),
            name,
        });
    }
    interfaces.sort_by_key(|interface| interface.index);
    Ok(interfaces)
}

/// How ip(8) changes an interface: radix-network(2)'s `configure` and
/// `configure_ipv6`, so that the tests can answer instead.
pub trait Network {
    /// Gives interface `index` `addresses`, the first the one it sends
    /// from, and `router`, in place of those it had.
    fn configure(
        &self,
        index: u32,
        addresses: &[Cidr],
        router: Option<Ipv4Addr>,
    ) -> Result<(), Status>;

    /// Sets how interface `index` makes its IPv6 addresses, `settings`
    /// (or, with `None`, only what it was given changes), and gives it
    /// `addresses` and `router` in place of those it was given.
    fn configure_ipv6(
        &self,
        index: u32,
        settings: Option<&Ipv6Settings>,
        addresses: &[Cidr6],
        router: Option<Ipv6Addr>,
    ) -> Result<(), Status>;
}

/// Calls `call` with each network capability's slot in turn until one
/// allows it.
fn through_slots(mut call: impl FnMut(u32) -> Result<(), Status>) -> Result<(), Status> {
    let mut refusal = Kernel::EmptySlot.status(0);
    for slot in SLOT_NETWORK..SLOT_NETWORK + NETWORK_SLOTS {
        match call(slot) {
            Ok(()) => return Ok(()),
            Err(status) => match Kernel::from_status(status) {
                Some(Kernel::MissingRights) => refusal = status,
                Some(Kernel::NoSuchSlot | Kernel::EmptySlot | Kernel::WrongKind) => {}
                _ => return Err(status),
            },
        }
    }
    Err(refusal)
}

/// The kernel, through the network capabilities in the NETWORK_SLOTS
/// slots from SLOT_NETWORK: the first that allows it.
pub struct Slots;

impl Network for Slots {
    fn configure(
        &self,
        index: u32,
        addresses: &[Cidr],
        router: Option<Ipv4Addr>,
    ) -> Result<(), Status> {
        let mut bytes = Vec::with_capacity(addresses.len() * InterfaceAddress::SIZE);
        for cidr in addresses {
            bytes.extend_from_slice(&cidr.ip.octets());
            bytes.extend_from_slice(&[cidr.prefix, 0, 0, 0]);
        }
        let router = router.map(|router| router.octets());
        let router: &[u8] = router.as_ref().map_or(&[], |router| router);
        through_slots(|slot| network::configure(&SYS, slot, index, &bytes, router))
    }

    fn configure_ipv6(
        &self,
        index: u32,
        settings: Option<&Ipv6Settings>,
        addresses: &[Cidr6],
        router: Option<Ipv6Addr>,
    ) -> Result<(), Status> {
        let given = Ipv6Settings { flags: Ipv6Flags::GIVEN_ONLY.bits(), ..Ipv6Settings::default() };
        let settings = settings.unwrap_or(&given);
        let mut bytes = [0u8; Ipv6Settings::SIZE];
        bytes[Ipv6Settings::OFFSET_FLAGS..Ipv6Settings::OFFSET_FLAGS + 4]
            .copy_from_slice(&settings.flags.to_le_bytes());
        bytes[Ipv6Settings::OFFSET_SECRET..Ipv6Settings::OFFSET_SECRET + 16]
            .copy_from_slice(&settings.secret);
        let settings = bytes;
        let mut bytes = Vec::with_capacity(addresses.len() * InterfaceAddressIpv6::SIZE);
        for cidr in addresses {
            bytes.extend_from_slice(&cidr.ip.octets());
            bytes.extend_from_slice(&[cidr.prefix, 0, 0, 0, 0, 0, 0, 0]);
        }
        let router = router.map(|router| router.octets());
        let router: &[u8] = router.as_ref().map_or(&[], |router| router);
        through_slots(|slot| {
            network::configure_ipv6(&SYS, slot, index, &settings, &bytes, router)
        })
    }
}

/// A failure the kernel's way, as iproute2 says one: exit status 2.
fn answers(why: &str) -> Fail {
    fail_with(2, format!("RTNETLINK answers: {why}"))
}

/// What the kernel refused, in the C library's words.
fn refused(status: Status) -> Fail {
    let why = match Kernel::from_status(status) {
        Some(
            Kernel::NoSuchSlot | Kernel::EmptySlot | Kernel::WrongKind | Kernel::MissingRights,
        ) => {
            return fail_with(
                2,
                "RTNETLINK answers: Operation not permitted (it needs admin.network, spark(8))"
                    .to_string(),
            );
        }
        Some(Kernel::NotSupported) => "Operation not supported",
        Some(Kernel::InvalidArgument) => "Invalid argument",
        _ if FileError::from_status(status) == Some(FileError::NoDevice) => "No such device",
        _ => return answers(&crate::sys::status_name(status.0)),
    };
    answers(why)
}

/// A version of IP, as `-4` and `-6` choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    V4,
    V6,
}

/// What ip(8) is asked: its options, and the words after them.
struct Command {
    /// Only addresses and routes of this version, with `-4` or `-6`.
    family: Option<Family>,
    brief: bool,
    statistics: bool,
    /// The colours, with `-color` (ADR 0260).
    colors: Option<Colors>,
    words: Vec<String>,
}

/// The colours of `-color`, as SGR parameters: iproute2's, or those
/// `IP_COLORS` gives (`name=36:up=32:...`), which nukesh(1) makes from
/// colors.toml(5).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Colors {
    name: String,
    up: String,
    down: String,
    address: String,
    mac: String,
}

impl Colors {
    /// iproute2's colours, changed by `IP_COLORS`'s.
    fn new(ip_colors: Option<&str>) -> Colors {
        let mut colors = Colors {
            name: "36".into(),
            up: "32".into(),
            down: "31".into(),
            address: "35".into(),
            mac: "33".into(),
        };
        for pair in ip_colors.unwrap_or_default().split(':') {
            let Some((key, value)) = pair.split_once('=') else { continue };
            if !value.bytes().all(|b| b.is_ascii_digit() || b == b';') {
                continue;
            }
            let slot = match key {
                "name" => &mut colors.name,
                "up" => &mut colors.up,
                "down" => &mut colors.down,
                "address" => &mut colors.address,
                "mac" => &mut colors.mac,
                _ => continue,
            };
            *slot = value.to_string();
        }
        colors
    }
}

/// `text` in the colour `pick` chooses, with `-color`.
fn paint(command: &Command, pick: fn(&Colors) -> &str, text: &str) -> String {
    match &command.colors {
        Some(colors) if !pick(colors).is_empty() && pick(colors) != "0" => {
            format!("\x1b[{}m{text}\x1b[0m", pick(colors))
        }
        _ => text.to_string(),
    }
}

/// An interface's state, `UP` in green and `DOWN` in red with `-color`.
fn painted_state(command: &Command, interface: &Interface) -> String {
    let state = state(interface);
    match state.as_str() {
        "UP" => paint(command, |c| &c.up, &state),
        "DOWN" | "LOWERLAYERDOWN" => paint(command, |c| &c.down, &state),
        _ => state,
    }
}

/// Whether `word` is one of `full`'s first letters, at least `least` of
/// them, as iproute2 matches its words.
fn matches(word: &str, full: &str, least: usize) -> bool {
    word.len() >= least && full.starts_with(word)
}

const HELP: &str = "Usage: ip [ OPTIONS ] OBJECT { COMMAND | help }
where  OBJECT := { link | address | route }
       OPTIONS := { -4 | -6 | -s[tatistics] | -br[ief] | -c[olor][={always|auto|never}] }
       ip link [ show [ [ dev ] NAME ] ]
       ip link set [ dev ] NAME up
       ip address [ show [ [ dev ] NAME ] ]
       ip address { add | del } IFADDR dev NAME
       ip address flush dev NAME
       ip route [ show [ dev NAME ] ]
       ip -6 route [ show [ dev NAME ] ]
       ip route { add | replace } default via ADDRESS [ dev NAME ]
       ip route del default [ via ADDRESS ] [ dev NAME ]
";

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut command =
        Command { family: None, brief: false, statistics: false, colors: None, words: Vec::new() };
    let mut options = true;
    for word in args.rest() {
        let word = word.into_string().map_err(|_| usage("an argument is not UTF-8".into()))?;
        if options && let Some(option) = word.strip_prefix('-').map(|o| o.trim_start_matches('-')) {
            match option {
                "4" => command.family = Some(Family::V4),
                "6" => command.family = Some(Family::V6),
                "h" | "help" => {
                    print!("{HELP}");
                    return Ok(0);
                }
                _ if matches(option, "brief", 2) => command.brief = true,
                // As iproute2: -c, -color, -color=always|auto|never; auto
                // only on a terminal, and never with NO_COLOR set.
                _ if matches(option.split('=').next().unwrap_or(""), "color", 1) => {
                    let when = option.split_once('=').map_or("always", |(_, when)| when);
                    let on = match when {
                        "always" => true,
                        "never" => false,
                        "auto" => {
                            crate::util::is_terminal(1)
                                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
                        }
                        other => {
                            return Err(usage(format!(
                                "Option \"-color={other}\" is unknown, try \"ip -help\"."
                            )));
                        }
                    };
                    command.colors =
                        on.then(|| Colors::new(std::env::var("IP_COLORS").ok().as_deref()));
                }
                _ if matches(option, "statistics", 1) || option == "stats" => {
                    command.statistics = true;
                }
                _ => {
                    return Err(usage(format!(
                        "Option \"-{option}\" is unknown, try \"ip -help\"."
                    )));
                }
            }
            continue;
        }
        options = false;
        command.words.push(word);
    }
    let interfaces = read_interfaces(Path::new(INTERFACES), Path::new(ROUTES))
        .map_err(|error| fail_with(1, format!("Cannot read {INTERFACES}: {error}")))?;
    let mut out = io::stdout().lock();
    run(&command, &interfaces, &Slots, &mut out)
}

/// Runs `command` on `interfaces`, changing them through `network`.
fn run(
    command: &Command,
    interfaces: &[Interface],
    network: &dyn Network,
    out: &mut dyn Write,
) -> Result<i32, Fail> {
    let words: Vec<&str> = command.words.iter().map(String::as_str).collect();
    let Some((object, rest)) = words.split_first() else {
        write!(out, "{HELP}")?;
        return Ok(1);
    };
    let object = *object;
    let (verb, rest) = rest.split_first().map_or(("show", &[][..]), |(v, r)| (*v, r));
    let show = matches(verb, "show", 1) || matches(verb, "list", 1) || verb == "lst";
    if verb == "help" {
        write!(out, "{HELP}")?;
        return Ok(0);
    }
    if matches(object, "link", 1) {
        if show {
            let shown = selected(interfaces, rest)?;
            return link(command, &shown, out).map(|()| 0);
        }
        if matches(verb, "set", 1) {
            return set_link(interfaces, rest);
        }
    } else if matches(object, "address", 1) || object == "addr" {
        if show {
            let shown = selected(interfaces, rest)?;
            return address(command, &shown, out).map(|()| 0);
        }
        if matches(verb, "add", 1) || matches(verb, "delete", 1) || matches(verb, "flush", 1) {
            return change_address(command.family, verb, interfaces, rest, network);
        }
    } else if matches(object, "route", 1) {
        if show {
            let shown = selected(interfaces, rest)?;
            if command.family == Some(Family::V6) {
                return route6(&shown, out).map(|()| 0);
            }
            return route(&shown, out).map(|()| 0);
        }
        if matches(verb, "add", 1) || matches(verb, "replace", 3) || matches(verb, "delete", 1) {
            return change_route(command.family, verb, interfaces, rest, network);
        }
    } else {
        return Err(usage(format!("Object \"{object}\" is unknown, try \"ip help\".")));
    }
    Err(usage(format!("Command \"{verb}\" is unknown, try \"ip {object} help\".")))
}

/// The interface named `name`.
fn named<'a>(interfaces: &'a [Interface], name: &str) -> Result<&'a Interface, Fail> {
    interfaces
        .iter()
        .find(|interface| interface.name == name)
        .ok_or_else(|| usage(format!("Device \"{name}\" does not exist.")))
}

/// The interfaces `show`'s words name, `[dev] NAME`, or all of them.
fn selected<'a>(interfaces: &'a [Interface], words: &[&str]) -> Result<Vec<&'a Interface>, Fail> {
    match words {
        [] => Ok(interfaces.iter().collect()),
        ["dev", name] | [name] => Ok(vec![named(interfaces, name)?]),
        [other, ..] => {
            Err(usage(format!("Error: either \"dev\" is duplicate, or \"{other}\" is a garbage.")))
        }
    }
}

/// An interface's flags, as ip(8) shows them: `<BROADCAST,UP,LOWER_UP>`.
fn flags(interface: &Interface) -> String {
    let mut flags = Vec::new();
    if !interface.carrier {
        flags.push("NO-CARRIER");
    }
    flags.push(if interface.loopback { "LOOPBACK" } else { "BROADCAST" });
    flags.push("UP");
    if interface.carrier {
        flags.push("LOWER_UP");
    }
    format!("<{}>", flags.join(","))
}

/// `state UP`, as ip(8) says it.
fn state(interface: &Interface) -> String {
    interface.operstate.to_ascii_uppercase()
}

/// The first line of an interface, and its link's.
fn heading(
    command: &Command,
    interface: &Interface,
    mode: bool,
    out: &mut dyn Write,
) -> io::Result<()> {
    writeln!(
        out,
        "{}: {}: {} mtu {} qdisc noqueue state {}{} group default",
        interface.index,
        paint(command, |c| &c.name, &interface.name),
        flags(interface),
        interface.mtu,
        painted_state(command, interface),
        if mode { " mode DEFAULT" } else { "" },
    )?;
    let (kind, broadcast) = if interface.loopback {
        ("loopback", "00:00:00:00:00:00")
    } else {
        ("ether", "ff:ff:ff:ff:ff:ff")
    };
    let mac = paint(command, |c| &c.mac, &interface.mac);
    writeln!(out, "    link/{kind} {mac} brd {broadcast}")
}

/// `ip link`.
fn link(command: &Command, interfaces: &[&Interface], out: &mut dyn Write) -> Result<(), Fail> {
    for interface in interfaces {
        if command.brief {
            writeln!(
                out,
                "{} {} {} {} ",
                padded(command, |c| &c.name, &interface.name, 16),
                padded_state(command, interface),
                paint(command, |c| &c.mac, &interface.mac),
                flags(interface)
            )?;
            continue;
        }
        heading(command, interface, true, out)?;
        if command.statistics {
            let [bytes, packets, errors, dropped] = interface.received;
            writeln!(out, "    RX:  bytes packets errors dropped  missed   mcast")?;
            writeln!(
                out,
                "    {bytes:>10} {packets:>7} {errors:>6} {dropped:>7} {:>7} {:>7}",
                0, 0
            )?;
            let [bytes, packets, errors, dropped] = interface.sent;
            writeln!(out, "    TX:  bytes packets errors dropped carrier collsns")?;
            writeln!(
                out,
                "    {bytes:>10} {packets:>7} {errors:>6} {dropped:>7} {:>7} {:>7}",
                0, 0
            )?;
        }
    }
    Ok(())
}

/// `ip address`: each interface, and its addresses, an address in the
/// network of one before it a secondary one, as on Linux.
fn address(command: &Command, interfaces: &[&Interface], out: &mut dyn Write) -> Result<(), Fail> {
    let v4 = command.family != Some(Family::V6);
    let v6 = command.family != Some(Family::V4);
    for interface in interfaces {
        let inet = if v4 { &interface.addresses[..] } else { &[] };
        let inet6 = if v6 { shown6(interface) } else { Vec::new() };
        // With -4 or -6, only the interfaces with addresses of it.
        if command.family.is_some() && inet.is_empty() && inet6.is_empty() {
            continue;
        }
        if command.brief {
            write!(
                out,
                "{} {} ",
                padded(command, |c| &c.name, &interface.name, 16),
                padded_state(command, interface)
            )?;
            for cidr in inet {
                write!(out, "{} ", paint(command, |c| &c.address, &cidr.to_string()))?;
            }
            for address in &inet6 {
                write!(out, "{} ", paint(command, |c| &c.address, &address.cidr.to_string()))?;
            }
            writeln!(out)?;
            continue;
        }
        heading(command, interface, false, out)?;
        for (at, cidr) in inet.iter().enumerate() {
            let secondary = interface.addresses[..at]
                .iter()
                .any(|earlier| earlier.contains(cidr.ip) && earlier.prefix == cidr.prefix);
            let broadcast =
                cidr.broadcast().filter(|_| !interface.loopback).map(|b| format!(" brd {b}"));
            writeln!(
                out,
                "    inet {}{} scope {}{} {}",
                paint(command, |c| &c.address, &cidr.to_string()),
                broadcast.unwrap_or_default(),
                if interface.loopback { "host" } else { "global" },
                if secondary { " secondary" } else { "" },
                paint(command, |c| &c.name, &interface.name),
            )?;
            writeln!(out, "       valid_lft forever preferred_lft forever")?;
        }
        let temporaries = inet6.iter().any(|address| address.origin == Origin::Temporary);
        for address in &inet6 {
            write!(
                out,
                "    inet6 {} scope {} ",
                paint(command, |c| &c.address, &address.cidr.to_string()),
                scope6(address.cidr.ip)
            )?;
            // iproute2's flags, in its order: `dynamic` for one that is
            // not permanent, `mngtmpaddr` for the one temporaries are
            // made from, `stable-privacy` for one made with the secret.
            let mut flags = Vec::new();
            if address.origin == Origin::Temporary {
                flags.push("temporary");
            }
            if address.preferred == Some(0) {
                flags.push("deprecated");
            }
            if address.tentative {
                flags.push("tentative");
            }
            if matches!(address.origin, Origin::Stable | Origin::Temporary) {
                flags.push("dynamic");
            }
            if address.origin == Origin::Stable && temporaries {
                flags.push("mngtmpaddr");
            }
            if matches!(address.origin, Origin::Stable | Origin::LinkLocal) {
                flags.push("stable-privacy");
            }
            for flag in flags {
                write!(out, "{flag} ")?;
            }
            writeln!(out)?;
            writeln!(
                out,
                "       valid_lft {} preferred_lft {}",
                lifetime(address.valid),
                lifetime(address.preferred)
            )?;
        }
    }
    Ok(())
}

/// An interface's IPv6 addresses in iproute2's order: those it was given
/// or made to send from, temporary ones first, then its link-local one.
fn shown6(interface: &Interface) -> Vec<Address6> {
    let mut addresses = interface.ipv6.addresses.clone();
    addresses.sort_by_key(|address| match address.origin {
        Origin::Temporary => 0,
        Origin::Stable => 1,
        Origin::Given => 2,
        Origin::LinkLocal => 3,
    });
    addresses
}

/// An IPv6 address's scope, as iproute2 names it.
fn scope6(ip: Ipv6Addr) -> &'static str {
    if ip.is_loopback() {
        "host"
    } else if link_local(ip) {
        "link"
    } else if ip.segments()[0] & 0xffc0 == 0xfec0 {
        "site"
    } else {
        "global"
    }
}

/// A lifetime, as iproute2 shows it: `forever`, or `86400sec`.
fn lifetime(seconds: Option<u64>) -> String {
    seconds.map_or_else(|| "forever".to_string(), |seconds| format!("{seconds}sec"))
}

/// `text` painted, then padded to `width` columns as `{:<width}` would.
fn padded(command: &Command, pick: fn(&Colors) -> &str, text: &str, width: usize) -> String {
    let pad = width.saturating_sub(text.chars().count());
    format!("{}{}", paint(command, pick, text), " ".repeat(pad))
}

/// The state, painted and padded to 14 columns, as `-brief` has it.
fn padded_state(command: &Command, interface: &Interface) -> String {
    let state = state(interface);
    let pad = 14usize.saturating_sub(state.chars().count());
    format!("{}{}", painted_state(command, interface), " ".repeat(pad))
}

/// `ip route`: the default routes, then each address's network, as
/// Linux's main table lists them (`lo`'s are in its local table, which
/// ip(8) does not show without `table local`).
fn route(interfaces: &[&Interface], out: &mut dyn Write) -> Result<(), Fail> {
    for interface in interfaces.iter().filter(|interface| !interface.loopback) {
        if let Some(router) = interface.router {
            writeln!(out, "default via {router} dev {} ", interface.name)?;
        }
    }
    for interface in interfaces.iter().filter(|interface| !interface.loopback) {
        let mut networks: Vec<Cidr> = Vec::new();
        for cidr in interface.addresses.iter().filter(|cidr| cidr.prefix < 32) {
            let network = Cidr { ip: cidr.network(), prefix: cidr.prefix };
            if networks.contains(&network) {
                continue;
            }
            networks.push(network);
            writeln!(
                out,
                "{network} dev {} proto kernel scope link src {} ",
                interface.name, cidr.ip
            )?;
        }
    }
    Ok(())
}

/// `ip -6 route`: `lo`'s address, each card's networks, its link-local
/// one, then its default route, as Linux lists its main table.
fn route6(interfaces: &[&Interface], out: &mut dyn Write) -> Result<(), Fail> {
    for interface in interfaces {
        let name = &interface.name;
        if interface.loopback {
            for address in &interface.ipv6.addresses {
                writeln!(out, "{} dev {name} proto kernel metric 256 pref medium", address.cidr.ip)?;
            }
            continue;
        }
        let mut networks: Vec<(Cidr6, Option<u64>)> = Vec::new();
        let mut link = false;
        for address in interface.ipv6.addresses.iter().filter(|a| !a.tentative) {
            if link_local(address.cidr.ip) {
                link = true;
                continue;
            }
            if address.cidr.prefix == 128 {
                continue;
            }
            let network = Cidr6 { ip: address.cidr.network(), prefix: address.cidr.prefix };
            match networks.iter_mut().find(|(known, _)| *known == network) {
                // The route lasts as long as its longest-lived address.
                Some((_, expires)) => {
                    *expires = expires.zip(address.valid).map(|(a, b)| a.max(b));
                }
                None => networks.push((network, address.valid)),
            }
        }
        for (network, expires) in networks {
            let expires = expires.map(|seconds| format!(" expires {seconds}sec")).unwrap_or_default();
            writeln!(out, "{network} dev {name} proto kernel metric 256{expires} pref medium")?;
        }
        if link {
            writeln!(out, "fe80::/64 dev {name} proto kernel metric 256 pref medium")?;
        }
        if let Some((router, expires)) = interface.ipv6.routers.first() {
            writeln!(
                out,
                "default via {router} dev {name} proto ra metric 1024 expires {expires}sec pref medium"
            )?;
        } else if let Some(router) = interface.ipv6.given_router {
            writeln!(out, "default via {router} dev {name} metric 1024 pref medium")?;
        }
    }
    Ok(())
}

/// `ip link set [dev] NAME up`: the interfaces are always up, and taking
/// one down is not there yet.
fn set_link(interfaces: &[Interface], words: &[&str]) -> Result<i32, Fail> {
    let (name, rest) = match words {
        ["dev", name, rest @ ..] | [name, rest @ ..] => (*name, rest),
        [] => return Err(usage("Not enough information: \"dev\" argument is required.".into())),
    };
    named(interfaces, name)?;
    match rest {
        ["up"] => Ok(0),
        ["down"] => Err(answers("Operation not supported (interfaces are always up)")),
        _ => Err(usage("ip link set takes only \"up\" here; see ip(8).".into())),
    }
}

/// The value after each of `keys` in `words`, which are key-value pairs,
/// and the first word left that is no key, as iproute2's `local` is.
fn pairs<'a>(
    words: &[&'a str],
    keys: &[&str],
) -> Result<(Vec<Option<&'a str>>, Option<&'a str>), Fail> {
    let mut values = vec![None; keys.len()];
    let mut plain = None;
    let mut at = 0;
    while at < words.len() {
        let word = words[at];
        if let Some(key) = keys.iter().position(|key| *key == word) {
            let value = words
                .get(at + 1)
                .ok_or_else(|| usage("Command line is not complete. Try option \"help\"".into()))?;
            if values[key].replace(*value).is_some() {
                return Err(usage(format!(
                    "Error: either \"{word}\" is duplicate, or \"{value}\" is a garbage."
                )));
            }
            at += 2;
        } else if plain.is_none() {
            plain = Some(word);
            at += 1;
        } else {
            return Err(usage(format!(
                "Error: either \"local\" is duplicate, or \"{word}\" is a garbage."
            )));
        }
    }
    Ok((values, plain))
}

/// The router `interface` keeps with `addresses`: its own if one of
/// them still reaches it, as Linux drops the routes whose gateway no
/// address reaches.
fn kept_router(interface: &Interface, addresses: &[Cidr]) -> Option<Ipv4Addr> {
    interface.router.filter(|router| addresses.iter().any(|cidr| cidr.contains(*router)))
}

/// `ip address add`, `del` and `flush`: of IPv4 addresses, or of IPv6
/// ones with `-6` or for an address that is one, as iproute2 tells.
fn change_address(
    family: Option<Family>,
    verb: &str,
    interfaces: &[Interface],
    words: &[&str],
    network: &dyn Network,
) -> Result<i32, Fail> {
    let (values, plain) = pairs(words, &["dev", "local", "brd", "broadcast"])?;
    let [dev, local, brd, broadcast] = values[..] else { unreachable!() };
    let dev =
        dev.ok_or_else(|| usage("Not enough information: \"dev\" argument is required.".into()))?;
    let interface = named(interfaces, dev)?;
    if interface.loopback {
        return Err(answers("Operation not supported (lo's addresses are its own)"));
    }
    let text = local.or(plain);
    let v6 = family == Some(Family::V6) || text.is_some_and(|text| text.contains(':'));
    if matches(verb, "flush", 1) {
        if text.is_some() {
            return Err(usage("ip address flush takes only \"dev\" here; see ip(8).".into()));
        }
        // Without -4 or -6, both, as iproute2 flushes.
        if family != Some(Family::V4) && !interface.ipv6.given().is_empty() {
            let router = interface.ipv6.given_router.filter(|router| link_local(*router));
            network.configure_ipv6(interface.index, None, &[], router).map_err(refused)?;
        }
        if family == Some(Family::V6) {
            return Ok(0);
        }
        network.configure(interface.index, &[], None).map_err(refused)?;
        return Ok(0);
    }
    let text =
        text.ok_or_else(|| usage("Not enough information: an address is required.".into()))?;
    if v6 {
        return change_address6(verb, interface, text, network);
    }
    let mut addresses = interface.addresses.clone();
    {
        let cidr = Cidr::parse(text).ok_or_else(|| {
            usage(format!("Error: inet prefix is expected rather than \"{text}\"."))
        })?;
        if let Some(given) = brd.or(broadcast)
            && given != "+"
            && given.parse().ok() != cidr.broadcast()
        {
            return Err(answers("Invalid argument (the broadcast address is the network's own)"));
        }
        if matches(verb, "add", 1) {
            if addresses.iter().any(|other| other.ip == cidr.ip) {
                return Err(answers("File exists"));
            }
            if addresses.len() == ADDRESSES_MAX {
                return Err(answers(&format!(
                    "No space left on device (at most {ADDRESSES_MAX} addresses)"
                )));
            }
            addresses.push(cidr);
        } else {
            // Without a prefix, the address whatever its prefix, as
            // iproute2 still deletes one.
            let alone = !text.contains('/');
            let Some(at) = addresses
                .iter()
                .position(|other| other.ip == cidr.ip && (alone || other.prefix == cidr.prefix))
            else {
                return Err(answers("Cannot assign requested address"));
            };
            addresses.remove(at);
        }
    }
    let router = kept_router(interface, &addresses);
    network.configure(interface.index, &addresses, router).map_err(refused)?;
    Ok(0)
}

/// `ip address add` and `del` of the IPv6 address `text`: of those the
/// interface was given.
fn change_address6(
    verb: &str,
    interface: &Interface,
    text: &str,
    network: &dyn Network,
) -> Result<i32, Fail> {
    let cidr = Cidr6::parse(text)
        .ok_or_else(|| usage(format!("Error: inet6 prefix is expected rather than \"{text}\".")))?;
    if link_local(cidr.ip) {
        return Err(answers("Operation not supported (the link-local address is the interface's own)"));
    }
    let mut given = interface.ipv6.given();
    if matches(verb, "add", 1) {
        if interface.ipv6.addresses.iter().any(|other| other.cidr.ip == cidr.ip) {
            return Err(answers("File exists"));
        }
        if given.len() == ADDRESSES_IPV6_MAX {
            return Err(answers(&format!(
                "No space left on device (at most {ADDRESSES_IPV6_MAX} IPv6 addresses)"
            )));
        }
        given.push(cidr);
    } else {
        let alone = !text.contains('/');
        let found = |other: &Cidr6| other.ip == cidr.ip && (alone || other.prefix == cidr.prefix);
        let Some(at) = given.iter().position(found) else {
            if interface.ipv6.addresses.iter().any(|other| found(&other.cidr)) {
                return Err(answers(
                    "Operation not supported (made from router adverts; netd(8) sets how)",
                ));
            }
            return Err(answers("Cannot assign requested address"));
        };
        given.remove(at);
    }
    // A router the addresses left no longer reach goes with them, unless
    // it is link-local, which every interface with IPv6 reaches.
    let router = interface.ipv6.given_router.filter(|router| {
        link_local(*router) || given.iter().any(|cidr| cidr.contains(*router))
    });
    network.configure_ipv6(interface.index, None, &given, router).map_err(refused)?;
    Ok(0)
}

/// `ip route add`, `replace` and `del`, of the default route only: the
/// networks' routes are their addresses'.
fn change_route(
    family: Option<Family>,
    verb: &str,
    interfaces: &[Interface],
    words: &[&str],
    network: &dyn Network,
) -> Result<i32, Fail> {
    let (values, plain) = pairs(words, &["via", "dev"])?;
    let [via, dev] = values[..] else { unreachable!() };
    if family == Some(Family::V6) || via.is_some_and(|via| via.contains(':')) || plain == Some("::/0")
    {
        return change_route6(verb, interfaces, plain, via, dev, network);
    }
    match plain {
        Some("default" | "0.0.0.0/0" | "0/0") => {}
        Some(other) => {
            return Err(answers(&format!(
                "Operation not supported (only the default route, not {other}, is set this way)"
            )));
        }
        None => {
            return Err(usage(
                "Not enough information: the route's destination is required.".into(),
            ));
        }
    }
    let via = via
        .map(|text| {
            text.parse::<Ipv4Addr>().map_err(|_| {
                usage(format!("Error: inet address is expected rather than \"{text}\"."))
            })
        })
        .transpose()?;
    let cards = || interfaces.iter().filter(|interface| !interface.loopback);
    let interface = match dev {
        Some(name) => named(interfaces, name)?,
        None if matches(verb, "delete", 1) => {
            let mut routed = cards()
                .filter(|card| card.router.is_some() && (via.is_none() || card.router == via));
            routed.next().ok_or_else(|| answers("No such process"))?
        }
        None => {
            let via = via.ok_or_else(|| {
                usage("Not enough information: \"via\" argument is required.".into())
            })?;
            cards()
                .find(|card| card.addresses.iter().any(|cidr| cidr.contains(via)))
                .ok_or_else(|| answers("Network is unreachable"))?
        }
    };
    if interface.loopback {
        return Err(answers("Operation not supported (lo has no router)"));
    }
    let router = if matches(verb, "delete", 1) {
        if interface.router.is_none() || (via.is_some() && interface.router != via) {
            return Err(answers("No such process"));
        }
        None
    } else {
        let via = via
            .ok_or_else(|| usage("Not enough information: \"via\" argument is required.".into()))?;
        if !interface.addresses.iter().any(|cidr| cidr.contains(via)) {
            return Err(fail_with(2, "Error: Nexthop has invalid gateway.".into()));
        }
        if matches(verb, "add", 1) && interface.router.is_some() {
            return Err(answers("File exists"));
        }
        Some(via)
    };
    network.configure(interface.index, &interface.addresses, router).map_err(refused)?;
    Ok(0)
}

/// `ip -6 route add`, `replace` and `del` of the default route: the one
/// the interface was given, used while no router advertises itself.
fn change_route6(
    verb: &str,
    interfaces: &[Interface],
    plain: Option<&str>,
    via: Option<&str>,
    dev: Option<&str>,
    network: &dyn Network,
) -> Result<i32, Fail> {
    match plain {
        Some("default" | "::/0") => {}
        Some(other) => {
            return Err(answers(&format!(
                "Operation not supported (only the default route, not {other}, is set this way)"
            )));
        }
        None => {
            return Err(usage(
                "Not enough information: the route's destination is required.".into(),
            ));
        }
    }
    let via = via
        .map(|text| {
            text.parse::<Ipv6Addr>().map_err(|_| {
                usage(format!("Error: inet6 address is expected rather than \"{text}\"."))
            })
        })
        .transpose()?;
    let cards = || interfaces.iter().filter(|interface| !interface.loopback);
    let interface = match dev {
        Some(name) => named(interfaces, name)?,
        None if matches(verb, "delete", 1) => cards()
            .find(|card| {
                card.ipv6.given_router.is_some()
                    && (via.is_none() || card.ipv6.given_router == via)
            })
            .ok_or_else(|| answers("No such process"))?,
        None => {
            let via = via.ok_or_else(|| {
                usage("Not enough information: \"via\" argument is required.".into())
            })?;
            if link_local(via) {
                return Err(usage(
                    "Not enough information: a link-local router needs \"dev\".".into(),
                ));
            }
            cards()
                .find(|card| card.ipv6.addresses.iter().any(|a| a.cidr.contains(via)))
                .ok_or_else(|| answers("Network is unreachable"))?
        }
    };
    if interface.loopback {
        return Err(answers("Operation not supported (lo has no router)"));
    }
    let given = interface.ipv6.given();
    let router = if matches(verb, "delete", 1) {
        let had = interface.ipv6.given_router;
        if had.is_none() || (via.is_some() && had != via) {
            return Err(answers("No such process"));
        }
        None
    } else {
        let via = via
            .ok_or_else(|| usage("Not enough information: \"via\" argument is required.".into()))?;
        if !link_local(via) && !interface.ipv6.addresses.iter().any(|a| a.cidr.contains(via)) {
            return Err(fail_with(2, "Error: Nexthop has invalid gateway.".into()));
        }
        if matches(verb, "add", 1) && interface.ipv6.given_router.is_some() {
            return Err(answers("File exists"));
        }
        Some(via)
    };
    network.configure_ipv6(interface.index, None, &given, router).map_err(refused)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// `lo` and a card, as the kernel shows them, with the card's router.
    fn sysfs() -> (PathBuf, PathBuf) {
        // A directory for each call: the tests run at once.
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let call = CALLS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ip-test-{}-{call}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let write = |name: &str, files: &[(&str, &str)]| {
            let path = dir.join("net").join(name);
            fs::create_dir_all(path.join("statistics")).unwrap();
            for (file, value) in files {
                fs::write(path.join(file), format!("{value}\n")).unwrap();
            }
        };
        write(
            "lo",
            &[
                ("ifindex", "1"),
                ("address", "00:00:00:00:00:00"),
                ("mtu", "65535"),
                ("type", "772"),
                ("operstate", "unknown"),
                ("carrier", "1"),
                ("ipv4", "127.0.0.1/8"),
                ("ipv6", "address ::1/128 given preferred forever valid forever"),
                ("statistics/rx_bytes", "84"),
                ("statistics/rx_packets", "1"),
            ],
        );
        write(
            "net0",
            &[
                ("ifindex", "2"),
                ("address", "52:54:00:12:34:56"),
                ("mtu", "1500"),
                ("type", "1"),
                ("operstate", "up"),
                ("carrier", "1"),
                ("ipv4", "10.0.2.15/24\n10.0.2.16/24\n192.168.7.1/32"),
                (
                    "ipv6",
                    "address fe80::8c1:5bff:fe2e:a7d3/64 link-local preferred forever valid forever\n\
                     address fec0::d1f:8e01:6a3c:52b0/64 stable preferred 14390 valid 86390\n\
                     address fec0::71a0:1bc2:e9d4:3f11/64 temporary preferred 3600 valid 86000\n\
                     address 2001:db8::5/64 given preferred forever valid forever tentative\n\
                     router fe80::2 lifetime 1790\n\
                     dns fec0::3 lifetime 1790\n\
                     mtu 1500\n\
                     dhcp other",
                ),
                ("statistics/tx_bytes", "4200"),
                ("statistics/tx_packets", "3"),
            ],
        );
        let routes = dir.join("route");
        fs::write(
            &routes,
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
             net0\t0002000A\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
             net0\t00000000\t0202000A\t0003\t0\t0\t0\t00000000\t0\t0\t0\n",
        )
        .unwrap();
        (dir.join("net"), routes)
    }

    fn interfaces() -> Vec<Interface> {
        let (dir, routes) = sysfs();
        read_interfaces(&dir, &routes).unwrap()
    }

    /// One `configure` call: the interface, its addresses and its router.
    type Asked = (u32, Vec<Cidr>, Option<Ipv4Addr>);
    /// One `configure_ipv6` call, given only.
    type Asked6 = (u32, Vec<Cidr6>, Option<Ipv6Addr>);

    /// What ip(8) asks the kernel, kept instead.
    #[derive(Default)]
    struct Kept(RefCell<Vec<Asked>>, RefCell<Vec<Asked6>>);

    impl Network for Kept {
        fn configure(
            &self,
            index: u32,
            addresses: &[Cidr],
            router: Option<Ipv4Addr>,
        ) -> Result<(), Status> {
            self.0.borrow_mut().push((index, addresses.to_vec(), router));
            Ok(())
        }

        fn configure_ipv6(
            &self,
            index: u32,
            settings: Option<&Ipv6Settings>,
            addresses: &[Cidr6],
            router: Option<Ipv6Addr>,
        ) -> Result<(), Status> {
            assert!(settings.is_none(), "ip(8) changes only what it gives");
            self.1.borrow_mut().push((index, addresses.to_vec(), router));
            Ok(())
        }
    }

    fn command(words: &str) -> Command {
        let mut words: Vec<String> = words.split_whitespace().map(str::to_string).collect();
        let family = match words.first().map(String::as_str) {
            Some("-4") => Some(Family::V4),
            Some("-6") => Some(Family::V6),
            _ => None,
        };
        if family.is_some() {
            words.remove(0);
        }
        Command {
            family,
            brief: false,
            statistics: false,
            colors: None,
            words,
        }
    }

    /// What `ip <words>` prints, or how it fails, and what it asked.
    fn ip(words: &str) -> (Result<String, Fail>, Vec<Asked>) {
        let (result, asked, _) = ip6(words);
        (result, asked)
    }

    /// The same, with the IPv6 changes it asked for.
    fn ip6(words: &str) -> (Result<String, Fail>, Vec<Asked>, Vec<Asked6>) {
        let kept = Kept::default();
        let mut out = Vec::new();
        let result = run(&command(words), &interfaces(), &kept, &mut out)
            .map(|_| String::from_utf8(out).unwrap());
        (result, kept.0.into_inner(), kept.1.into_inner())
    }

    fn cidr6(text: &str) -> Cidr6 {
        Cidr6::parse(text).unwrap()
    }

    fn cidr(text: &str) -> Cidr {
        Cidr::parse(text).unwrap()
    }

    #[test]
    fn interfaces_are_read_with_their_routers() {
        let interfaces = interfaces();
        assert_eq!(interfaces.len(), 2);
        let (lo, card) = (&interfaces[0], &interfaces[1]);
        assert_eq!((lo.name.as_str(), lo.index, lo.loopback, lo.router), ("lo", 1, true, None));
        assert_eq!(lo.received[..2], [84, 1]);
        assert_eq!((card.name.as_str(), card.index, card.mtu), ("net0", 2, 1500));
        assert_eq!(
            card.addresses,
            [cidr("10.0.2.15/24"), cidr("10.0.2.16/24"), cidr("192.168.7.1")]
        );
        assert_eq!(card.router, Some(Ipv4Addr::new(10, 0, 2, 2)));
        assert_eq!(card.sent[..2], [4200, 3]);
    }

    #[test]
    fn addresses_are_shown_as_iproute2_shows_them() {
        let (out, asked) = ip("-4 address");
        assert!(asked.is_empty());
        assert_eq!(
            out.unwrap(),
            "1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65535 qdisc noqueue state UNKNOWN group default\n\
             \x20   link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00\n\
             \x20   inet 127.0.0.1/8 scope host lo\n\
             \x20      valid_lft forever preferred_lft forever\n\
             2: net0: <BROADCAST,UP,LOWER_UP> mtu 1500 qdisc noqueue state UP group default\n\
             \x20   link/ether 52:54:00:12:34:56 brd ff:ff:ff:ff:ff:ff\n\
             \x20   inet 10.0.2.15/24 brd 10.0.2.255 scope global net0\n\
             \x20      valid_lft forever preferred_lft forever\n\
             \x20   inet 10.0.2.16/24 brd 10.0.2.255 scope global secondary net0\n\
             \x20      valid_lft forever preferred_lft forever\n\
             \x20   inet 192.168.7.1/32 scope global net0\n\
             \x20      valid_lft forever preferred_lft forever\n"
        );
        let (out, _) = ip("a s dev lo");
        assert!(out.unwrap().starts_with("1: lo: "));
        let (out, _) = ip("addr show eth0");
        assert_eq!(out.unwrap_err().message, "Device \"eth0\" does not exist.");
    }

    #[test]
    fn ipv6_addresses_are_shown_as_iproute2_shows_them() {
        let (out, _) = ip("-6 address");
        assert_eq!(
            out.unwrap(),
            "1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65535 qdisc noqueue state UNKNOWN group default\n\
             \x20   link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00\n\
             \x20   inet6 ::1/128 scope host \n\
             \x20      valid_lft forever preferred_lft forever\n\
             2: net0: <BROADCAST,UP,LOWER_UP> mtu 1500 qdisc noqueue state UP group default\n\
             \x20   link/ether 52:54:00:12:34:56 brd ff:ff:ff:ff:ff:ff\n\
             \x20   inet6 fec0::71a0:1bc2:e9d4:3f11/64 scope site temporary dynamic \n\
             \x20      valid_lft 86000sec preferred_lft 3600sec\n\
             \x20   inet6 fec0::d1f:8e01:6a3c:52b0/64 scope site dynamic mngtmpaddr stable-privacy \n\
             \x20      valid_lft 86390sec preferred_lft 14390sec\n\
             \x20   inet6 2001:db8::5/64 scope global tentative \n\
             \x20      valid_lft forever preferred_lft forever\n\
             \x20   inet6 fe80::8c1:5bff:fe2e:a7d3/64 scope link stable-privacy \n\
             \x20      valid_lft forever preferred_lft forever\n"
        );
        // Without -6, IPv4 then IPv6.
        let (out, _) = ip("address show dev net0");
        let out = out.unwrap();
        let (inet, inet6) = (out.find("    inet ").unwrap(), out.find("    inet6 ").unwrap());
        assert!(inet < inet6, "{out}");
        let mut brief = command("-6 address show dev net0");
        brief.brief = true;
        let mut out = Vec::new();
        run(&brief, &interfaces(), &Kept::default(), &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "net0             UP             fec0::71a0:1bc2:e9d4:3f11/64 \
             fec0::d1f:8e01:6a3c:52b0/64 2001:db8::5/64 fe80::8c1:5bff:fe2e:a7d3/64 \n"
        );
        let card = &interfaces()[1].ipv6;
        assert_eq!(card.dns, [("fec0::3".parse().unwrap(), 1790)]);
        assert_eq!((card.mtu, card.managed, card.other), (Some(1500), false, true));
        assert_eq!(card.router(), Some("fe80::2".parse().unwrap()));
    }

    #[test]
    fn ipv6_routes_are_the_networks_then_the_default() {
        let (out, _) = ip("-6 route");
        assert_eq!(
            out.unwrap(),
            "::1 dev lo proto kernel metric 256 pref medium\n\
             fec0::/64 dev net0 proto kernel metric 256 expires 86390sec pref medium\n\
             fe80::/64 dev net0 proto kernel metric 256 pref medium\n\
             default via fe80::2 dev net0 proto ra metric 1024 expires 1790sec pref medium\n"
        );
    }

    #[test]
    fn ipv6_addresses_and_routes_change_only_those_given() {
        let (out, asked, asked6) = ip6("address add 2001:db8:1::9/48 dev net0");
        assert_eq!((out.unwrap(), asked.len()), (String::new(), 0));
        assert_eq!(asked6, [(2, vec![cidr6("2001:db8::5/64"), cidr6("2001:db8:1::9/48")], None)]);
        let (out, ..) = ip6("-6 address add fec0::d1f:8e01:6a3c:52b0/64 dev net0");
        assert_eq!(out.unwrap_err().message, "RTNETLINK answers: File exists");
        let (_, _, asked6) = ip6("address del 2001:db8::5 dev net0");
        assert_eq!(asked6, [(2, Vec::new(), None)]);
        let (out, ..) = ip6("address del fec0::d1f:8e01:6a3c:52b0/64 dev net0");
        assert!(out.unwrap_err().message.contains("router adverts"));
        let (out, ..) = ip6("address add fe80::1/64 dev net0");
        assert_eq!(out.unwrap_err().code, 2);
        // flush without -4 or -6: both.
        let (_, asked, asked6) = ip6("address flush dev net0");
        assert_eq!((asked.len(), asked6.len()), (1, 1));
        let (_, asked, asked6) = ip6("-6 address flush dev net0");
        assert_eq!((asked.len(), asked6), (0, vec![(2, Vec::new(), None)]));
        // The default route.
        let (_, _, asked6) = ip6("-6 route add default via fe80::1 dev net0");
        assert_eq!(
            asked6,
            [(2, vec![cidr6("2001:db8::5/64")], Some("fe80::1".parse().unwrap()))]
        );
        let (out, ..) = ip6("-6 route add default via fe80::1");
        assert_eq!(out.unwrap_err().code, 1);
        let (_, _, asked6) = ip6("route replace default via 2001:db8::1");
        assert_eq!(asked6[0].2, Some("2001:db8::1".parse().unwrap()));
        let (out, ..) = ip6("-6 route del default");
        assert_eq!(out.unwrap_err().message, "RTNETLINK answers: No such process");
        let (out, ..) = ip6("-6 route add 2001:db8:2::/48 via fe80::1 dev net0");
        assert_eq!(out.unwrap_err().code, 2);
    }

    #[test]
    fn links_are_shown_in_full_and_briefly() {
        let (out, _) = ip("link show net0");
        assert_eq!(
            out.unwrap(),
            "2: net0: <BROADCAST,UP,LOWER_UP> mtu 1500 qdisc noqueue state UP mode DEFAULT group default\n\
             \x20   link/ether 52:54:00:12:34:56 brd ff:ff:ff:ff:ff:ff\n"
        );
        let mut brief = command("link");
        brief.brief = true;
        let mut out = Vec::new();
        run(&brief, &interfaces(), &Kept::default(), &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "lo               UNKNOWN        00:00:00:00:00:00 <LOOPBACK,UP,LOWER_UP> \n\
             net0             UP             52:54:00:12:34:56 <BROADCAST,UP,LOWER_UP> \n"
        );
        let mut statistics = command("l");
        statistics.statistics = true;
        let mut out = Vec::new();
        run(&statistics, &interfaces(), &Kept::default(), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        let received = concat!(
            "    RX:  bytes packets errors dropped  missed   mcast\n",
            "            84       1      0       0       0       0\n",
        );
        assert!(out.contains(received), "{out}");
    }

    #[test]
    fn routes_are_the_default_then_the_networks() {
        let (out, _) = ip("route");
        assert_eq!(
            out.unwrap(),
            "default via 10.0.2.2 dev net0 \n\
             10.0.2.0/24 dev net0 proto kernel scope link src 10.0.2.15 \n"
        );
    }

    #[test]
    fn addresses_are_added_and_deleted() {
        let (out, asked) = ip("address add 192.168.1.5/16 dev net0");
        assert_eq!(out.unwrap(), "");
        let mut after = interfaces()[1].addresses.clone();
        after.push(cidr("192.168.1.5/16"));
        assert_eq!(asked, [(2, after, Some(Ipv4Addr::new(10, 0, 2, 2)))]);
        // One it has is there already, and a fifth is one too many.
        let (out, asked) = ip("addr add 10.0.2.15/24 dev net0");
        assert_eq!((out.unwrap_err().code, asked.len()), (2, 0));
        let (out, asked) = ip("a a 10.0.2.20 dev net0");
        assert_eq!(asked[0].1.last(), Some(&cidr("10.0.2.20/32")));
        assert!(out.is_ok());
        let mut full = interfaces();
        full[1].addresses.push(cidr("10.0.2.20"));
        let kept = Kept::default();
        let words = ["10.0.2.21/24", "dev", "net0"];
        let out = change_address(None, "add", &full, &words, &kept);
        assert_eq!(
            out.unwrap_err().message,
            "RTNETLINK answers: No space left on device (at most 4 addresses)"
        );
        // Without a prefix, the address goes whatever its prefix.
        let (_, asked) = ip("address del 10.0.2.15 dev net0");
        assert_eq!(asked[0].1, [cidr("10.0.2.16/24"), cidr("192.168.7.1")]);
        let (out, _) = ip("address del 10.0.2.15/16 dev net0");
        assert_eq!(out.unwrap_err().message, "RTNETLINK answers: Cannot assign requested address");
        // The router goes with the last address that reached it.
        let (_, asked) = ip("address flush dev net0");
        assert_eq!(asked, [(2, Vec::new(), None)]);
        let (out, _) = ip("address add 127.0.0.2/8 dev lo");
        assert_eq!(out.unwrap_err().code, 2);
        let (out, _) = ip("address add 10.0.2.30/24");
        assert_eq!(out.unwrap_err().code, 1);
    }

    #[test]
    fn the_default_route_is_added_replaced_and_deleted() {
        let (out, _) = ip("route add default via 10.0.2.3");
        assert_eq!(out.unwrap_err().message, "RTNETLINK answers: File exists");
        let (_, asked) = ip("route replace default via 10.0.2.3");
        assert_eq!(asked[0].2, Some(Ipv4Addr::new(10, 0, 2, 3)));
        assert_eq!(asked[0].1, interfaces()[1].addresses);
        let (out, _) = ip("route replace default via 172.16.0.1");
        assert_eq!(out.unwrap_err().message, "RTNETLINK answers: Network is unreachable");
        let (out, _) = ip("route replace default via 172.16.0.1 dev net0");
        assert_eq!(out.unwrap_err().message, "Error: Nexthop has invalid gateway.");
        let (_, asked) = ip("route del default");
        assert_eq!(asked, [(2, interfaces()[1].addresses.clone(), None)]);
        let (out, _) = ip("route del default via 10.0.2.9");
        assert_eq!(out.unwrap_err().message, "RTNETLINK answers: No such process");
        let (out, _) = ip("route add 10.1.0.0/16 via 10.0.2.2");
        assert_eq!(out.unwrap_err().code, 2);
    }

    #[test]
    fn colors_come_from_ip_colors() {
        let colors = Colors::new(Some("name=1;36:up=92:bogus=1:down=x;y"));
        assert_eq!(colors.name, "1;36");
        assert_eq!(colors.up, "92");
        assert_eq!(colors.down, "31");
        let mut colored = command("address show dev net0");
        colored.colors = Some(Colors::new(None));
        let mut out = Vec::new();
        run(&colored, &interfaces(), &Kept::default(), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(": \x1b[36mnet0\x1b[0m: "), "{out}");
        assert!(out.contains("state \x1b[32mUP\x1b[0m"), "{out}");
        assert!(out.contains("inet \x1b[35m"), "{out}");
        // Without -color, none.
        let (plain, _) = ip("address show dev net0");
        assert!(!plain.unwrap().contains('\x1b'));
    }

    #[test]
    fn links_are_always_up() {
        assert_eq!(ip("link set dev lo up").0.unwrap(), "");
        assert_eq!(ip("link set net0 down").0.unwrap_err().code, 2);
    }

    #[test]
    fn ipv6_prefixes_reach_their_networks() {
        let card = cidr6("2001:db8::5/64");
        assert_eq!(card.network(), "2001:db8::".parse::<Ipv6Addr>().unwrap());
        assert!(card.contains("2001:db8::1".parse().unwrap()));
        assert!(!card.contains("2001:db9::1".parse().unwrap()));
        assert_eq!(cidr6("::/0").network(), Ipv6Addr::UNSPECIFIED);
        assert_eq!(Cidr6::parse("::1/129"), None);
        assert!(link_local("fe80::1".parse().unwrap()) && !link_local("fec0::1".parse().unwrap()));
    }

    #[test]
    fn prefixes_reach_their_networks() {
        let card = cidr("10.0.2.15/24");
        assert_eq!(card.network(), Ipv4Addr::new(10, 0, 2, 0));
        assert_eq!(card.broadcast(), Some(Ipv4Addr::new(10, 0, 2, 255)));
        assert!(card.contains(Ipv4Addr::new(10, 0, 2, 2)));
        assert!(!card.contains(Ipv4Addr::new(10, 0, 3, 2)));
        assert_eq!(cidr("0.0.0.0/0").mask(), 0);
        assert_eq!(cidr("10.0.0.1/31").broadcast(), None);
        assert_eq!(Cidr::parse("10.0.0.1/33"), None);
    }
}
