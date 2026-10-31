// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Network sockets (Phase 12, ADR 0183): TCP's stream sockets and UDP's
//! datagram sockets, over IPv4, on the kernel's stack (`crate::net`).
//!
//! A program makes one from a network capability (radix-network(2)'s
//! `socket`), and binds and connects it through one (radix-file(2)'s
//! `bind_address` and `connect_address`), whose rights and ports the
//! caller checks before it comes here: [`Authority`] is what it found.
//! Like the other sockets, a network socket is a node on the pipes'
//! detached filesystem, so its descriptions are copied, passed and
//! inherited as theirs are.
//!
//! A socket is smoltcp's sockets, its [`Part`]s, one on each interface it
//! is on: a connected stream socket's one on the interface its connection
//! goes through; a datagram socket's one on each interface its address is
//! on, every one when it is bound to any address; a listening socket's a
//! few on each such interface, a pool of smoltcp sockets that listen,
//! each of which becomes a connection when one comes, for `accept` to
//! take and the pool to make again. Each part's memory is lent to smoltcp
//! and comes back when it is taken out (`net::memory`). A socket bound
//! to an interface (SO_BINDTODEVICE, ADR 0190) is on that one alone, and
//! sends through it; on one with no IPv4 address, a datagram goes from
//! 0.0.0.0, as a DHCP client's does.
//!
//! A call that cannot go on waits for networking's count of changes to
//! move (`net::generation`), then looks again; the interfaces' threads
//! move it after a poll that may have changed what a socket can do.
//!
//! A socket that is closed takes its parts out of their stacks, all but
//! a stream socket's connection that has not ended: that one is closed,
//! its FIN sent, and left to its interface as an orphan (`net::Orphan`),
//! which goes once the connection has, or is reset after a minute.

use alloc::vec::Vec;
use core::any::Any;

use radix_abi::Status;
use radix_abi::errors::{File, Kernel};
use radix_abi::file::SocketOption;
use radix_abi::network::{Address, AddressFamily, SocketType};
use radix_abi::process::PipeFlags;
use radix_cap::{ObjectKind, Rights, SlotRef};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_net::smoltcp::iface::SocketHandle;
use radix_net::smoltcp::socket::{tcp, udp};
use radix_net::smoltcp::time::Duration;
use radix_net::smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint};
use radix_net::{Across, ConnectError, Ipv4Address, OtherEnd, TcpMemory};
use radix_sync::Mutex;
use radix_vfs::mount::Location;
use radix_vfs::{In, Kind as NodeKind, Node, NodeRef, Out, Stat, mem, next_id};

use super::{Description, FileObject, FileRef, pipe};
use crate::net::memory::{self, TcpLeases, UdpLeases};
use crate::net::ports::{self, Held, Protocol};
use crate::net::{self, Interface, Orphan};
use crate::sched::{self, wait};
use crate::user::posix::poll::{self, Polled};
use crate::user::posix::signal::{Restart, interrupted};

/// The most smoltcp sockets a listening socket keeps listening on each
/// interface: connections that come faster are refused.
const POOL: usize = 4;
/// How long a connection has to be made.
const CONNECT_NS: u64 = 60_000_000_000;
/// How long a connection waits for an answer to what it sent before it
/// is reset, as Linux's tcp_retries2 gives about.
const ANSWER_SECS: u64 = 900;
/// How long a connection with keep_alive is quiet before it is probed,
/// how long apart the probes go when unanswered, and how many before it
/// is reset, to begin with: Linux's defaults (TCP_KEEPIDLE,
/// TCP_KEEPINTVL and TCP_KEEPCNT).
const KEEP_IDLE_SECS: u16 = 7200;
const KEEP_INTERVAL_SECS: u16 = 75;
const KEEP_COUNT: u8 = 9;
/// The most seconds TCP_KEEPIDLE and TCP_KEEPINTVL take, and probes
/// TCP_KEEPCNT takes, as on Linux.
const KEEP_SECS_MAX: u64 = 32_767;
const KEEP_COUNT_MAX: u64 = 127;
/// The bytes of an IPv4 header and a UDP header, which a datagram's
/// interface's MTU holds with it.
const UDP_HEADERS: usize = 28;
/// The bytes a datagram socket's first copy of a datagram has room for, at
/// least: a DNS query's, say.
const BOUNCE_FIRST: usize = 512;

/// What a network socket carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// TCP's stream of bytes.
    Stream,
    /// UDP's datagrams.
    Datagram,
}

impl Kind {
    fn protocol(self) -> Protocol {
        match self {
            Kind::Stream => Protocol::Tcp,
            Kind::Datagram => Protocol::Udp,
        }
    }
}

/// What a network capability lets its holder do: its rights, and its
/// range of ports, as (first, count).
#[derive(Clone, Copy, Debug)]
pub struct Authority {
    pub rights: Rights,
    pub ports: (u64, u64),
}

impl Authority {
    /// Whether it has the receive right for `port`.
    fn receives_on(&self, port: u16) -> bool {
        let (first, count) = self.ports;
        self.rights.contains(Rights::RECEIVE)
            && u64::from(port) >= first
            && u64::from(port) < first.saturating_add(count)
    }
}

/// An address and a port; no address is any.
pub type Endpoint = (Option<Ipv4Address>, u16);

/// An address and a port to connect or send to, or that sent.
pub type Peer = (Ipv4Address, u16);

/// How many bytes a receive took, and from where.
type Received = (usize, Option<Peer>);

/// The endpoint `bytes`, an Address, is: not_supported for IPv6, until
/// Phase 12c.
pub fn endpoint_of(bytes: &[u8; Address::SIZE]) -> Result<Endpoint, Status> {
    let word =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    match AddressFamily::from_raw(word(Address::OFFSET_FAMILY)) {
        Some(AddressFamily::Ipv4) => {}
        Some(_) => return Err(Kernel::NotSupported.status(0)),
        None => return Err(Kernel::InvalidArgument.status(0)),
    }
    let port = u16::from_le_bytes([bytes[Address::OFFSET_PORT], bytes[Address::OFFSET_PORT + 1]]);
    let at = Address::OFFSET_IP;
    let ip = Ipv4Address::new(bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]);
    Ok(((!ip.is_unspecified()).then_some(ip), port))
}

/// `endpoint` as an Address's bytes.
pub fn address_of(endpoint: Endpoint) -> [u8; Address::SIZE] {
    let mut bytes = [0; Address::SIZE];
    let family = AddressFamily::Ipv4.raw();
    bytes[Address::OFFSET_FAMILY..Address::OFFSET_FAMILY + 4]
        .copy_from_slice(&family.to_le_bytes());
    bytes[Address::OFFSET_PORT..Address::OFFSET_PORT + 2]
        .copy_from_slice(&endpoint.1.to_le_bytes());
    let ip = endpoint.0.unwrap_or(Ipv4Address::UNSPECIFIED);
    bytes[Address::OFFSET_IP..Address::OFFSET_IP + 4].copy_from_slice(&ip.octets());
    bytes
}

