// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! sockets: the test program of the `sockets` boot test, from
//! `/system/tests/sockets` in the boot archive (ADR 0183). Spark starts it
//! with `--network`, so its network capability, for all of the network's
//! ports, is in `SLOT_NETWORK` (radix-network(2)).
//!
//! - It derives two capabilities from it: one that may only connect out,
//!   and one that may only listen, on ten ports.
//! - TCP over the loopback interface: a socket listens, another connects
//!   through the one that may only connect out, the connection is taken,
//!   a line goes each way, a mebibyte goes one way as fast as the other
//!   end reads, each end's names are checked, and each end shuts down in
//!   turn; a socket that does not wait connects too.
//! - UDP over the loopback interface: datagrams go each way, keep their
//!   bounds and say where they came from; one too long for the interface
//!   is refused.
//! - Each method refuses what the capability it is given does not allow:
//!   ports outside its range, binding without the receive right,
//!   connecting or sending without the send right, listening on a port
//!   bound without the receive right; and ports in use, addresses the
//!   machine does not have, and connections no one takes.
//! - IPv6 over the loopback interface (ADR 0332): a socket on any address
//!   takes a connection to ::1 and one from an IPv4 socket, whose peer is
//!   IPv4-mapped; datagrams go between ::1's ports; a v6-only socket
//!   shares its port with an IPv4 one; and IPv4 addresses are refused.
//! - With `virtio` as its argument, TCP through the virtio network card:
//!   it connects to 10.0.2.100 port 7, where QEMU runs `cat`
//!   (`guestfwd`), and what it sends comes back.
//!
//! It prints a line per step, `sockets: FAILED: ...` for each check that
//! fails, and exits with 0 only if none did.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libradix::abi::capability::{self, Rights};
use libradix::abi::errors::Kernel;
use libradix::abi::file::{self, SocketFlags, SocketOption};
use libradix::abi::network::{self, Address, AddressFamily, EPHEMERAL_FIRST, SocketType};
use libradix::abi::process::PipeFlags;
use libradix::abi::startup::{SLOT_NETWORK, SLOT_ROOT, Startup};
use libradix::abi::{Status, process};
use radix_rt::{SYS, args, entry, println, slots};

entry!(main);

/// The loopback interface's address.
const LOOPBACK: [u8; 4] = [127, 0, 0, 1];
/// Its IPv6 address, ::1.
const LOOPBACK6: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
/// Any IPv6 address, ::.
const ANY6: [u8; 16] = [0; 16];
/// Where QEMU runs `cat` for the virtio test (`guestfwd`).
const ECHO: ([u8; 4], u16) = ([10, 0, 2, 100], 7);
/// The ports the listening capability covers.
const LISTEN_FIRST: u16 = 8000;
const LISTEN_COUNT: u16 = 10;
/// How many bytes go one way over the loopback interface, and in what
/// pieces they are sent and read.
const BULK: usize = 1 << 20;
const PIECE: usize = 16 * 1024;

/// The checks so far.
struct Checks {
    failed: u32,
}

impl Checks {
    /// Says that `what` failed unless `ok`.
    fn check(&mut self, ok: bool, what: &str) {
        if !ok {
            println!("sockets: FAILED: {what}");
            self.failed += 1;
        }
    }

    /// Checks that `result` is the error `want`, from `what`.
    fn refused<T>(&mut self, result: Result<T, Status>, want: Kernel, what: &str) {
        match result.map(drop) {
            Ok(()) => {
                self.check(false, &format!("{what} worked; it should fail with {}", want.name()));
            }
            Err(status) => {
                let got = Kernel::from_status(status);
                self.check(
                    got == Some(want),
                    &format!("{what} failed with {}, not {}", said(status), want.name()),
                );
            }
        }
    }

    /// The value of `result`, or `None` once it is counted as a failure of
    /// `what`.
    fn worked<T>(&mut self, result: Result<T, Status>, what: &str) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(status) => {
                self.check(false, &format!("{what} failed with {}", said(status)));
                None
            }
        }
    }
}

/// `status` by its name, if the kernel's.
fn said(status: Status) -> String {
    match Kernel::from_status(status) {
        Some(kind) => kind.name().into(),
        None => format!("{:#x}", status.0),
    }
}

