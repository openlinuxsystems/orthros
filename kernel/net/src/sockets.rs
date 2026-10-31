// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Programs' TCP and UDP sockets, on smoltcp's (ADR 0183): the memory
//! each is made from, which comes back when it is taken out of its stack,
//! and the calls on a stack that need its interface, as connecting does.

use smoltcp::iface::SocketHandle;
use smoltcp::socket::{Socket, tcp, udp};
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use crate::{Link, Stack, lent};

/// The buffers of a TCP socket, lent to it for as long as it lives.
pub struct TcpMemory {
    /// The bytes that came, until the program reads them: the window it
    /// offers is this long at most.
    pub receive: &'static mut [u8],
    /// The bytes to send, until the other end has them.
    pub transmit: &'static mut [u8],
}

impl TcpMemory {
    /// A TCP socket, closed, made with this memory. smoltcp takes at most
    /// 1 GiB to receive into.
    pub fn socket(self) -> tcp::Socket<'static> {
        let TcpMemory { receive, transmit } = self;
        tcp::Socket::new(tcp::SocketBuffer::new(receive), tcp::SocketBuffer::new(transmit))
    }

    /// The memory `socket` was made with, back.
    pub fn of(socket: tcp::Socket<'static>) -> TcpMemory {
        let (receive, transmit) = socket.into_buffers();
        TcpMemory { receive: lent(receive.into_storage()), transmit: lent(transmit.into_storage()) }
    }
}

/// The buffers of a UDP socket, lent to it for as long as it lives.
pub struct UdpMemory {
    /// Room for the datagrams that came, until the program reads them.
    pub receive_meta: &'static mut [udp::PacketMetadata],
    /// Their bytes.
    pub receive: &'static mut [u8],
    /// Room for the datagrams to send.
    pub transmit_meta: &'static mut [udp::PacketMetadata],
    /// Their bytes.
    pub transmit: &'static mut [u8],
}

impl UdpMemory {
    /// A UDP socket, not bound, made with this memory.
    pub fn socket(self) -> udp::Socket<'static> {
        let UdpMemory { receive_meta, receive, transmit_meta, transmit } = self;
        udp::Socket::new(
            udp::PacketBuffer::new(receive_meta, receive),
            udp::PacketBuffer::new(transmit_meta, transmit),
        )
    }

    /// The memory `socket` was made with, back.
    pub fn of(socket: udp::Socket<'static>) -> UdpMemory {
        let (receive, transmit) = socket.into_buffers();
        let (receive_meta, receive) = receive.into_storage();
        let (transmit_meta, transmit) = transmit.into_storage();
        UdpMemory {
            receive_meta: lent(receive_meta),
            receive: lent(receive),
            transmit_meta: lent(transmit_meta),
            transmit: lent(transmit),
        }
    }
}

/// Why [`Stack::connect`] could not start a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// The stack has no TCP socket `handle`.
    NotHere,
    /// The socket is open already: connecting, connected or listening.
    Open,
    /// An address or a port is 0, or the stack has no address to connect
    /// from.
    Unaddressable,
}

impl<L: Link> Stack<L> {
    /// Takes the TCP socket `handle` out of the stack, and gives back the
    /// memory it was made with: `None` if the stack has no such socket.
    /// Another kind of socket in its slot stays.
    pub fn remove_tcp(&mut self, handle: SocketHandle) -> Option<TcpMemory> {
        self.socket_mut::<tcp::Socket<'static>>(handle)?;
        match self.remove_socket(handle)? {
            Socket::Tcp(socket) => Some(TcpMemory::of(socket)),
            // The slot held a TCP socket, looked at above.
            _ => None,
        }
    }

    /// Gives back the memory of the TCP socket `handle`, in TIME-WAIT, whose
    /// program closed it: it stays until TIME-WAIT ends, with no memory of
    /// its own, only its slot. `None` if the stack has no such socket, or
    /// it is in another state.
    pub fn shed_tcp(&mut self, handle: SocketHandle) -> Option<TcpMemory> {
        let socket = self.socket_mut::<tcp::Socket<'static>>(handle)?;
        let (receive, transmit) = socket.shed_buffers()?;
        Some(TcpMemory {
            receive: lent(receive.into_storage()),
            transmit: lent(transmit.into_storage()),
        })
    }