/// The memory of a part, lent.
enum Leases {
    Tcp(TcpLeases),
    Udp(UdpLeases),
}

/// One of smoltcp's sockets, of a network socket's, on one interface.
struct Part {
    interface: &'static Interface,
    handle: SocketHandle,
    leases: Leases,
    /// Where a connection's other end was last found on its interface,
    /// when it is a socket of the same one (ADR 0186).
    other: OtherEnd,
}

/// Where a socket is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Neither connected nor listening: maybe bound.
    Fresh,
    /// A stream socket whose connection is being made.
    Connecting,
    /// Connected: for a datagram socket, with where its datagrams go.
    Connected,
    /// Listening: its parts are pools of smoltcp's listening sockets.
    Listening,
}

/// A socket's options, as `socket_option` sets them.
#[derive(Clone, Copy)]
struct Options {
    reuse_address: bool,
    keep_alive: bool,
    keep_idle_secs: u16,
    keep_interval_secs: u16,
    keep_count: u8,
    no_delay: bool,
    broadcast: bool,
    time_to_live: u8,
    receive_timeout_ns: u64,
    send_timeout_ns: u64,
    /// The interface it is bound to, by its number (`ifindex`): 0 for
    /// none.
    device: u32,
}

impl Options {
    const DEFAULT: Options = Options {
        reuse_address: false,
        keep_alive: false,
        keep_idle_secs: KEEP_IDLE_SECS,
        keep_interval_secs: KEEP_INTERVAL_SECS,
        keep_count: KEEP_COUNT,
        no_delay: false,
        broadcast: false,
        time_to_live: 64,
        receive_timeout_ns: 0,
        send_timeout_ns: 0,
        device: 0,
    };

    /// Sets what they say on `socket`, a stream socket's connection.
    fn apply(&self, socket: &mut tcp::Socket<'static>, connected: bool) {
        socket.set_nagle_enabled(!self.no_delay);
        socket.set_hop_limit(Some(self.time_to_live));
        // Probes that go unanswered reset the connection after their count,
        // as on Linux; the wait for an answer to data is the timeout's.
        if self.keep_alive {
            socket.set_keep_alive(Some(Duration::from_secs(self.keep_idle_secs.into())));
            let interval = Duration::from_secs(self.keep_interval_secs.into());
            socket.set_keep_alive_probes(Some((interval, self.keep_count)));
        } else {
            socket.set_keep_alive(None);
        }
        let answer = if connected {
            Duration::from_secs(ANSWER_SECS)
        } else {
            Duration::from_micros(CONNECT_NS / 1_000)
        };
        socket.set_timeout(Some(answer));
    }
}

/// What a socket keeps under its lock.
struct State {
    phase: Phase,
    /// The address and port it is bound to, once it is.
    local: Option<Endpoint>,
    /// The port it holds while it is bound, or connected from a port the
    /// kernel picked.
    held: Option<Held>,
    /// Whether the kernel picked that port when it connected, not bound:
    /// it goes if the connection fails.
    picked: bool,
    /// Whether it was bound through a capability with the receive right
    /// for its port, as listening needs.
    may_listen: bool,
    /// Where it is connected, or for a datagram socket where its
    /// datagrams go.
    peer: Option<Peer>,
    parts: Vec<Part, Kmalloc>,
    /// When its connection began to be made.
    connect_started_ns: u64,
    /// Why its last connection failed, for SO_ERROR: 0 for none.
    error: u64,
    shut_read: bool,
    shut_write: bool,
    options: Options,
    /// Where a datagram socket's datagram is copied from the program before
    /// it goes, made when it first sends one.
    bounce: Option<Vec<u8, Kmalloc>>,
}