/// An address, as radix-network(2)'s Address.
fn address(ip: [u8; 4], port: u16) -> [u8; Address::SIZE] {
    let mut bytes = [0; Address::SIZE];
    let family = AddressFamily::Ipv4.raw().to_le_bytes();
    bytes[Address::OFFSET_FAMILY..Address::OFFSET_FAMILY + 4].copy_from_slice(&family);
    bytes[Address::OFFSET_PORT..Address::OFFSET_PORT + 2].copy_from_slice(&port.to_le_bytes());
    bytes[Address::OFFSET_IP..Address::OFFSET_IP + 4].copy_from_slice(&ip);
    bytes
}

/// The IPv4 address and port `bytes`, an Address, hold.
fn parse(bytes: &[u8; Address::SIZE]) -> ([u8; 4], u16) {
    let at = Address::OFFSET_IP;
    let port = u16::from_le_bytes([bytes[Address::OFFSET_PORT], bytes[Address::OFFSET_PORT + 1]]);
    ([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]], port)
}

/// An IPv6 address, as radix-network(2)'s Address.
fn address6(ip: [u8; 16], port: u16) -> [u8; Address::SIZE] {
    let mut bytes = [0; Address::SIZE];
    let family = AddressFamily::Ipv6.raw().to_le_bytes();
    bytes[Address::OFFSET_FAMILY..Address::OFFSET_FAMILY + 4].copy_from_slice(&family);
    bytes[Address::OFFSET_PORT..Address::OFFSET_PORT + 2].copy_from_slice(&port.to_le_bytes());
    bytes[Address::OFFSET_IP..Address::OFFSET_IP + 16].copy_from_slice(&ip);
    bytes
}

/// The family, IPv6 address and port `bytes`, an Address, hold.
fn parse6(bytes: &[u8; Address::SIZE]) -> (u32, [u8; 16], u16) {
    let at = Address::OFFSET_IP;
    let word = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let port = u16::from_le_bytes([bytes[Address::OFFSET_PORT], bytes[Address::OFFSET_PORT + 1]]);
    let mut ip = [0; 16];
    ip.copy_from_slice(&bytes[at..at + 16]);
    (word(Address::OFFSET_FAMILY), ip, port)
}

/// `ip` as an IPv4-mapped IPv6 address, ::ffff:127.0.0.1.
fn mapped(ip: [u8; 4]) -> [u8; 16] {
    let mut v6 = [0; 16];
    v6[10] = 0xff;
    v6[11] = 0xff;
    v6[12..].copy_from_slice(&ip);
    v6
}

/// A socket, in a slot of its own, closed when it goes.
struct Socket(u32);

impl Socket {
    /// A new socket of `kind`, made through the network capability in
    /// `network`.
    fn new(network: u32, kind: SocketType, flags: PipeFlags) -> Result<Socket, Status> {
        Socket::of(AddressFamily::Ipv4, network, kind, flags)
    }

    /// A new IPv6 socket of `kind`, made through `network`.
    fn new6(network: u32, kind: SocketType) -> Result<Socket, Status> {
        Socket::of(AddressFamily::Ipv6, network, kind, PipeFlags::NONE)
    }

    fn of(
        family: AddressFamily,
        network: u32,
        kind: SocketType,
        flags: PipeFlags,
    ) -> Result<Socket, Status> {
        let slot = slots::take().ok_or(Status(0))?;
        match network::socket(&SYS, network, slot, family, kind, flags) {
            Ok(()) => Ok(Socket(slot)),
            Err(status) => {
                slots::give_back(slot);
                Err(status)
            }
        }
    }

    fn bind(&self, network: u32, ip: [u8; 4], port: u16) -> Result<(), Status> {
        file::bind_address(&SYS, self.0, network, &address(ip, port))
    }

    fn connect(&self, network: u32, ip: [u8; 4], port: u16) -> Result<(), Status> {
        file::connect_address(&SYS, self.0, network, &address(ip, port))
    }

    fn listen(&self) -> Result<(), Status> {
        file::listen(&SYS, self.0, 4)
    }

