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
//! Errors are iproute2's: a mistake in the command line ends with status
//! 1, and what the kernel refuses with 2, said as `RTNETLINK answers:`
//! and the C library's text, which scripts look for.

use std::fs;
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::Path;

use radix_abi::Status;
use radix_abi::errors::{File as FileError, Kernel};
use radix_abi::network::{self, InterfaceAddress};
use radix_abi::startup::{NETWORK_SLOTS, SLOT_NETWORK};

use crate::args::Args;
use crate::sys::SYS;
use crate::util::{Fail, fail_with, usage};

/// Where the kernel shows the interfaces and the routes.
pub const INTERFACES: &str = "/kernel/class/net";
pub const ROUTES: &str = "/processes/net/route";

/// The most addresses an interface takes (radix-network(2)).
const ADDRESSES_MAX: usize = network::ADDRESSES_MAX as usize;

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
            received: ["rx_bytes", "rx_packets", "rx_errors", "rx_dropped"].map(statistic),
            sent: ["tx_bytes", "tx_packets", "tx_errors", "tx_dropped"].map(statistic),
            name,
        });
    }
    interfaces.sort_by_key(|interface| interface.index);
    Ok(interfaces)
}

/// How ip(8) changes an interface: radix-network(2)'s `configure`, so
/// that the tests can answer instead.
pub trait Network {
    /// Gives interface `index` `addresses`, the first the one it sends
    /// from, and `router`, in place of those it had.
    fn configure(
        &self,
        index: u32,
        addresses: &[Cidr],
        router: Option<Ipv4Addr>,
    ) -> Result<(), Status>;
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
        let mut refusal = Kernel::EmptySlot.status(0);
        for slot in SLOT_NETWORK..SLOT_NETWORK + NETWORK_SLOTS {
            match network::configure(&SYS, slot, index, &bytes, router) {
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

/// What ip(8) is asked: its options, and the words after them.
struct Command {
    brief: bool,
    statistics: bool,
    words: Vec<String>,
}

/// Whether `word` is one of `full`'s first letters, at least `least` of
/// them, as iproute2 matches its words.
fn matches(word: &str, full: &str, least: usize) -> bool {
    word.len() >= least && full.starts_with(word)
}

const HELP: &str = "Usage: ip [ OPTIONS ] OBJECT { COMMAND | help }
where  OBJECT := { link | address | route }
       OPTIONS := { -4 | -s[tatistics] | -br[ief] }
       ip link [ show [ [ dev ] NAME ] ]
       ip link set [ dev ] NAME up
       ip address [ show [ [ dev ] NAME ] ]
       ip address { add | del } IFADDR dev NAME
       ip address flush dev NAME
       ip route [ show [ dev NAME ] ]
       ip route { add | replace } default via ADDRESS [ dev NAME ]
       ip route del default [ via ADDRESS ] [ dev NAME ]
";

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut command = Command { brief: false, statistics: false, words: Vec::new() };
    let mut options = true;
    for word in args.rest() {
        let word = word.into_string().map_err(|_| usage("an argument is not UTF-8".into()))?;
        if options && let Some(option) = word.strip_prefix('-').map(|o| o.trim_start_matches('-')) {
            match option {
                "4" => {}
                "6" => return Err(usage("IPv6 comes with Phase 12c".into())),
                "h" | "help" => {
                    print!("{HELP}");
                    return Ok(0);
                }
                _ if matches(option, "brief", 2) => command.brief = true,
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
            return change_address(verb, interfaces, rest, network);
        }
    } else if matches(object, "route", 1) {
        if show {
            let shown = selected(interfaces, rest)?;
            return route(&shown, out).map(|()| 0);
        }
        if matches(verb, "add", 1) || matches(verb, "replace", 3) || matches(verb, "delete", 1) {
            return change_route(verb, interfaces, rest, network);
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
fn heading(interface: &Interface, mode: bool, out: &mut dyn Write) -> io::Result<()> {
    writeln!(
        out,
        "{}: {}: {} mtu {} qdisc noqueue state {}{} group default",
        interface.index,
        interface.name,
        flags(interface),
        interface.mtu,
        state(interface),
        if mode { " mode DEFAULT" } else { "" },
    )?;
    let (kind, broadcast) = if interface.loopback {
        ("loopback", "00:00:00:00:00:00")
    } else {
        ("ether", "ff:ff:ff:ff:ff:ff")
    };
    writeln!(out, "    link/{kind} {} brd {broadcast}", interface.mac)
}

/// `ip link`.
fn link(command: &Command, interfaces: &[&Interface], out: &mut dyn Write) -> Result<(), Fail> {
    for interface in interfaces {
        if command.brief {
            writeln!(
                out,
                "{:<16} {:<14} {} {} ",
                interface.name,
                state(interface),
                interface.mac,
                flags(interface)
            )?;
            continue;
        }
        heading(interface, true, out)?;
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
    for interface in interfaces {
        if command.brief {
            write!(out, "{:<16} {:<14} ", interface.name, state(interface))?;
            for cidr in &interface.addresses {
                write!(out, "{cidr} ")?;
            }
            writeln!(out)?;
            continue;
        }
        heading(interface, false, out)?;
        for (at, cidr) in interface.addresses.iter().enumerate() {
            let secondary = interface.addresses[..at]
                .iter()
                .any(|earlier| earlier.contains(cidr.ip) && earlier.prefix == cidr.prefix);
            let broadcast =
                cidr.broadcast().filter(|_| !interface.loopback).map(|b| format!(" brd {b}"));
            writeln!(
                out,
                "    inet {cidr}{} scope {}{} {}",
                broadcast.unwrap_or_default(),
                if interface.loopback { "host" } else { "global" },
                if secondary { " secondary" } else { "" },
                interface.name,
            )?;
            writeln!(out, "       valid_lft forever preferred_lft forever")?;
        }
    }
    Ok(())
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

/// `ip address add`, `del` and `flush`.
fn change_address(
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
    let mut addresses = interface.addresses.clone();
    if matches(verb, "flush", 1) {
        if local.is_some() || plain.is_some() {
            return Err(usage("ip address flush takes only \"dev\" here; see ip(8).".into()));
        }
        addresses.clear();
    } else {
        let text = local
            .or(plain)
            .ok_or_else(|| usage("Not enough information: an address is required.".into()))?;
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

/// `ip route add`, `replace` and `del`, of the default route only: the
/// networks' routes are their addresses'.
fn change_route(
    verb: &str,
    interfaces: &[Interface],
    words: &[&str],
    network: &dyn Network,
) -> Result<i32, Fail> {
    let (values, plain) = pairs(words, &["via", "dev"])?;
    let [via, dev] = values[..] else { unreachable!() };
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

    /// What ip(8) asks the kernel, kept instead.
    #[derive(Default)]
    struct Kept(RefCell<Vec<Asked>>);

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
    }

    fn command(words: &str) -> Command {
        Command {
            brief: false,
            statistics: false,
            words: words.split_whitespace().map(str::to_string).collect(),
        }
    }

    /// What `ip <words>` prints, or how it fails, and what it asked.
    fn ip(words: &str) -> (Result<String, Fail>, Vec<Asked>) {
        let kept = Kept::default();
        let mut out = Vec::new();
        let result = run(&command(words), &interfaces(), &kept, &mut out)
            .map(|_| String::from_utf8(out).unwrap());
        (result, kept.0.into_inner())
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
        let (out, asked) = ip("address");
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
        let out = change_address("add", &full, &words, &kept);
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
    fn links_are_always_up() {
        assert_eq!(ip("link set dev lo up").0.unwrap(), "");
        assert_eq!(ip("link set net0 down").0.unwrap_err().code, 2);
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
