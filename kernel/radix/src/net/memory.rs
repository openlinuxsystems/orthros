// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The memory of programs' TCP and UDP sockets (ADR 0183), lent to
//! smoltcp from the heap through leases, and taken back when the socket
//! comes out of its stack (ADR 0182).
//!
//! The memory comes back as the return value of `Stack::remove_tcp` or
//! `Stack::remove_udp`, and is turned into raw pointers ([`TcpPlaces`],
//! [`UdpPlaces`]) before the leases end: a call that has the slices as
//! arguments must not free them, as a lease's end says.
//!
//! A few buffers of sockets that closed are kept to make new sockets with
//! ([`SPARES`]), so that a program that opens and closes connections does
//! not have the heap make and zero 128 KiB for each (ADR 0185). What a
//! closed socket left in a buffer stays out of reach: smoltcp reads only
//! the bytes written to a buffer since it was lent.

use alloc::vec::Vec;
use core::ptr;

use radix_klog::warn;
use radix_mm::Kmalloc;
use radix_mm::lease::{self, Lease};
use radix_net::smoltcp::iface::SocketHandle;
use radix_net::smoltcp::socket::udp;
use radix_net::{Stack, TcpMemory, UdpMemory};
use radix_sync::Mutex;

use super::{Card, kmalloc, values};

/// The bytes a TCP socket holds each way: the most it offers the other
/// end, and the most it sends before an answer.
pub const TCP_BUFFER: usize = 65_536;
/// The bytes of datagrams a UDP socket holds each way, and how many
/// datagrams: the longest the loopback interface carries fits, 65,507
/// bytes, as on Linux.
pub const UDP_BUFFER: usize = TCP_BUFFER;
pub const UDP_DATAGRAMS: usize = 16;

/// How many buffers of [`TCP_BUFFER`] bytes are kept once their sockets
/// closed: four sockets' worth, 512 KiB.
const SPARES: usize = 8;

/// The buffers kept, from sockets that closed. A leaf lock: taken last,
/// and with the stack's lock held as a socket comes out of its stack.
static SPARE: Mutex<[Option<Vec<u8, Kmalloc>>; SPARES]> = Mutex::new([const { None }; SPARES]);

/// A buffer of [`TCP_BUFFER`] bytes for a new socket: a spare one, or one
/// made from the heap, zeroed.
fn buffer() -> Option<Vec<u8, Kmalloc>> {
    let spare = SPARE.lock().iter_mut().find_map(Option::take);
    spare.or_else(|| radix_mm::bytes::zeroed(TCP_BUFFER, kmalloc()).ok())
}

/// Keeps `buffer`, which a socket that closed gave back, if there is room
/// among the spares; else it goes back to the heap.
fn keep(buffer: Vec<u8, Kmalloc>) {
    if buffer.len() != TCP_BUFFER {
        return;
    }
    let mut spare = SPARE.lock();
    if let Some(slot) = spare.iter_mut().find(|slot| slot.is_none()) {
        *slot = Some(buffer);
        return;
    }
    // Freed without the lock.
    drop(spare);
    drop(buffer);
}

/// The leases of a TCP socket's memory.
pub struct TcpLeases {
    receive: Lease<u8, Kmalloc>,
    transmit: Lease<u8, Kmalloc>,
}

/// The leases of a UDP socket's memory.
pub struct UdpLeases {
    receive_meta: Lease<udp::PacketMetadata, Kmalloc>,
    receive: Lease<u8, Kmalloc>,
    transmit_meta: Lease<udp::PacketMetadata, Kmalloc>,
    transmit: Lease<u8, Kmalloc>,
}

/// Memory for a TCP socket, lent from the heap.
pub fn lend_tcp() -> Option<(TcpLeases, TcpMemory)> {
    let receive = buffer()?;
    let Some(transmit) = buffer() else {
        keep(receive);
        return None;
    };
    let (receive_lease, receive) = lease::lend(receive);
    let (transmit_lease, transmit) = lease::lend(transmit);
    Some((
        TcpLeases { receive: receive_lease, transmit: transmit_lease },
        TcpMemory { receive, transmit },
    ))
}

/// Memory for a UDP socket, lent from the heap.
pub fn lend_udp() -> Option<(UdpLeases, UdpMemory)> {
    let receive_meta = values(|| udp::PacketMetadata::EMPTY, UDP_DATAGRAMS)?;
    let transmit_meta = values(|| udp::PacketMetadata::EMPTY, UDP_DATAGRAMS)?;
    let receive = buffer()?;
    let Some(transmit) = buffer() else {
        keep(receive);
        return None;
    };
    let (receive_meta_lease, receive_meta) = lease::lend(receive_meta);
    let (receive_lease, receive) = lease::lend(receive);
    let (transmit_meta_lease, transmit_meta) = lease::lend(transmit_meta);
    let (transmit_lease, transmit) = lease::lend(transmit);
    Some((
        UdpLeases {
            receive_meta: receive_meta_lease,
            receive: receive_lease,
            transmit_meta: transmit_meta_lease,
            transmit: transmit_lease,
        },
        UdpMemory { receive_meta, receive, transmit_meta, transmit },
    ))
}