    fn accept(&self) -> Result<Socket, Status> {
        let slot = slots::take().ok_or(Status(0))?;
        match file::accept(&SYS, self.0, slot, PipeFlags::NONE) {
            Ok(()) => Ok(Socket(slot)),
            Err(status) => {
                slots::give_back(slot);
                Err(status)
            }
        }
    }

    fn send(&self, data: &[u8], flags: SocketFlags) -> Result<usize, Status> {
        file::send(&SYS, self.0, data, flags).map(|sent| sent.count as usize)
    }

    fn receive(&self, buffer: &mut [u8], flags: SocketFlags) -> Result<usize, Status> {
        file::receive(&SYS, self.0, buffer, flags).map(|got| got.count as usize)
    }

    /// Sends all of `data`.
    fn send_all(&self, data: &[u8]) -> Result<(), Status> {
        let mut sent = 0;
        while sent < data.len() {
            sent += self.send(&data[sent..], SocketFlags::NONE)?;
        }
        Ok(())
    }

    /// Receives `len` bytes, or what comes before the end.
    fn receive_len(&self, len: usize) -> Result<Vec<u8>, Status> {
        let mut bytes = alloc::vec![0; len];
        let mut got = 0;
        while got < len {
            let n = self.receive(&mut bytes[got..], SocketFlags::NONE)?;
            if n == 0 {
                break;
            }
            got += n;
        }
        bytes.truncate(got);
        Ok(bytes)
    }

    fn send_to(&self, network: u32, to: ([u8; 4], u16), data: &[u8]) -> Result<usize, Status> {
        let to = address(to.0, to.1);
        file::send_to(&SYS, self.0, network, &to, data, SocketFlags::NONE)
            .map(|sent| sent.count as usize)
    }

    fn receive_from(&self, buffer: &mut [u8]) -> Result<(usize, ([u8; 4], u16)), Status> {
        let mut from = [0; Address::SIZE];
        let got = file::receive_from(&SYS, self.0, buffer, &mut from, SocketFlags::NONE)?;
        Ok((got.count as usize, parse(&from)))
    }

    /// Where it is bound, or with `peer` where it is connected.
    fn name(&self, peer: bool) -> Result<([u8; 4], u16), Status> {
        let mut bytes = [0; Address::SIZE];
        file::socket_name(&SYS, self.0, peer, &mut bytes)?;
        Ok(parse(&bytes))
    }

    fn option(&self, option: SocketOption) -> Result<u64, Status> {
        file::socket_option(&SYS, self.0, option, false, 0).map(|got| got.value)
    }

    fn set_option(&self, option: SocketOption, value: u64) -> Result<u64, Status> {
        file::socket_option(&SYS, self.0, option, true, value).map(|got| got.value)
    }

    fn bind6(&self, network: u32, ip: [u8; 16], port: u16) -> Result<(), Status> {
        file::bind_address(&SYS, self.0, network, &address6(ip, port))
    }

    fn connect6(&self, network: u32, ip: [u8; 16], port: u16) -> Result<(), Status> {
        file::connect_address(&SYS, self.0, network, &address6(ip, port))
    }

    fn send_to6(&self, network: u32, ip: [u8; 16], port: u16, data: &[u8]) -> Result<usize, Status> {
        file::send_to(&SYS, self.0, network, &address6(ip, port), data, SocketFlags::NONE)
            .map(|sent| sent.count as usize)
    }

    fn receive_from6(&self, buffer: &mut [u8]) -> Result<(usize, (u32, [u8; 16], u16)), Status> {
        let mut from = [0; Address::SIZE];
        let got = file::receive_from(&SYS, self.0, buffer, &mut from, SocketFlags::NONE)?;
        Ok((got.count as usize, parse6(&from)))
    }

    /// Where an IPv6 socket is bound, or with `peer` where it is
    /// connected: its family, address and port.
    fn name6(&self, peer: bool) -> Result<(u32, [u8; 16], u16), Status> {
        let mut bytes = [0; Address::SIZE];
        file::socket_name(&SYS, self.0, peer, &mut bytes)?;
        Ok(parse6(&bytes))
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = capability::delete(&SYS, self.0);
        slots::give_back(self.0);
    }
}

/// The byte at `at` of the bulk transfer's.
fn pattern(at: usize) -> u8 {
    (at.wrapping_mul(31) ^ (at >> 11)) as u8
}