impl State {
    /// Puts a UDP socket bound to `address` and `port` on each interface
    /// it is on, as its parts.
    fn datagram_parts(&mut self, address: Option<Ipv4Address>, port: u16) -> Result<(), Status> {
        self.parts.try_reserve_exact(net::MAX_INTERFACES).map_err(|_| status(Kernel::NoMemory))?;
        for interface in interfaces_for(address, self.options.device) {
            match udp_part(interface, address, port) {
                // Room was reserved for every interface.
                Ok(part) => {
                    let _ = self.parts.push_within_capacity(part);
                }
                Err(err) => {
                    release_parts(&mut self.parts, false, None);
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    /// The interface a packet to `to` goes out through: the one it is
    /// bound to, if it can reach `to` (any interface reaches the
    /// broadcast address), else the way the routes go.
    fn route(&self, to: Ipv4Address) -> Result<&'static Interface, Status> {
        let interface = match self.options.device {
            0 => net::route(to),
            device => net::by_index(device)
                .filter(|interface| to.is_broadcast() || interface.stack.lock().routes_to(to)),
        };
        interface.ok_or(status(Kernel::Unreachable))
    }

    /// Starts its connection to `to`, under its lock.
    fn start_connection(&mut self, to: Peer) -> Result<(), Status> {
        let interface = self.route(to.0)?;
        let bound = self.local;
        if let Some((Some(address), _)) = bound
            && !interface.stack.lock().has_address(address)
        {
            // Bound to an address the way to `to` does not go from.
            return Err(status(Kernel::Unreachable));
        }
        let (held, from) = match bound {
            Some((_, port)) => (None, port),
            None => {
                let held = ports::take_any(Protocol::Tcp).ok_or(status(Kernel::AddressInUse))?;
                let port = held.port();
                (Some(held), port)
            }
        };
        self.parts.try_reserve_exact(1).map_err(|_| status(Kernel::NoMemory))?;
        let (leases, tcp_memory) = memory::lend_tcp().ok_or(status(Kernel::NoMemory))?;
        let mut socket = tcp_socket(tcp_memory);
        self.options.apply(&mut socket, false);
        let mut stack = interface.stack.lock();
        let handle = match interface.add_socket(&mut stack, socket) {
            Ok(handle) => handle,
            Err(socket) => {
                drop(stack);
                memory::give_back_tcp(leases, socket);
                return Err(status(Kernel::NoBuffers));
            }
        };
        let local = bound.and_then(|(address, _)| address);
        if let Err(err) = stack.connect(handle, to, local, from) {
            memory::end_tcp(&mut stack, handle, leases, interface.name.as_str());
            return Err(match err {
                ConnectError::Unaddressable => status(Kernel::Unreachable),
                ConnectError::NotHere | ConnectError::Open => status(Kernel::Busy),
            });
        }
        drop(stack);
        // Room was reserved above.
        let _ = self.parts.push_within_capacity(Part {
            interface,
            handle,
            leases: Leases::Tcp(leases),
            other: OtherEnd::default(),
        });
        if held.is_some() {
            self.held = held;
            self.picked = true;
            self.local = Some((None, from));
        }
        self.peer = Some(to);
        self.phase = Phase::Connecting;
        self.connect_started_ns = sched::now_ns();
        self.error = 0;
        interface.kick();
        Ok(())
    }

    /// Whether its connection, being made, has been made, under its lock:
    /// it is connected then; one that failed fails, and the socket can
    /// connect again.
    fn settle(&mut self) -> Result<bool, Status> {
        let Some(&Part { interface, handle, .. }) = self.parts.first() else {
            self.phase = Phase::Fresh;
            return Err(status(Kernel::Refused));
        };
        let mut stack = interface.stack.lock();
        let Some(socket) = stack.socket_mut::<tcp::Socket<'static>>(handle) else {
            return Err(status(Kernel::Refused));
        };
        match socket.state() {
            tcp::State::SynSent | tcp::State::SynReceived => Ok(false),
            tcp::State::Closed => {
                drop(stack);
                let failure = connect_failure(self.connect_started_ns);
                // It is closed: it goes at once.
                release_parts(&mut self.parts, true, None);
                self.phase = Phase::Fresh;
                self.peer = None;
                if core::mem::take(&mut self.picked) {
                    // So does a port the kernel picked for the connection.
                    self.held = None;
                    self.local = None;
                }
                Err(failure)
            }
            _ => {
                self.options.apply(socket, true);
                if let Some(local) = socket.local_endpoint() {
                    self.local = Some((Some(v4(local.addr)), local.port));
                }
                self.phase = Phase::Connected;
                Ok(true)
            }
        }
    }

    /// Its connection's part, settling one being made first.
    fn connection(&mut self) -> Result<Option<usize>, Status> {
        match self.phase {
            Phase::Connected => Ok(Some(0)),
            Phase::Connecting => match self.settle() {
                Ok(true) => Ok(Some(0)),
                Ok(false) => Ok(None),
                Err(err) => {
                    self.error = err.0;
                    Err(err)
                }
            },
            _ => Err(status(Kernel::NotConnected)),
        }
    }
}

/// A network socket, as a node.
pub struct InetSocket {
    /// Its number, which its node shows.
    id: u64,
    kind: Kind,
    state: Mutex<State>,
}

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

fn status(kind: Kernel) -> Status {
    kind.status(0)
}

/// A TCP socket made with `memory`, which sends timestamps (RFC 7323), as
/// Linux's do: in milliseconds, from a random start of its own.
fn tcp_socket(memory: TcpMemory) -> tcp::Socket<'static> {
    let mut socket = memory.socket();
    socket.set_tsval_generator(Some(tsval));
    socket.set_tsval_offset(crate::user::random::u64() as u32);
    socket
}

/// The clock of TCP's timestamps, in milliseconds since boot, wrapping.
fn tsval() -> u32 {
    (sched::now_ns() / 1_000_000) as u32
}

/// A keep-alive setting of `value`, 1 to `max`, as its field holds it.
fn keep<T: TryFrom<u64>>(value: u64, max: u64) -> Result<T, Status> {
    (1..=max)
        .contains(&value)
        .then(|| T::try_from(value).ok())
        .flatten()
        .ok_or(status(Kernel::InvalidArgument))
}

/// The interfaces a socket bound to `address`, and to the interface
/// numbered `device` if not 0, is on: every one for any address, else
/// those that have it; only `device`, when it is bound to one.
fn interfaces_for(
    address: Option<Ipv4Address>,
    device: u32,
) -> impl Iterator<Item = &'static Interface> {
    net::interfaces().filter(move |interface| {
        (device == 0 || interface.index() == device)
            && address.is_none_or(|address| interface.stack.lock().has_address(address))
    })
}

/// The status a failed connection gives: timed out if it was given all
/// its time, else refused.
fn connect_failure(started_ns: u64) -> Status {
    if sched::now_ns().saturating_sub(started_ns) >= CONNECT_NS {
        status(Kernel::TimedOut)
    } else {
        status(Kernel::Refused)
    }
}

impl InetSocket {
    fn new(kind: Kind) -> Result<InetSocket, Status> {
        let mut parts = Vec::new_in(kmalloc());
        parts.try_reserve_exact(1).map_err(|_| status(Kernel::NoMemory))?;
        Ok(InetSocket {
            id: next_id(),
            kind,
            state: Mutex::new(State {
                phase: Phase::Fresh,
                local: None,
                held: None,
                picked: false,
                may_listen: false,
                peer: None,
                parts,
                connect_started_ns: 0,
                error: 0,
                shut_read: false,
                shut_write: false,
                options: Options::DEFAULT,
                bounce: None,
            }),
        })
    }

    /// What it carries.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Waits until `attempt` gives an answer: it returns `None` when the
    /// call cannot go on yet. Fails with would_block at once if
    /// `non_blocking`, and after `timeout_ns` (0: none) with `late`.
    fn wait<R>(
        &self,
        non_blocking: bool,
        timeout_ns: u64,
        late: Kernel,
        reason: &'static str,
        mut attempt: impl FnMut(&mut State) -> Result<Option<R>, Status>,
    ) -> Result<R, Status> {
        let deadline = (timeout_ns > 0).then(|| sched::now_ns().saturating_add(timeout_ns));
        loop {
            let seen = net::generation();
            if let Some(done) = attempt(&mut self.state.lock())? {
                return Ok(done);
            }
            if non_blocking {
                return Err(status(Kernel::WouldBlock));
            }
            let moved = || net::generation() != seen || sched::interrupted();
            match deadline {
                None => wait::wait_until(net::key(), reason, moved),
                Some(deadline) => {
                    if !wait::wait_until_or_timeout(net::key(), reason, deadline, moved) {
                        return Err(status(late));
                    }
                }
            }
            if sched::interrupted() {
                // As on Linux, a call with a timeout is not made again.
                let restart = if timeout_ns > 0 { Restart::Never } else { Restart::Always };
                return Err(interrupted(restart));
            }
        }
    }

    /// Binds it to `endpoint` through `authority` (radix-file(2)'s
    /// `bind_address`).
    pub fn bind(&self, endpoint: Endpoint, authority: Authority) -> Result<(), Status> {
        let mut state = self.state.lock();
        if state.local.is_some() || state.phase != Phase::Fresh {
            return Err(status(Kernel::InvalidArgument));
        }
        self.bind_locked(&mut state, endpoint, Some(authority))
    }