/// Where a TCP socket's memory is once it came back, as raw pointers:
/// what it was lent as is not used again.
pub struct TcpPlaces {
    receive: *mut [u8],
    transmit: *mut [u8],
}

impl TcpPlaces {
    /// Where `memory` is: its slices end here.
    pub fn of(memory: TcpMemory) -> TcpPlaces {
        let TcpMemory { receive, transmit } = memory;
        TcpPlaces { receive: ptr::from_mut(receive), transmit: ptr::from_mut(transmit) }
    }
}

/// Where a UDP socket's memory is once it came back, as [`TcpPlaces`].
pub struct UdpPlaces {
    receive_meta: *mut [udp::PacketMetadata],
    receive: *mut [u8],
    transmit_meta: *mut [udp::PacketMetadata],
    transmit: *mut [u8],
}

impl UdpPlaces {
    /// Where `memory` is: its slices end here.
    pub fn of(memory: UdpMemory) -> UdpPlaces {
        let UdpMemory { receive_meta, receive, transmit_meta, transmit } = memory;
        UdpPlaces {
            receive_meta: ptr::from_mut(receive_meta),
            receive: ptr::from_mut(receive),
            transmit_meta: ptr::from_mut(transmit_meta),
            transmit: ptr::from_mut(transmit),
        }
    }
}

/// Takes the bytes `lease` lent back from `place`, where they came back to,
/// and keeps them for another socket: false if they were not where they
/// were lent, and stay lent.
///
/// # Safety
///
/// As for `Lease::take_back`: `place` is where the socket's memory came
/// back to, as the return value of the call that took the socket apart.
#[allow(unsafe_code)]
unsafe fn take_back(lease: Lease<u8, Kmalloc>, place: *mut [u8]) -> bool {
    // SAFETY: as the caller says.
    match unsafe { lease.take_back(place) } {
        Ok(buffer) => {
            keep(buffer);
            true
        }
        Err(_) => false,
    }
}

impl TcpLeases {
    /// Takes the memory back from where it came back to: false if some of
    /// it was not where it was lent, and stays lent.
    #[allow(unsafe_code)]
    pub fn end(self, places: &TcpPlaces) -> bool {
        // SAFETY: `places` is where the socket's memory came back to, as
        // the return value of the call that took the socket apart, turned
        // into raw pointers by `TcpPlaces::of` and not used otherwise:
        // nothing else reaches the memory, and no call still running has
        // it as an argument. `take_back` checks each is where it was lent.
        let ended = unsafe {
            [take_back(self.receive, places.receive), take_back(self.transmit, places.transmit)]
        };
        ended.into_iter().all(|ended| ended)
    }
}

impl UdpLeases {
    /// Takes the memory back from where it came back to, as
    /// [`TcpLeases::end`] does.
    #[allow(unsafe_code)]
    pub fn end(self, places: &UdpPlaces) -> bool {
        // SAFETY: as in `TcpLeases::end`, from `UdpPlaces::of`.
        let ended = unsafe {
            [
                self.receive_meta.end(places.receive_meta).is_ok(),
                take_back(self.receive, places.receive),
                self.transmit_meta.end(places.transmit_meta).is_ok(),
                take_back(self.transmit, places.transmit),
            ]
        };
        ended.into_iter().all(|ended| ended)
    }
}

/// Takes the TCP socket `handle` out of `stack`, the interface `name`'s,
/// and its memory back through `leases`. Memory that does not come back
/// stays lent, which leaks it, and is said.
pub fn end_tcp(stack: &mut Stack<Card>, handle: SocketHandle, leases: TcpLeases, name: &str) {
    let ended = match stack.remove_tcp(handle) {
        Some(memory) => leases.end(&TcpPlaces::of(memory)),
        None => false,
    };
    if !ended {
        warn!("Net: {name}: a TCP socket's memory did not come back");
    }
}

/// Takes the UDP socket `handle` out of `stack`, as [`end_tcp`] does.
pub fn end_udp(stack: &mut Stack<Card>, handle: SocketHandle, leases: UdpLeases, name: &str) {
    let ended = match stack.remove_udp(handle) {
        Some(memory) => leases.end(&UdpPlaces::of(memory)),
        None => false,
    };
    if !ended {
        warn!("Net: {name}: a UDP socket's memory did not come back");
    }
}

/// Takes back the memory of `socket`, a TCP socket no stack took.
pub fn give_back_tcp(leases: TcpLeases, socket: radix_net::smoltcp::socket::tcp::Socket<'static>) {
    if !leases.end(&TcpPlaces::of(TcpMemory::of(socket))) {
        warn!("Net: a TCP socket's memory did not come back");
    }
}

/// Takes back the memory of `socket`, a UDP socket no stack took.
pub fn give_back_udp(leases: UdpLeases, socket: udp::Socket<'static>) {
    if !leases.end(&UdpPlaces::of(UdpMemory::of(socket))) {
        warn!("Net: a UDP socket's memory did not come back");
    }
}