/// A capability derived from the network one, for `count` ports from
/// `first`, with `rights`, in a slot of its own.
fn restricted(checks: &mut Checks, first: u32, count: u32, rights: Rights) -> Option<u32> {
    let slot = slots::take()?;
    let made = network::restrict(&SYS, SLOT_NETWORK, slot, first, count, rights);
    checks.worked(made, "restricting the network capability")?;
    Some(slot)
}

fn main(_start: &Startup) -> u64 {
    let mut checks = Checks { failed: 0 };
    let virtio = args().any(|arg| arg == "virtio");
    match network::ports(&SYS, SLOT_NETWORK) {
        Ok(ports) => {
            println!(
                "sockets: the network capability covers {} ports from {}",
                ports.count, ports.first
            );
            checks
                .check((ports.first, ports.count) == (0, 65_536), "the network capability's ports");
        }
        Err(status) => {
            println!("sockets: FAILED: no network capability in SLOT_NETWORK: {}", said(status));
            return 1;
        }
    }
    let out = restricted(&mut checks, 0, 65_536, Rights::SEND);
    let listen = restricted(
        &mut checks,
        u32::from(LISTEN_FIRST),
        u32::from(LISTEN_COUNT),
        Rights::RECEIVE.union(Rights::COPY),
    );
    let (Some(out), Some(listen)) = (out, listen) else { return 1 };
    let ports = network::ports(&SYS, listen).map(|ports| (ports.first, ports.count));
    checks.check(ports == Ok((8000, 10)), "the listening capability's ports");
    println!("sockets: derived one capability to connect out and one to listen on ports 8000-8009");

    tcp(&mut checks, out);
    udp(&mut checks, out);
    rights(&mut checks, out, listen);
    ipv6(&mut checks, out);
    if virtio {
        through_virtio(&mut checks, out);
    }

    for slot in [out, listen] {
        let _ = capability::delete(&SYS, slot);
        slots::give_back(slot);
    }
    if checks.failed == 0 {
        println!("sockets: every check passed");
        0
    } else {
        println!("sockets: {} checks failed", checks.failed);
        1
    }
}