    /// Binds it, under its lock, to `endpoint`: with a port of its own
    /// through `authority`, or with port 0 to one the kernel picks, which
    /// connecting needs no authority for.
    fn bind_locked(
        &self,
        state: &mut State,
        endpoint: Endpoint,
        authority: Option<Authority>,
    ) -> Result<(), Status> {
        let (address, port) = endpoint;
        if let Some(address) = address
            && interfaces_for(Some(address), 0).next().is_none()
        {
            return Err(status(Kernel::AddressNotAvailable));
        }
        let held = if port == 0 {
            ports::take_any(self.kind.protocol()).ok_or(status(Kernel::AddressInUse))?
        } else {
            let authority = authority.ok_or(status(Kernel::InvalidArgument))?;
            if !authority.rights.contains(Rights::RECEIVE) {
                return Err(Kernel::MissingRights.status(u32::from(Rights::RECEIVE.bits())));
            }
            if !authority.receives_on(port) {
                return Err(status(Kernel::OutOfRange));
            }
            // Sockets on other interfaces may hold it too (ADR 0191).
            ports::take_on(self.kind.protocol(), port, state.options.device)
                .ok_or(status(Kernel::AddressInUse))?
        };
        let port = held.port();
        if self.kind == Kind::Datagram {
            // It receives from now on.
            state.datagram_parts(address, port)?;
        }
        state.may_listen = authority.is_some_and(|authority| authority.receives_on(port));
        state.local = Some((address, port));
        state.held = Some(held);
        Ok(())
    }

    /// Binds it, if it is not, to any address and a port the kernel picks:
    /// before it connects or sends.
    fn bind_if_not(&self, state: &mut State) -> Result<(), Status> {
        if state.local.is_none() { self.bind_locked(state, (None, 0), None) } else { Ok(()) }
    }

    /// Has it listen (radix-file(2)'s `listen`).
    pub fn listen(&self, backlog: u32) -> Result<(), Status> {
        if self.kind != Kind::Stream {
            return Err(status(Kernel::NotSupported));
        }
        let backlog = u64::from(backlog).clamp(1, radix_abi::file::BACKLOG_MAX) as usize;
        let mut state = self.state.lock();
        match state.phase {
            // As the pool's size is fixed, a new backlog changes nothing.
            Phase::Listening => return Ok(()),
            Phase::Connecting | Phase::Connected => return Err(status(Kernel::InvalidArgument)),
            Phase::Fresh => {}
        }
        let Some((address, port)) = state.local.filter(|_| state.may_listen) else {
            return Err(Kernel::MissingRights.status(u32::from(Rights::RECEIVE.bits())));
        };
        let each = backlog.min(POOL);
        let mut pool = Vec::new_in(kmalloc());
        pool.try_reserve_exact(each * net::MAX_INTERFACES).map_err(|_| status(Kernel::NoMemory))?;
        for interface in interfaces_for(address, state.options.device) {
            for _ in 0..each {
                match listening_part(interface, address, port, &state.options) {
                    // Room was reserved for every interface's.
                    Ok(part) => {
                        let _ = pool.push_within_capacity(part);
                    }
                    Err(err) => {
                        release_parts(&mut pool, false, None);
                        return Err(err);
                    }
                }
            }
        }
        if pool.is_empty() {
            return Err(status(Kernel::NoBuffers));
        }
        // The pool is the parts from here; a bound socket had none.
        state.parts = pool;
        state.phase = Phase::Listening;
        Ok(())
    }