    /// Takes the UDP socket `handle` out of the stack, as
    /// [`Stack::remove_tcp`] does.
    pub fn remove_udp(&mut self, handle: SocketHandle) -> Option<UdpMemory> {
        self.socket_mut::<udp::Socket<'static>>(handle)?;
        match self.remove_socket(handle)? {
            Socket::Udp(socket) => Some(UdpMemory::of(socket)),
            // The slot held a UDP socket, looked at above.
            _ => None,
        }
    }

    /// Has the TCP socket `handle` connect to port `port` of `to`, from
    /// port `from` of `local`, or of the address the interface sends to
    /// `to` from. The handshake goes on as the stack polls.
    ///
    /// # Errors
    ///
    /// Why it could not start: see [`ConnectError`].
    pub fn connect(
        &mut self,
        handle: SocketHandle,
        to: (Ipv4Address, u16),
        local: Option<Ipv4Address>,
        from: u16,
    ) -> Result<(), ConnectError> {
        let remote = IpEndpoint::new(IpAddress::Ipv4(to.0), to.1);
        let local = IpListenEndpoint { addr: local.map(IpAddress::Ipv4), port: from };
        let socket = self
            .sockets
            .try_get_mut::<tcp::Socket<'static>>(handle)
            .ok_or(ConnectError::NotHere)?;
        socket.connect(self.iface.context(), remote, local).map_err(|err| match err {
            tcp::ConnectError::InvalidState => ConnectError::Open,
            tcp::ConnectError::Unaddressable => ConnectError::Unaddressable,
        })
    }

    /// The address the interface sends to `to` from, if it has one.
    pub fn source_for(&self, to: Ipv4Address) -> Option<Ipv4Address> {
        self.iface.get_source_address_ipv4(&to)
    }
}

/// A TCP or UDP socket of a stack's, as `/proc/net/tcp` and
/// `/proc/net/udp` show one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shown {
    /// Whether it is TCP's, or UDP's.
    pub tcp: bool,
    /// The address and port it is at: an address of 0.0.0.0 for any.
    pub local: (Ipv4Address, u16),
    /// Where it is connected: 0.0.0.0, port 0, for nowhere.
    pub remote: (Ipv4Address, u16),
    /// A TCP socket's state; for UDP's, Closed.
    pub state: tcp::State,
    /// The bytes it has to send, not yet acknowledged for TCP.
    pub send_queue: usize,
    /// The bytes that came, not yet read.
    pub receive_queue: usize,
}

/// The IPv4 address of `address`, or of any for none.
fn v4(address: Option<IpAddress>) -> Ipv4Address {
    match address {
        Some(IpAddress::Ipv4(address)) => address,
        None => Ipv4Address::UNSPECIFIED,
    }
}

impl Shown {
    /// What `socket` shows: `None` for an ICMP socket, or a UDP or TCP
    /// one not bound yet.
    pub(crate) fn of(socket: &Socket<'_>) -> Option<Shown> {
        match socket {
            Socket::Tcp(socket) => {
                let state = socket.state();
                let (local, remote) = match (socket.local_endpoint(), socket.remote_endpoint()) {
                    (Some(local), remote) => {
                        let remote = remote.map_or((Ipv4Address::UNSPECIFIED, 0), |remote| {
                            (v4(Some(remote.addr)), remote.port)
                        });
                        ((v4(Some(local.addr)), local.port), remote)
                    }
                    (None, _) if state == tcp::State::Listen => {
                        let listen = socket.listen_endpoint();
                        ((v4(listen.addr), listen.port), (Ipv4Address::UNSPECIFIED, 0))
                    }
                    (None, _) => return None,
                };
                Some(Shown {
                    tcp: true,
                    local,
                    remote,
                    state,
                    send_queue: socket.send_queue(),
                    receive_queue: socket.recv_queue(),
                })
            }
            Socket::Udp(socket) => {
                let IpListenEndpoint { addr, port } = socket.endpoint();
                (port != 0).then(|| Shown {
                    tcp: false,
                    local: (v4(addr), port),
                    remote: (Ipv4Address::UNSPECIFIED, 0),
                    state: tcp::State::Closed,
                    send_queue: socket.send_queue(),
                    receive_queue: socket.recv_queue(),
                })
            }
            Socket::Icmp(_) => None,
        }
    }
}