/// TCP over the loopback interface.
fn tcp(checks: &mut Checks, out: u32) {
    let made = Socket::new(SLOT_NETWORK, SocketType::Stream, PipeFlags::NONE);
    let Some(server) = checks.worked(made, "making a stream socket") else { return };
    let bound = server.bind(SLOT_NETWORK, LOOPBACK, 7000).and_then(|()| server.listen());
    if checks.worked(bound, "listening on 127.0.0.1 port 7000").is_none() {
        return;
    }
    let made = Socket::new(out, SocketType::Stream, PipeFlags::NONE);
    let Some(client) = checks.worked(made, "making a socket to connect") else { return };
    if checks.worked(client.connect(out, LOOPBACK, 7000), "connecting to port 7000").is_none() {
        return;
    }
    let Some(accepted) = checks.worked(server.accept(), "taking the connection") else { return };
    let (Ok(near), Ok(far)) = (client.name(false), client.name(true)) else {
        checks.check(false, "the connecting socket's names");
        return;
    };
    checks.check(
        near.0 == LOOPBACK && u64::from(near.1) >= EPHEMERAL_FIRST,
        &format!(
            "the connecting socket is bound to {near:?}, not a port of the kernel's on 127.0.0.1"
        ),
    );
    checks.check(far == (LOOPBACK, 7000), &format!("the connecting socket's peer is {far:?}"));
    checks.check(accepted.name(true) == Ok(near), "the taken connection's peer");
    checks.check(accepted.name(false) == Ok((LOOPBACK, 7000)), "the taken connection's name");
    let state = file::socket_state(&SYS, server.0).map(|state| (state.listening, state.connected));
    checks.check(state == Ok((true, false)), "the listening socket's state");
    println!(
        "sockets: tcp: connected from port {} to port 7000 over lo, and the connection taken",
        near.1
    );

    // A line each way.
    let line = b"hello over loopback";
    let sent = client.send_all(line).and_then(|()| accepted.receive_len(line.len()));
    checks.check(sent.as_deref() == Ok(&line[..]), "a line from the connecting end");
    let reply = b"and back again";
    let sent = accepted.send_all(reply).and_then(|()| client.receive_len(reply.len()));
    checks.check(sent.as_deref() == Ok(&reply[..]), "a line from the taking end");

    // A mebibyte one way: as much as there is room for is sent without
    // waiting, then what came is read, waiting for it.
    let (mut sent, mut got, mut wrong) = (0, 0, None);
    let mut piece = alloc::vec![0u8; PIECE];
    while got < BULK {
        if sent < BULK {
            let end = (sent + PIECE).min(BULK);
            for (at, byte) in (sent..end).zip(piece.iter_mut()) {
                *byte = pattern(at);
            }
            match client.send(&piece[..end - sent], SocketFlags::DONT_WAIT) {
                Ok(n) => sent += n,
                Err(status) if Kernel::from_status(status) == Some(Kernel::WouldBlock) => {}
                Err(status) => {
                    checks.check(false, &format!("sending the mebibyte: {}", said(status)));
                    return;
                }
            }
        }
        if got == sent {
            continue;
        }
        let mut buffer = [0u8; 4096];
        match accepted.receive(&mut buffer, SocketFlags::NONE) {
            Ok(0) => break,
            Ok(n) => {
                for (offset, &byte) in buffer[..n].iter().enumerate() {
                    if wrong.is_none() && byte != pattern(got + offset) {
                        wrong = Some(got + offset);
                    }
                }
                got += n;
            }
            Err(status) => {
                checks.check(false, &format!("receiving the mebibyte: {}", said(status)));
                return;
            }
        }
    }
    checks.check(got == BULK, &format!("{got} bytes of the mebibyte came"));
    checks.check(wrong.is_none(), &format!("the mebibyte's byte {wrong:?} came wrong"));
    println!("sockets: tcp: {got} bytes went one way and came in order");

    // Each end shuts down in turn: the other reads the end of what came,
    // and can still send.
    checks.worked(file::shutdown(&SYS, client.0, false, true), "shutting the connecting end");
    let mut buffer = [0u8; 16];
    checks.check(
        accepted.receive(&mut buffer, SocketFlags::NONE) == Ok(0),
        "the taking end reads the end after the other shuts",
    );
    let sent = accepted.send_all(b"bye").and_then(|()| client.receive_len(3));
    checks.check(sent.as_deref() == Ok(&b"bye"[..]), "a line after the other end shut");
    drop(accepted);
    checks.check(
        client.receive(&mut buffer, SocketFlags::NONE) == Ok(0),
        "the connecting end reads the end once the other closes",
    );
    println!("sockets: tcp: each end shut down in turn, and each read the end");

    // A socket that does not wait: it is connecting, then connected.
    let made = Socket::new(out, SocketType::Stream, PipeFlags::NON_BLOCKING);
    let Some(quick) = checks.worked(made, "making a socket that does not wait") else { return };
    checks.refused(
        quick.connect(out, LOOPBACK, 7000),
        Kernel::InProgress,
        "connecting without waiting",
    );
    let taken = checks.worked(server.accept(), "taking the second connection");
    checks.worked(quick.connect(out, LOOPBACK, 7000), "connecting again, once connected");
    checks.refused(
        quick.connect(out, LOOPBACK, 7000),
        Kernel::Connected,
        "connecting a third time",
    );
    checks.check(quick.option(SocketOption::Error) == Ok(0), "the connected socket's error");
    checks.check(
        quick.option(SocketOption::Kind) == Ok(u64::from(SocketType::Stream.raw())),
        "the socket's kind",
    );
    drop(taken);
    println!("sockets: tcp: a socket that does not wait connected, and said when it had");
}