    /// Takes a connection made to it, waiting for one unless
    /// `non_blocking` (radix-file(2)'s `accept`): a connected socket.
    pub fn accept(&self, non_blocking: bool) -> Result<InetSocket, Status> {
        let timeout = self.state.lock().options.receive_timeout_ns;
        self.wait(non_blocking, timeout, Kernel::WouldBlock, "accept", |state| {
            if state.phase != Phase::Listening {
                return Err(status(Kernel::InvalidArgument));
            }
            let made = state.parts.iter().position(|part| {
                let mut stack = part.interface.stack.lock();
                stack.socket_mut::<tcp::Socket<'static>>(part.handle).is_some_and(|socket| {
                    !matches!(socket.state(), tcp::State::Listen | tcp::State::SynReceived)
                })
            });
            let Some(index) = made else { return Ok(None) };
            let mut accepted = InetSocket::new(Kind::Stream)?;
            let part = state.parts.swap_remove(index);
            // Another socket listens in its place, if there is memory for one.
            let (address, port) = state.local.unwrap_or((None, 0));
            if let Ok(again) = listening_part(part.interface, address, port, &state.options) {
                let _ = state.parts.push_within_capacity(again);
            }
            let (local, peer) = {
                let mut stack = part.interface.stack.lock();
                let socket = stack.socket_mut::<tcp::Socket<'static>>(part.handle);
                let ends = socket.map(|socket| {
                    state.options.apply(socket, true);
                    (socket.local_endpoint(), socket.remote_endpoint())
                });
                ends.unwrap_or((None, None))
            };
            part.interface.kick();
            {
                // Not locked: no one else has it yet, and two sockets' locks
                // are never held at once.
                let new = accepted.state.get_mut();
                new.phase = Phase::Connected;
                new.local = local.map(|ip| (Some(v4(ip.addr)), ip.port));
                new.peer = peer.map(|ip| (v4(ip.addr), ip.port));
                new.options = state.options;
                // Room for one part was reserved when it was made.
                let _ = new.parts.push_within_capacity(part);
            }
            Ok(Some(accepted))
        })
    }

    /// Connects it to `to` (radix-file(2)'s `connect_address`), the caller
    /// having checked the send right: waits for a stream socket's
    /// connection unless `non_blocking`.
    pub fn connect(&self, to: Peer, non_blocking: bool) -> Result<(), Status> {
        if to.1 == 0 || to.0.is_unspecified() {
            return Err(status(Kernel::InvalidArgument));
        }
        if self.kind == Kind::Datagram {
            let mut state = self.state.lock();
            self.bind_if_not(&mut state)?;
            state.peer = Some(to);
            state.phase = Phase::Connected;
            return Ok(());
        }
        let timeout = {
            let mut state = self.state.lock();
            match state.phase {
                Phase::Fresh => {
                    state.start_connection(to)?;
                    if non_blocking {
                        return Err(status(Kernel::InProgress));
                    }
                }
                // As on Linux, connecting again while the connection is
                // being made says how it went: made, failed, or not yet.
                Phase::Connecting => match state.settle() {
                    Ok(true) => return Ok(()),
                    Ok(false) if non_blocking => return Err(status(Kernel::Already)),
                    Ok(false) => {}
                    Err(err) => return Err(err),
                },
                Phase::Connected => return Err(status(Kernel::Connected)),
                Phase::Listening => return Err(status(Kernel::InvalidArgument)),
            }
            state.options.send_timeout_ns
        };
        self.wait(false, timeout, Kernel::InProgress, "connect", |state| match state.phase {
            Phase::Connecting => state.settle().map(|done| done.then_some(())),
            Phase::Connected => Ok(Some(())),
            // Another thread saw it fail.
            _ => {
                let error = core::mem::take(&mut state.error);
                Err(if error == 0 { status(Kernel::Refused) } else { Status(error) })
            }
        })
    }

    /// Sends `data`, waiting for room unless `non_blocking`: a stream
    /// socket's bytes, all of them unless a signal or the timeout comes
    /// after some went; or for a datagram socket, one datagram to where
    /// it is connected, or to `to`. A stream socket that cannot send any
    /// more fails with gone, raising SIGPIPE if `signal`.
    pub fn send(
        &self,
        data: &dyn In,
        to: Option<Peer>,
        non_blocking: bool,
        signal: bool,
    ) -> Result<usize, Status> {
        if self.kind == Kind::Datagram {
            return self.send_datagram(data, to, non_blocking);
        }
        let timeout = self.state.lock().options.send_timeout_ns;
        let mut sent = 0;
        let result = self.wait(non_blocking, timeout, Kernel::WouldBlock, "send", |state| {
            loop {
                if state.shut_write {
                    return Err(status(Kernel::Gone));
                }
                let Some(index) = state.connection()? else { return Ok(None) };
                let part = &mut state.parts[index];
                let interface = part.interface;
                let mut stack = interface.stack.lock();
                let Some(socket) = stack.socket_mut::<tcp::Socket<'static>>(part.handle) else {
                    return Err(status(Kernel::Gone));
                };
                if !socket.may_send() {
                    return Err(status(Kernel::Gone));
                }
                let (mut copied, mut tried) = (Ok(true), 0);
                let mut write = |room: &mut [u8]| {
                    tried = room.len().min(data.len() - sent);
                    copied = data.get_now(sent, &mut room[..tried]);
                    if matches!(copied, Ok(true)) { (tried, tried) } else { (0, 0) }
                };
                // Straight into the other end's buffer, when it is a socket
                // on the same interface, which reads them as soon as its
                // program does (ADR 0186); else into the connection's own,
                // for the stack to send.
                let now = sched::now_ns();
                let (wrote, across) =
                    match stack.send_across(part.handle, &mut part.other, now, &mut write) {
                        Across::Passed(wrote, _) => (wrote, true),
                        Across::Full => (0, true),
                        Across::Packets => {
                            let socket = stack.socket_mut::<tcp::Socket<'static>>(part.handle);
                            (socket.map_or(0, |socket| socket.send(&mut write).unwrap_or(0)), false)
                        }
                    };
                drop(stack);
                if wrote > 0 {
                    // The other end hears of bytes that came across as it
                    // hears of a poll's.
                    if across { net::changed() } else { interface.kick() }
                }
                sent += wrote;
                // A page that was not there is made without the stack's lock,
                // which its interface's thread needs, and the copy tried again.
                let failed = match copied {
                    Ok(true) => None,
                    Ok(false) => data.fault_in(sent, tried).err(),
                    Err(err) => Some(err),
                };
                if let Some(err) = failed {
                    return if sent > 0 { Ok(Some(())) } else { Err(super::status(err)) };
                }
                if matches!(copied, Ok(false)) {
                    continue;
                }
                if sent == data.len() || (non_blocking && sent > 0) {
                    return Ok(Some(()));
                }
                return Ok(None);
            }
        });
        match result {
            Ok(()) => Ok(sent),
            Err(_) if sent > 0 => Ok(sent),
            Err(err) if err == status(Kernel::Gone) => pipe::broken(0, signal),
            Err(err) => Err(err),
        }
    }

    /// Sends `data` as one datagram, to `to` or where it is connected.
    fn send_datagram(
        &self,
        data: &dyn In,
        to: Option<Peer>,
        non_blocking: bool,
    ) -> Result<usize, Status> {
        let (to, interface, timeout) = {
            let mut state = self.state.lock();
            if state.shut_write {
                return pipe::broken(0, false);
            }
            let to = to.or(state.peer).ok_or(status(Kernel::NotConnected))?;
            if to.1 == 0 {
                return Err(status(Kernel::InvalidArgument));
            }
            self.bind_if_not(&mut state)?;
            let interface = state.route(to.0)?;
            let (room, broadcast) = {
                let stack = interface.stack.lock();
                let room = stack.mtu().saturating_sub(UDP_HEADERS);
                (room, to.0.is_broadcast() || stack.is_broadcast(to.0))
            };
            if broadcast && !state.options.broadcast {
                // As on Linux, a program sends to everyone only when it
                // says it means to (SO_BROADCAST).
                return Err(status(Kernel::Broadcast));
            }
            if data.len() > room.min(memory::UDP_BUFFER) {
                return Err(status(Kernel::MessageTooLong));
            }
            (to, interface, state.options.send_timeout_ns)
        };
        let endpoint = IpEndpoint::new(IpAddress::Ipv4(to.0), to.1);
        let len = data.len();
        self.wait(non_blocking, timeout, Kernel::WouldBlock, "send", |state| {
            // The datagram is copied in first, with only the socket's lock
            // held: smoltcp sends what it has room for once it is given it,
            // and copying may wait for a page, which must not hold up the
            // stack.
            if state.bounce.as_ref().is_none_or(|bounce| bounce.len() < len) {
                let size = len.next_power_of_two().clamp(BOUNCE_FIRST, memory::UDP_BUFFER);
                let bounce = radix_mm::bytes::zeroed(size, kmalloc());
                state.bounce = Some(bounce.map_err(|_| status(Kernel::NoMemory))?);
            }
            let bounce = state.bounce.as_mut().map_or(&mut [][..], |bounce| &mut bounce[..len]);
            data.get(0, bounce).map_err(super::status)?;
            let part = state
                .parts
                .iter()
                .find(|part| core::ptr::eq(part.interface, interface))
                .ok_or(status(Kernel::Unreachable))?;
            let bounce = state.bounce.as_ref().map_or(&[][..], |bounce| &bounce[..len]);
            let bound_to_any = state.local.is_some_and(|(address, _)| address.is_none());
            let mut stack = interface.stack.lock();
            // An interface with no address of its own sends from 0.0.0.0,
            // as smoltcp would otherwise drop the datagram.
            let from = (bound_to_any && stack.ipv4().is_none())
                .then_some(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED));
            let meta = udp::UdpMetadata { local_address: from, ..endpoint.into() };
            let socket = stack
                .socket_mut::<udp::Socket<'static>>(part.handle)
                .ok_or(status(Kernel::Unreachable))?;
            match socket.send_slice(bounce, meta) {
                Ok(()) => {
                    drop(stack);
                    interface.kick();
                    Ok(Some(len))
                }
                Err(udp::SendError::BufferFull) => Ok(None),
                Err(udp::SendError::Unaddressable) => Err(status(Kernel::Unreachable)),
            }
        })
    }

    /// Receives into `out`, waiting unless `non_blocking`: a stream
    /// socket's bytes, 0 at the end; a datagram socket's next datagram,
    /// of which what `out` has no room for is lost. Returns how many bytes
    /// came, and from where.
    pub fn receive(&self, out: &mut dyn Out, non_blocking: bool) -> Result<Received, Status> {
        let timeout = self.state.lock().options.receive_timeout_ns;
        self.wait(non_blocking, timeout, Kernel::WouldBlock, "receive", |state| {
            if state.shut_read {
                return Ok(Some((0, state.peer)));
            }
            match self.kind {
                Kind::Stream => receive_stream(state, out),
                Kind::Datagram => receive_datagram(state, out),
            }
        })
    }

    /// Shuts it for reading, writing or both (radix-file(2)'s
    /// `shutdown`): a stream socket's connection sends its FIN.
    pub fn shutdown(&self, read: bool, write: bool) -> Result<(), Status> {
        let mut state = self.state.lock();
        if state.phase != Phase::Connected && state.phase != Phase::Connecting {
            return Err(status(Kernel::NotConnected));
        }
        state.shut_read |= read;
        if write && !state.shut_write {
            state.shut_write = true;
            if self.kind == Kind::Stream
                && let Some(part) = state.parts.first()
            {
                if let Some(socket) =
                    part.interface.stack.lock().socket_mut::<tcp::Socket<'static>>(part.handle)
                {
                    socket.close();
                }
                part.interface.kick();
            }
        }
        drop(state);
        net::changed();
        Ok(())
    }

    /// Where it is bound, or with `peer` where it is connected.
    pub fn name(&self, peer: bool) -> Result<Endpoint, Status> {
        let state = self.state.lock();
        if peer {
            let peer = state.peer.filter(|_| state.phase == Phase::Connected);
            return peer.map(|(ip, port)| (Some(ip), port)).ok_or(status(Kernel::NotConnected));
        }
        Ok(state.local.unwrap_or((None, 0)))
    }

    /// Whether it listens, and whether it is connected.
    pub fn listens_and_connected(&self) -> (bool, bool) {
        let phase = self.state.lock().phase;
        (phase == Phase::Listening, phase == Phase::Connected)
    }

    /// Reads, and with `set` sets, `option` (radix-file(2)'s
    /// `socket_option`), returning it as it was.
    pub fn option(&self, option: SocketOption, set: Option<u64>) -> Result<u64, Status> {
        let mut state = self.state.lock();
        let flag = |on: bool| u64::from(on);
        let options = &mut state.options;
        let old = match option {
            SocketOption::ReuseAddress => flag(options.reuse_address),
            SocketOption::KeepAlive => flag(options.keep_alive),
            SocketOption::KeepIdle => u64::from(options.keep_idle_secs),
            SocketOption::KeepInterval => u64::from(options.keep_interval_secs),
            SocketOption::KeepCount => u64::from(options.keep_count),
            SocketOption::NoDelay => flag(options.no_delay),
            SocketOption::Broadcast => flag(options.broadcast),
            SocketOption::TimeToLive => u64::from(options.time_to_live),
            SocketOption::ReceiveTimeout => options.receive_timeout_ns,
            SocketOption::SendTimeout => options.send_timeout_ns,
            SocketOption::BindToDevice => {
                let Some(value) = set else { return Ok(u64::from(options.device)) };
                let old = u64::from(options.device);
                return self.bind_to_device(&mut state, value).map(|()| old);
            }
            SocketOption::ReceiveBuffer | SocketOption::SendBuffer => match self.kind {
                Kind::Stream => memory::TCP_BUFFER as u64,
                Kind::Datagram => memory::UDP_BUFFER as u64,
            },
            SocketOption::Error => {
                if set.is_some() {
                    return Err(status(Kernel::InvalidArgument));
                }
                return Ok(core::mem::replace(&mut state.error, 0));
            }
            SocketOption::Kind => {
                if set.is_some() {
                    return Err(status(Kernel::InvalidArgument));
                }
                return Ok(match self.kind {
                    Kind::Stream => u64::from(SocketType::Stream.raw()),
                    Kind::Datagram => u64::from(SocketType::Datagram.raw()),
                });
            }
        };
        let Some(value) = set else { return Ok(old) };
        match option {
            SocketOption::ReuseAddress => options.reuse_address = value != 0,
            SocketOption::KeepAlive => options.keep_alive = value != 0,
            SocketOption::KeepIdle => options.keep_idle_secs = keep(value, KEEP_SECS_MAX)?,
            SocketOption::KeepInterval => {
                options.keep_interval_secs = keep(value, KEEP_SECS_MAX)?;
            }
            SocketOption::KeepCount => options.keep_count = keep(value, KEEP_COUNT_MAX)?,
            SocketOption::NoDelay => options.no_delay = value != 0,
            SocketOption::Broadcast => options.broadcast = value != 0,
            SocketOption::TimeToLive => {
                options.time_to_live = u8::try_from(value)
                    .ok()
                    .filter(|&ttl| ttl > 0)
                    .ok_or(status(Kernel::InvalidArgument))?;
            }
            SocketOption::ReceiveTimeout => options.receive_timeout_ns = value,
            SocketOption::SendTimeout => options.send_timeout_ns = value,
            // Taken, and changing nothing.
            _ => {}
        }
        // A connection takes them at once, and its interface looks again
        // at when its timers are due.
        let options = state.options;
        if self.kind == Kind::Stream && state.phase == Phase::Connected {
            for part in &state.parts {
                if let Some(socket) =
                    part.interface.stack.lock().socket_mut::<tcp::Socket<'static>>(part.handle)
                {
                    options.apply(socket, true);
                }
                part.interface.kick();
            }
        }
        if self.kind == Kind::Datagram {
            for part in &state.parts {
                if let Some(socket) =
                    part.interface.stack.lock().socket_mut::<udp::Socket<'static>>(part.handle)
                {
                    socket.set_hop_limit(Some(options.time_to_live));
                }
            }
        }
        Ok(old)
    }

    /// Binds it, under its lock, to the interface numbered `device`, or
    /// to none for 0 (SO_BINDTODEVICE): before a stream socket connects
    /// or listens; a datagram socket already bound moves its parts to
    /// that interface, the datagrams waiting in the others lost. A socket
    /// that shares its port with those on other interfaces stays on its
    /// own.
    fn bind_to_device(&self, state: &mut State, device: u64) -> Result<(), Status> {
        let no_device = File::NoDevice.status(0);
        let device = u32::try_from(device).map_err(|_| no_device)?;
        if device != 0 && net::by_index(device).is_none() {
            return Err(no_device);
        }
        if self.kind == Kind::Stream && state.phase != Phase::Fresh {
            return Err(status(Kernel::InvalidArgument));
        }
        if device == state.options.device {
            return Ok(());
        }
        if state.held.as_ref().is_some_and(Held::shared) {
            return Err(status(Kernel::InvalidArgument));
        }
        let was = core::mem::replace(&mut state.options.device, device);
        if let (Kind::Datagram, Some((address, port))) = (self.kind, state.local) {
            let mut old = core::mem::replace(&mut state.parts, Vec::new_in(kmalloc()));
            if let Err(err) = state.datagram_parts(address, port) {
                // Back as it was.
                state.options.device = was;
                state.parts = old;
                return Err(err);
            }
            release_parts(&mut old, false, None);
        }
        Ok(())
    }

    /// How many bytes wait to be received: a stream socket's, or a
    /// datagram socket's next datagram's.
    pub fn pending(&self) -> usize {
        let state = self.state.lock();
        let mut pending = 0;
        for part in &state.parts {
            let mut stack = part.interface.stack.lock();
            pending += match self.kind {
                Kind::Stream => stack
                    .socket_mut::<tcp::Socket<'static>>(part.handle)
                    .map_or(0, |s| s.recv_queue()),
                Kind::Datagram => stack
                    .socket_mut::<udp::Socket<'static>>(part.handle)
                    .and_then(|socket| socket.peek().ok().map(|(data, _)| data.len()))
                    .unwrap_or(0),
            };
            if pending > 0 && self.kind == Kind::Datagram {
                break;
            }
        }
        pending
    }

    /// The events `poll` finds on it, as Linux's.
    pub fn events(&self) -> u16 {
        let mut state = self.state.lock();
        if self.kind == Kind::Datagram {
            let mut events = poll::WRITABLE;
            for part in &state.parts {
                let mut stack = part.interface.stack.lock();
                if stack
                    .socket_mut::<udp::Socket<'static>>(part.handle)
                    .is_some_and(|s| s.can_recv())
                {
                    events |= poll::READABLE;
                }
            }
            if state.shut_read {
                events |= poll::READABLE;
            }
            return events;
        }
        match state.phase {
            Phase::Fresh => poll::WRITABLE | poll::HANG_UP,
            Phase::Listening => {
                let waiting = state.parts.iter().any(|part| {
                    let mut stack = part.interface.stack.lock();
                    stack.socket_mut::<tcp::Socket<'static>>(part.handle).is_some_and(|socket| {
                        !matches!(socket.state(), tcp::State::Listen | tcp::State::SynReceived)
                    })
                });
                if waiting { poll::READABLE } else { 0 }
            }
            Phase::Connecting => match state.settle() {
                Ok(true) => connection_events(&mut state),
                Ok(false) => 0,
                Err(err) => {
                    state.error = err.0;
                    poll::WRITABLE | poll::ERROR | poll::HANG_UP
                }
            },
            Phase::Connected => connection_events(&mut state),
        }
    }
}

/// Takes a socket's `parts` out of their stacks: a connection that has
/// not ended is closed if the socket is `connected` through it, and left
/// to its interface to finish, holding `port`; else it is reset.
fn release_parts(parts: &mut Vec<Part, Kmalloc>, connected: bool, mut port: Option<Held>) {
    while let Some(Part { interface, handle, leases, .. }) = parts.pop() {
        let mut stack = interface.stack.lock();
        match leases {
            Leases::Udp(leases) => {
                memory::end_udp(&mut stack, handle, leases, interface.name.as_str());
            }
            Leases::Tcp(leases) => {
                let open = stack.socket_mut::<tcp::Socket<'static>>(handle).is_some_and(|socket| {
                    match socket.state() {
                        tcp::State::Closed | tcp::State::Listen => false,
                        // Its own connection ends as connections do.
                        _ if connected => {
                            socket.close();
                            true
                        }
                        // A listening socket's that no one took are reset.
                        _ => {
                            socket.abort();
                            true
                        }
                    }
                });
                if open {
                    let orphan = Orphan {
                        handle,
                        leases: Some(leases),
                        port: port.take(),
                        deadline_ns: sched::now_ns().saturating_add(net::ORPHAN_NS),
                    };
                    interface.adopt(&mut stack, orphan);
                } else {
                    memory::end_tcp(&mut stack, handle, leases, interface.name.as_str());
                }
            }
        }
        drop(stack);
        interface.kick();
    }
}