/// UDP over the loopback interface.
fn udp(checks: &mut Checks, out: u32) {
    let made = Socket::new(SLOT_NETWORK, SocketType::Datagram, PipeFlags::NONE);
    let Some(bound) = checks.worked(made, "making a datagram socket") else { return };
    if checks.worked(bound.bind(SLOT_NETWORK, LOOPBACK, 7001), "binding to port 7001").is_none() {
        return;
    }
    let made = Socket::new(out, SocketType::Datagram, PipeFlags::NONE);
    let Some(sender) = checks.worked(made, "making a datagram socket to send") else { return };
    let mut buffer = [0u8; 64];
    let sent = sender.send_to(out, (LOOPBACK, 7001), b"datagram one");
    checks.check(sent == Ok(12), "sending a datagram to port 7001");
    let Some((n, from)) = checks.worked(bound.receive_from(&mut buffer), "receiving it") else {
        return;
    };
    checks.check(&buffer[..n] == b"datagram one", "the datagram that came");
    checks.check(
        from.0 == LOOPBACK && u64::from(from.1) >= EPHEMERAL_FIRST,
        &format!("the datagram came from {from:?}, not a port of the kernel's on 127.0.0.1"),
    );
    checks.check(sender.name(false).map(|name| name.1) == Ok(from.1), "the sender's port");
    let sent = bound.send_to(SLOT_NETWORK, from, b"datagram two");
    checks.check(sent == Ok(12), "sending a datagram back");
    let back = sender.receive_from(&mut buffer);
    checks.check(back == Ok((12, (LOOPBACK, 7001))), "the datagram back, and where it came from");
    checks.check(&buffer[..12] == b"datagram two", "the datagram back's bytes");
    // Each datagram keeps its bounds, and what does not fit is lost.
    for data in [&b"a"[..], b"bb", b"a datagram longer than its buffer"] {
        checks.worked(sender.send_to(out, (LOOPBACK, 7001), data), "sending datagrams");
    }
    let mut small = [0u8; 8];
    let got: Vec<_> = (0..3).map(|_| bound.receive_from(&mut small).map(|(n, _)| n)).collect();
    checks.check(got == [Ok(1), Ok(2), Ok(8)], &format!("the datagrams came as {got:?}"));
    // The longest datagram lo carries is Linux's, 65,507 bytes.
    let mut longest = alloc::vec![7u8; 65_508];
    checks.refused(
        sender.send_to(out, (LOOPBACK, 7001), &longest),
        Kernel::MessageTooLong,
        "sending a datagram longer than lo carries",
    );
    longest.pop();
    let sent = sender.send_to(out, (LOOPBACK, 7001), &longest);
    checks.check(sent == Ok(65_507), "sending the longest datagram lo carries");
    let mut whole = alloc::vec![0u8; 65_536];
    let got = bound.receive_from(&mut whole).map(|(n, _)| n);
    checks.check(got == Ok(65_507), &format!("the longest datagram came as {got:?}"));
    checks.check(whole[..65_507].iter().all(|&b| b == 7), "the longest datagram's bytes");
    println!(
        "sockets: udp: datagrams went each way over lo, kept their bounds, and said where they came from"
    );
}

/// What each method needs of the network capability it is given.
fn rights(checks: &mut Checks, out: u32, listen: u32) {
    let stream = |network| Socket::new(network, SocketType::Stream, PipeFlags::NONE);
    let (Ok(a), Ok(b), Ok(c), Ok(d)) =
        (stream(out), stream(listen), stream(SLOT_NETWORK), stream(out))
    else {
        checks.check(false, "making the sockets to refuse");
        return;
    };
    // Connecting out only: no port of its own, and no listening.
    checks.refused(
        a.bind(out, LOOPBACK, 7002),
        Kernel::MissingRights,
        "binding without the receive right",
    );
    checks.worked(a.bind(out, LOOPBACK, 0), "binding to a port the kernel picks");
    checks.refused(
        a.listen(),
        Kernel::MissingRights,
        "listening on a port bound without the receive right",
    );
    // Listening only: its ports, and no connecting.
    checks.refused(
        b.connect(listen, LOOPBACK, 7000),
        Kernel::MissingRights,
        "connecting without the send right",
    );
    checks.refused(
        b.bind(listen, LOOPBACK, 9000),
        Kernel::OutOfRange,
        "binding to a port outside the range",
    );
    checks.worked(b.bind(listen, LOOPBACK, 8001), "binding to a port in the range");
    checks.worked(b.listen(), "listening on it");
    let made = Socket::new(listen, SocketType::Datagram, PipeFlags::NONE);
    if let Some(datagrams) = checks.worked(made, "making a datagram socket to listen") {
        checks.refused(
            datagrams.send_to(listen, (LOOPBACK, 7001), b"no"),
            Kernel::MissingRights,
            "sending a datagram without the send right",
        );
    }
    // Whatever the capability: ports in use, addresses not the machine's,
    // connections no one takes, and the wrong kind of capability.
    checks.refused(
        c.bind(SLOT_NETWORK, LOOPBACK, 8001),
        Kernel::AddressInUse,
        "binding to a port in use",
    );
    checks.refused(
        c.bind(SLOT_NETWORK, [10, 9, 9, 9], 7003),
        Kernel::AddressNotAvailable,
        "binding to an address the machine does not have",
    );
    checks.refused(
        d.connect(out, LOOPBACK, 7999),
        Kernel::Refused,
        "connecting to a port no one listens on",
    );
    checks.refused(
        d.connect(SLOT_ROOT, LOOPBACK, 7000),
        Kernel::WrongKind,
        "connecting through a directory",
    );
    let slot = slots::take().unwrap_or(0);
    let wider = network::restrict(&SYS, listen, slot, 7000, 1, Rights::ALL);
    checks.refused(wider, Kernel::OutOfRange, "restricting to ports the capability does not have");
    let _ = capability::delete(&SYS, slot);
    slots::give_back(slot);
    let number =
        process::info(&SYS, libradix::abi::startup::SLOT_PROCESS).map_or(0, |info| info.number);
    println!(
        "sockets: rights: each method refused what its capability does not allow (process {number})"
    );
}