/// The IPv4 address `address` is: IPv4 is the stack's only protocol,
/// until IPv6 in 12c.
fn v4(address: IpAddress) -> Ipv4Address {
    let IpAddress::Ipv4(address) = address;
    address
}

/// A UDP socket on `interface`, bound to `address` and `port`.
fn udp_part(
    interface: &'static Interface,
    address: Option<Ipv4Address>,
    port: u16,
) -> Result<Part, Status> {
    let (leases, udp_memory) = memory::lend_udp().ok_or(status(Kernel::NoMemory))?;
    let mut socket = udp_memory.socket();
    let endpoint = IpListenEndpoint { addr: address.map(IpAddress::Ipv4), port };
    if socket.bind(endpoint).is_err() {
        memory::give_back_udp(leases, socket);
        return Err(status(Kernel::InvalidArgument));
    }
    let mut stack = interface.stack.lock();
    match interface.add_socket(&mut stack, socket) {
        Ok(handle) => {
            Ok(Part { interface, handle, leases: Leases::Udp(leases), other: OtherEnd::default() })
        }
        Err(socket) => {
            drop(stack);
            memory::give_back_udp(leases, socket);
            Err(status(Kernel::NoBuffers))
        }
    }
}

/// A TCP socket on `interface` that listens at `address` and `port`.
fn listening_part(
    interface: &'static Interface,
    address: Option<Ipv4Address>,
    port: u16,
    options: &Options,
) -> Result<Part, Status> {
    let (leases, tcp_memory) = memory::lend_tcp().ok_or(status(Kernel::NoMemory))?;
    let mut socket = tcp_socket(tcp_memory);
    options.apply(&mut socket, true);
    let endpoint = IpListenEndpoint { addr: address.map(IpAddress::Ipv4), port };
    if socket.listen(endpoint).is_err() {
        memory::give_back_tcp(leases, socket);
        return Err(status(Kernel::InvalidArgument));
    }
    let mut stack = interface.stack.lock();
    match interface.add_socket(&mut stack, socket) {
        Ok(handle) => {
            Ok(Part { interface, handle, leases: Leases::Tcp(leases), other: OtherEnd::default() })
        }
        Err(socket) => {
            drop(stack);
            memory::give_back_tcp(leases, socket);
            Err(status(Kernel::NoBuffers))
        }
    }
}

/// What a stream socket's connection is ready for.
fn connection_events(state: &mut State) -> u16 {
    let shut_read = state.shut_read;
    let Some(part) = state.parts.first_mut() else { return poll::HANG_UP };
    let mut stack = part.interface.stack.lock();
    // A connection whose bytes go straight across can send as many as the
    // other end has room for (ADR 0186).
    let across = stack.room_across(part.handle, &mut part.other);
    let Some(socket) = stack.socket_mut::<tcp::Socket<'static>>(part.handle) else {
        return poll::HANG_UP;
    };
    let mut events = 0;
    if socket.can_recv() || !socket.may_recv() || shut_read {
        events |= poll::READABLE;
    }
    if across.map_or_else(|| socket.can_send(), |room| room > 0) || !socket.may_send() {
        events |= poll::WRITABLE;
    }
    if !socket.is_open() || (!socket.may_recv() && !socket.may_send()) {
        events |= poll::HANG_UP;
    }
    events
}

/// One try at receiving from a stream socket's connection into `out`.
fn receive_stream(state: &mut State, out: &mut dyn Out) -> Result<Option<Received>, Status> {
    loop {
        let Some(index) = state.connection()? else { return Ok(None) };
        let peer = state.peer;
        let part = &mut state.parts[index];
        let interface = part.interface;
        let mut stack = interface.stack.lock();
        let Some(connection) = stack.socket_mut::<tcp::Socket<'static>>(part.handle) else {
            return Err(status(Kernel::Reset));
        };
        if out.is_empty() {
            return Ok(Some((0, peer)));
        }
        // Whether the window it offers had shut to half its buffer or less.
        let narrow = connection.recv_queue() * 2 >= connection.recv_capacity();
        let (mut copied, mut tried) = (Ok(true), 0);
        let read = connection.recv(|bytes| {
            tried = bytes.len().min(out.len());
            copied = out.put_now(0, &bytes[..tried]);
            if matches!(copied, Ok(true)) { (tried, tried) } else { (0, 0) }
        });
        // The window the read opened goes straight to the other end when
        // it is a socket on the same interface (ADR 0186), in no packet:
        // whether it has bytes to send, which a poll sends.
        let told = match read {
            Ok(n) if n > 0 => stack.window_across(part.handle, &mut part.other, sched::now_ns()),
            _ => None,
        };
        drop(stack);
        match copied {
            Ok(true) => {}
            // As in `send`: the page is made without the stack's lock.
            Ok(false) => {
                out.fault_in(0, tried).map_err(super::status)?;
                continue;
            }
            Err(err) => return Err(super::status(err)),
        }
        return match read {
            Ok(0) => Ok(None),
            Ok(n) => {
                // The window it offers grew. The other end hears of it at
                // once only if it had shut to half the buffer or less, as on
                // Linux, which sends a window by itself only when it opens
                // that much: until then the other end has half the buffer
                // to send into, and the next ACK carries the window. One
                // told straight has gone already; the other end's program
                // may be waiting for it.
                match told {
                    Some(waiting) => {
                        if narrow {
                            net::changed();
                        }
                        if waiting {
                            interface.kick();
                        }
                    }
                    None if narrow => interface.kick(),
                    None => {}
                }
                Ok(Some((n, peer)))
            }
            // The other end sent its FIN, and all it sent has been read.
            Err(tcp::RecvError::Finished) => Ok(Some((0, peer))),
            Err(tcp::RecvError::InvalidState) => Err(status(Kernel::Reset)),
        };
    }
}

/// One try at receiving a datagram from any of a datagram socket's parts
/// into `out`: datagrams from elsewhere than where a connected one is
/// connected are dropped. A datagram is taken only once it is copied.
fn receive_datagram(state: &mut State, out: &mut dyn Out) -> Result<Option<Received>, Status> {
    let peer = (state.phase == Phase::Connected).then_some(state.peer).flatten();
    let mut index = 0;
    while let Some(part) = state.parts.get(index) {
        let mut stack = part.interface.stack.lock();
        let Some(socket) = stack.socket_mut::<udp::Socket<'static>>(part.handle) else {
            index += 1;
            continue;
        };
        let Ok((data, meta)) = socket.peek() else {
            index += 1;
            continue;
        };
        let from = (v4(meta.endpoint.addr), meta.endpoint.port);
        if peer.is_some_and(|peer| peer != from) {
            let _ = socket.recv();
            continue;
        }
        let n = data.len().min(out.len());
        match out.put_now(0, &data[..n]) {
            Ok(true) => {
                let _ = socket.recv();
                return Ok(Some((n, Some(from))));
            }
            // As in `receive_stream`, and this part is looked at again.
            Ok(false) => {
                drop(stack);
                out.fault_in(0, n).map_err(super::status)?;
            }
            Err(err) => return Err(super::status(err)),
        }
    }
    Ok(None)
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        // A connection that is still closing keeps its port until it has.
        let connected = matches!(state.phase, Phase::Connecting | Phase::Connected);
        let port = state.held.take_if(|_| connected && self.kind == Kind::Stream);
        release_parts(&mut state.parts, connected, port);
    }
}

impl Node for InetSocket {
    fn stat(&self) -> Stat {
        Stat {
            kind: NodeKind::Socket,
            size: 0,
            allocated: 0,
            id: self.id,
            created: 0,
            modified: 0,
            accessed: 0,
            changed: 0,
            links: 1,
            device: 0,
        }
    }

    fn kind(&self) -> NodeKind {
        NodeKind::Socket
    }

    // Reads and writes through the node itself (the kernel's own) never
    // wait.
    fn read(&self, _offset: u64, out: &mut dyn Out) -> radix_vfs::Result<usize> {
        self.receive(out, true).map(|(n, _)| n).map_err(|_| radix_vfs::Error::NotSupported)
    }

    fn write(&self, _offset: u64, data: &dyn In) -> radix_vfs::Result<usize> {
        self.send(data, None, true, false).map_err(|_| radix_vfs::Error::NotSupported)
    }

    fn set_size(&self, _size: u64) -> radix_vfs::Result<()> {
        Err(radix_vfs::Error::NotSupported)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A description of `socket`, with `flags`, acting for `user`'s process's
/// name.
pub fn file(
    user: &crate::user::UserRef,
    socket: InetSocket,
    flags: PipeFlags,
) -> Result<FileRef, Status> {
    let mount = pipe::mount().map_err(super::status)?;
    let node: NodeRef = mem::arc(socket).map_err(|_| status(Kernel::NoMemory))?;
    let description =
        Description { non_blocking: flags.contains(PipeFlags::NON_BLOCKING), append: false };
    FileObject::with(Location::new(mount, node), description, user.process.identity())
        .ok_or(status(Kernel::NoMemory))
}

/// Makes a network socket of `kind` for `user`'s process, in slot `to`,
/// with `flags` (radix-network(2)'s `socket`).
pub fn make(
    user: &crate::user::UserRef,
    to: u32,
    kind: Kind,
    flags: PipeFlags,
) -> Result<(), Status> {
    install(user, to, file(user, InetSocket::new(kind)?, flags)?, flags)
}

/// Puts a capability to `file`, a network socket's description, in
/// `user`'s slot `slot`, closed by exec if `flags` say so.
pub fn install(
    user: &crate::user::UserRef,
    slot: u32,
    file: FileRef,
    flags: PipeFlags,
) -> Result<(), Status> {
    let rights = Rights::COPY.union(Rights::TRANSFER).union(Rights::READ).union(Rights::WRITE);
    crate::user::files::install_file(
        user,
        SlotRef::new(user.process.cspace(), slot),
        ObjectKind::File,
        rights,
        file,
        flags.contains(PipeFlags::CLOSE_ON_EXEC),
    )
}

/// The network socket `node` is, if it is one.
pub fn socket_of(node: &NodeRef) -> Option<&InetSocket> {
    radix_vfs::downcast::<InetSocket>(&**node)
}

/// Calls `each` with what a network socket's pollers count themselves in:
/// networking's, for every network socket.
pub fn polled(each: &mut dyn FnMut(&Polled)) {
    each(&net::POLLED);
}