/// IPv6 over the loopback interface.
fn ipv6(checks: &mut Checks, out: u32) {
    let v6 = u64::from(AddressFamily::Ipv6.raw());
    let made = Socket::new6(SLOT_NETWORK, SocketType::Stream);
    let Some(server) = checks.worked(made, "making an IPv6 stream socket") else { return };
    checks.check(server.option(SocketOption::Family) == Ok(v6), "the IPv6 socket's family");
    checks.check(server.option(SocketOption::V6Only) == Ok(0), "an IPv6 socket takes IPv4 too");
    checks.refused(
        server.bind(SLOT_NETWORK, LOOPBACK, 7100),
        Kernel::NotSupported,
        "binding an IPv6 socket to an IPv4 address",
    );
    let bound = server.bind6(SLOT_NETWORK, ANY6, 7100).and_then(|()| server.listen());
    if checks.worked(bound, "listening on [::] port 7100").is_none() {
        return;
    }

    // From ::1.
    let made = Socket::new6(out, SocketType::Stream);
    let Some(client) = checks.worked(made, "making an IPv6 socket to connect") else { return };
    if checks.worked(client.connect6(out, LOOPBACK6, 7100), "connecting to [::1]:7100").is_none() {
        return;
    }
    let Some(accepted) = checks.worked(server.accept(), "taking the IPv6 connection") else {
        return;
    };
    let near = client.name6(false);
    checks.check(
        near.is_ok_and(|(family, ip, port)| {
            u64::from(family) == v6 && ip == LOOPBACK6 && u64::from(port) >= EPHEMERAL_FIRST
        }),
        &format!("the IPv6 connecting socket is bound to {near:?}"),
    );
    checks.check(accepted.name6(true) == near, "the taken IPv6 connection's peer");
    checks.check(
        accepted.name6(false).map(|(_, ip, port)| (ip, port)) == Ok((LOOPBACK6, 7100)),
        "the taken IPv6 connection's name",
    );
    let line = b"hello over IPv6";
    let sent = client.send_all(line).and_then(|()| accepted.receive_len(line.len()));
    checks.check(sent.as_deref() == Ok(&line[..]), "a line over IPv6");
    let sent = accepted.send_all(b"back").and_then(|()| client.receive_len(4));
    checks.check(sent.as_deref() == Ok(&b"back"[..]), "a line back over IPv6");

    // From 127.0.0.1, to the same socket: its peer is IPv4-mapped.
    let made = Socket::new(out, SocketType::Stream, PipeFlags::NONE);
    let Some(client4) = checks.worked(made, "making an IPv4 socket to connect") else { return };
    checks.worked(client4.connect(out, LOOPBACK, 7100), "connecting to 127.0.0.1 port 7100");
    if let Some(taken) = checks.worked(server.accept(), "taking the IPv4 connection") {
        let peer = taken.name6(true).map(|(_, ip, port)| (ip, port));
        let want = client4.name(false).map(|(ip, port)| (mapped(ip), port));
        checks.check(peer == want, &format!("the IPv4 peer is {peer:?}, not {want:?}"));
        let sent = client4.send_all(b"v4").and_then(|()| taken.receive_len(2));
        checks.check(sent.as_deref() == Ok(&b"v4"[..]), "a line from IPv4 to the IPv6 socket");
    }
    println!("sockets: ipv6: a socket on [::] took connections from ::1 and from 127.0.0.1");

    // Datagrams between ::1's ports.
    let (Ok(bound), Ok(sender)) = (
        Socket::new6(SLOT_NETWORK, SocketType::Datagram),
        Socket::new6(out, SocketType::Datagram),
    ) else {
        checks.check(false, "making IPv6 datagram sockets");
        return;
    };
    checks.worked(bound.bind6(SLOT_NETWORK, LOOPBACK6, 7101), "binding to [::1]:7101");
    let sent = sender.send_to6(out, LOOPBACK6, 7101, b"datagram six");
    checks.check(sent == Ok(12), "sending a datagram to [::1]:7101");
    let mut buffer = [0u8; 64];
    let got = bound.receive_from6(&mut buffer);
    let port = sender.name6(false).map(|(.., port)| port).unwrap_or(0);
    checks.check(
        got == Ok((12, (v6 as u32, LOOPBACK6, port))) && &buffer[..12] == b"datagram six",
        &format!("the IPv6 datagram came as {got:?}"),
    );
    println!("sockets: ipv6: a datagram went between ports of ::1");

    // A v6-only socket shares its port with an IPv4 one, and reaches no
    // IPv4 machine.
    let (Ok(only), Ok(four)) = (
        Socket::new6(SLOT_NETWORK, SocketType::Stream),
        Socket::new(SLOT_NETWORK, SocketType::Stream, PipeFlags::NONE),
    ) else {
        checks.check(false, "making the sockets to share a port");
        return;
    };
    checks.worked(only.set_option(SocketOption::V6Only, 1), "making a socket v6-only");
    checks.worked(four.bind(SLOT_NETWORK, [0; 4], 7102), "binding an IPv4 socket to port 7102");
    checks.worked(only.bind6(SLOT_NETWORK, ANY6, 7102), "binding a v6-only one to port 7102");
    let made = Socket::new6(out, SocketType::Stream);
    if let Some(socket) = checks.worked(made, "making a socket to stay v6-only") {
        checks.worked(socket.set_option(SocketOption::V6Only, 1), "making it v6-only");
        checks.refused(
            socket.connect6(out, mapped(LOOPBACK), 7100),
            Kernel::Unreachable,
            "a v6-only socket connecting to an IPv4-mapped address",
        );
    }
    checks.refused(
        server.bind6(SLOT_NETWORK, ANY6, 7103),
        Kernel::InvalidArgument,
        "binding a listening socket again",
    );
    println!("sockets: ipv6: a v6-only socket shared its port with an IPv4 one");
}

/// TCP through the virtio card, to QEMU's `cat`.
fn through_virtio(checks: &mut Checks, out: u32) {
    let made = Socket::new(out, SocketType::Stream, PipeFlags::NONE);
    let Some(socket) = checks.worked(made, "making a socket for the virtio card") else { return };
    if checks
        .worked(socket.connect(out, ECHO.0, ECHO.1), "connecting to 10.0.2.100 port 7")
        .is_none()
    {
        return;
    }
    let near = socket.name(false);
    checks.check(
        near.is_ok_and(|near| near.0 == [10, 0, 2, 15]),
        &format!("the connection to QEMU is from {near:?}, not the card's address"),
    );
    let line = b"through the virtio card and back";
    let echoed = socket.send_all(line).and_then(|()| socket.receive_len(line.len()));
    checks.check(echoed.as_deref() == Ok(&line[..]), "what QEMU's cat sent back");
    println!(
        "sockets: virtio: a connection to 10.0.2.100 port 7 echoed what went through the card"
    );
}
