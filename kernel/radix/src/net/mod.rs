// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Networking: the network cards and the TCP/IP stack on each (ADR 0180),
//! and the loopback interface (ADR 0183).
//!
//! The PCI scan hands each virtio network card to [`bind`], which sets
//! it up (`radix-virtio`), puts a stack on it (`radix-net`) and starts
//! its thread, `net0`, `net1` and on. The thread does all of the stack's
//! work: it polls the stack when the card's interrupt wakes it and when
//! the stack's next timer is due, and otherwise sleeps. Others reach the
//! stack through its lock, and wake the thread when they gave it work.
//! The loopback interface, `lo`, is one too, on a link that hands back
//! what is sent on it, at 127.0.0.1/8 and every card's own address: what
//! a program sends to the machine itself goes through it. Its stack is
//! polled at once by whoever gave it work, as Linux's loopback runs in
//! the sender's context: a packet a program sends to itself is received
//! before the call that sent it returns, and `lo`'s thread only wakes for
//! the stack's timers.
//!
//! A card starts with no address, or for the first card the IPv4
//! address and router of the `ip=` and `gateway=` flags; ip(8), and
//! the DHCP client to come, set them through the network capability's control right
//! ([`configure`], ADR 0190). Each interface is numbered as Linux numbers
//! them, `lo` 1 and the cards from 2 ([`Interface::index`]);
//! `/kernel/class/net` and `/processes/net` show them.
//!
//! When a poll may have changed what a socket can do, the thread bumps
//! one count for all of networking and wakes those who wait on it, and
//! the pollers of every network socket: they look again
//! ([`generation`], [`changed`]).
//!
//! `selftest=net` pings the router from the first card (QEMU's user
//! network answers at 10.0.2.2, which is what it uses without flags).
//!
//! A stack's own buffers live as long as it does. A socket's are lent to
//! it through leases (`radix_mm::lease`), and come back when the socket
//! is taken apart (ADR 0182). A TCP socket a program closes before its
//! connection has ended is an [`Orphan`] of its interface, which the
//! thread takes apart once the connection has gone.

pub mod memory;
pub mod ports;
pub mod proc;

use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::ptr;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use radix_hal::Devices;
use radix_hw::pci::{Bar, Function, MsiX};
use radix_klog::{TextBuf, error, info, warn};
use radix_mm::lease::{self, Lease};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_net::smoltcp::iface::{SocketHandle, SocketStorage};
use radix_net::smoltcp::socket::{AnySocket, icmp, tcp};
use radix_net::{
    FRAME, Ipv4Address, Ipv4Cidr, Link, Loopback, Medium, Memory, PingMemory, Pinger, Stack,
};
use radix_sync::{Mutex, Once};
use radix_virtio::net::{MAX_FRAME, Net};
use radix_virtio::{NO_VECTOR, Transport};

use crate::Arch;
use crate::sched::{self, Options, Policy, ThreadRef, wait};
use crate::user::posix::poll::Polled;

/// The most network cards.
const MAX_CARDS: usize = 8;
/// The most interfaces: the loopback one, and the cards.
pub const MAX_INTERFACES: usize = MAX_CARDS + 1;
/// The sockets an interface has slots for at first. When every slot is
/// taken it makes twice as many, up to [`MAX_SOCKETS`] (ADR 0188).
const SOCKETS: usize = 128;
/// The most sockets on one interface: 16,384 slots of 688 bytes, 11 MiB.
const MAX_SOCKETS: usize = 16_384;
/// How long the thread sleeps when nothing is due: an interrupt wakes
/// it sooner, and a card without one is polled this often.
const IDLE_NS: u64 = 10_000_000;
/// The loopback interface's MTU, the longest packet a program's TCP sends
/// to itself at once: the most IPv4 allows, as on Linux, whose `lo` has
/// 65,536.
const LOOPBACK_MTU: usize = 65_535;
/// The bytes of packets the loopback interface holds until its stack
/// takes them back: two of the longest, two windows of a socket's
/// (`TCP_BUFFER`), or one and the ACKs that answer it.
const LOOPBACK_RING: usize = Loopback::ring_for(2, LOOPBACK_MTU);
/// The loopback interface's own address.
pub const LOOPBACK: Ipv4Address = Ipv4Address::new(127, 0, 0, 1);
/// How long an orphan has to finish closing before it is reset.
pub const ORPHAN_NS: u64 = 60_000_000_000;

/// What an interface's stack sends and receives through: a virtio network
/// card, or the loopback link.
#[expect(clippy::large_enum_variant, reason = "each is made once, in its interface's place")]
pub enum Card {
    Virtio { net: Net, vector: Option<u32> },
    Loopback(Loopback),
}

impl Card {
    /// Whether the card stopped with an error.
    pub fn failed(&self) -> bool {
        match self {
            Card::Virtio { net, .. } => net.failed(),
            Card::Loopback(_) => false,
        }
    }
}

impl Link for Card {
    fn medium(&self) -> Medium {
        match self {
            Card::Virtio { .. } => Medium::Ethernet,
            Card::Loopback(link) => link.medium(),
        }
    }

    fn pending(&self) -> bool {
        match self {
            Card::Virtio { .. } => false,
            Card::Loopback(link) => link.pending(),
        }
    }

    fn mac(&self) -> [u8; 6] {
        match self {
            Card::Virtio { net, .. } => net.info().mac,
            Card::Loopback(link) => link.mac(),
        }
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        match self {
            Card::Virtio { net, .. } => net.receive(frame),
            Card::Loopback(link) => link.receive(frame),
        }
    }

    fn can_send(&mut self) -> bool {
        match self {
            Card::Virtio { net, .. } => net.can_send(),
            Card::Loopback(link) => link.can_send(),
        }
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        match self {
            Card::Virtio { net, .. } => frame.len() <= MAX_FRAME && net.send(frame).is_ok(),
            Card::Loopback(link) => link.send(frame),
        }
    }

    fn flush(&mut self) {
        match self {
            Card::Virtio { net, .. } => net.notify(),
            Card::Loopback(link) => link.flush(),
        }
    }

    fn needs_checksums(&self) -> bool {
        match self {
            Card::Virtio { .. } => true,
            Card::Loopback(link) => link.needs_checksums(),
        }
    }

    fn loopback(&mut self) -> Option<&mut Loopback> {
        match self {
            Card::Virtio { .. } => None,
            Card::Loopback(link) => link.loopback(),
        }
    }
}

/// Only a card whose setup failed is dropped: the card stops, and its
/// interrupt is let go.
impl Drop for Card {
    fn drop(&mut self) {
        if let Card::Virtio { net, vector } = self {
            net.reset();
            if let Some(vector) = *vector {
                <Arch as Devices>::free_interrupt(vector);
            }
        }
    }
}

/// A TCP socket a program closed while its connection was still ending:
/// its interface takes it out of the stack, and its memory back, once the
/// connection has gone (on the loopback link, once it reached TIME_WAIT),
/// or resets it at `deadline_ns`.
pub struct Orphan {
    pub handle: SocketHandle,
    /// The leases of its memory, until it gives the memory back: on a
    /// card, at once in TIME-WAIT.
    pub leases: Option<memory::TcpLeases>,
    /// The port the kernel picked for it, held until it goes.
    #[expect(dead_code, reason = "held only to keep the port taken")]
    pub port: Option<ports::Held>,
    pub deadline_ns: u64,
}

/// One interface: its stack and the thread that runs it.
pub struct Interface {
    /// `lo`, `net0`, `net1` and on.
    pub name: TextBuf<8>,
    /// Its place among the interfaces: 0 for `lo`, then the cards in the
    /// order they were found.
    pub number: usize,
    pub stack: Mutex<Stack<Card>>,
    /// Its orphans, with room for as many as it has socket slots. Taken
    /// with the stack's lock held.
    orphans: Mutex<Vec<Orphan, Kmalloc>>,
    /// The lease of its stack's socket slots, which come back when the
    /// stack moves to more. Taken with the stack's lock held.
    slots: Mutex<Lease<SocketStorage<'static>, Kmalloc>>,
    thread: Once<ThreadRef>,
    /// When its thread is next to wake by itself, by the clock of
    /// `sched::now_ns` (`u64::MAX`: only when woken), set with the stack's
    /// lock held: the loopback interface's poller wakes it sooner when a
    /// timer is due before then.
    wake_at: AtomicU64,
}

impl Interface {
    /// Gives its stack the work a caller just queued on it: the loopback
    /// interface does it at once, in the caller's thread; a card's thread
    /// is woken to do it. Called without its stack's lock.
    pub fn kick(&self) {
        if self.is_loopback() {
            self.poll_here();
        } else {
            self.wake();
        }
    }

    /// Wakes the interface's thread, to poll its stack.
    fn wake(&self) {
        if let Some(thread) = self.thread.get() {
            sched::wake(thread);
        }
    }

    /// Polls its stack in the calling thread until what was sent on it has
    /// been received, takes its orphans that have gone, and wakes its
    /// thread if the stack has a timer due before the thread would wake.
    fn poll_here(&self) {
        let (changed, due, wake_at) = {
            let mut stack = self.stack.lock();
            let now = sched::now_ns();
            let changed = stack.poll(now);
            let orphans = self.reap(&mut stack, now);
            let delay = stack.poll_delay(now).map(|delay| now.saturating_add(delay));
            let due = [delay, orphans].into_iter().flatten().min();
            (changed, due, self.wake_at.load(Ordering::Relaxed))
        };
        if changed {
            self::changed();
        }
        if due.is_some_and(|due| due < wake_at) {
            self.wake();
        }
    }

    /// Whether it is the loopback interface.
    pub fn is_loopback(&self) -> bool {
        self.number == 0
    }

    /// Its number as Linux numbers interfaces, its `ifindex`: 1 for `lo`,
    /// then the cards from 2.
    pub fn index(&self) -> u32 {
        // At most MAX_INTERFACES.
        self.number as u32 + 1
    }

    /// Adds `socket` to `stack`, its own, locked, making room for it if
    /// every slot is taken; or gives it back if there is none to make.
    ///
    /// # Errors
    ///
    /// `socket`, when there is no room for it.
    pub fn add_socket<S: AnySocket<'static>>(
        &self,
        stack: &mut Stack<Card>,
        socket: S,
    ) -> Result<SocketHandle, S> {
        match stack.add_socket(socket) {
            Err(socket) if self.make_room(stack) => stack.add_socket(socket),
            added => added,
        }
    }

    /// Makes room on `stack`, its own, locked, for one more socket: twice
    /// the slots, up to [`MAX_SOCKETS`]; with that many, the slot of its
    /// oldest orphan in TIME-WAIT, which ends at once, as Linux ends one
    /// past `tcp_max_tw_buckets`. False if there is no room to make.
    fn make_room(&self, stack: &mut Stack<Card>) -> bool {
        let slots = stack.socket_slots();
        (slots < MAX_SOCKETS && self.grow(stack, (slots * 2).min(MAX_SOCKETS)))
            || self.end_oldest_time_wait(stack)
    }

    /// Moves `stack`'s sockets to `len` slots, more than it has, and takes
    /// the memory of the slots they were in back: false if there is no
    /// memory for them.
    #[allow(unsafe_code)]
    fn grow(&self, stack: &mut Stack<Card>, len: usize) -> bool {
        {
            // Room for an orphan in each slot, as `adopt` counts on.
            let mut orphans = self.orphans.lock();
            let more = len.saturating_sub(orphans.len());
            if orphans.try_reserve_exact(more).is_err() {
                return false;
            }
        }
        let Some((lease, slots)) = slots(len) else { return false };
        match stack.grow_sockets(slots) {
            Ok(old) => {
                let old = ptr::from_mut(old);
                let old_lease = core::mem::replace(&mut *self.slots.lock(), lease);
                // SAFETY: `old` is the slots the stack had, given back as the
                // return value of `grow_sockets` and turned into a raw pointer
                // at once: nothing else reaches them. `end` checks they are
                // where they were lent.
                if unsafe { old_lease.end(old) }.is_err() {
                    warn!("Net: {}: its old socket slots did not come back", self.name);
                }
                info!("Net: {} has room for {len} sockets", self.name);
                true
            }
            Err(slots) => {
                // SAFETY: as above: `slots` came back as `grow_sockets`'
                // return value.
                if unsafe { lease.end(ptr::from_mut(slots)) }.is_err() {
                    warn!("Net: {}: socket slots did not come back", self.name);
                }
                false
            }
        }
    }

    /// Ends the oldest of its orphans in TIME-WAIT on `stack`, its own,
    /// locked, which frees its slot: false if none is.
    fn end_oldest_time_wait(&self, stack: &mut Stack<Card>) -> bool {
        let mut orphans = self.orphans.lock();
        let oldest = orphans
            .iter()
            .enumerate()
            .filter(|(_, orphan)| {
                stack
                    .socket_mut::<tcp::Socket<'static>>(orphan.handle)
                    .is_some_and(|socket| socket.state() == tcp::State::TimeWait)
            })
            .min_by_key(|(_, orphan)| orphan.deadline_ns)
            .map(|(index, _)| index);
        let Some(index) = oldest else { return false };
        let orphan = orphans.swap_remove(index);
        drop(orphans);
        end_orphan(stack, orphan, self.name.as_str());
        true
    }

    /// Takes `orphan`, on `stack`, its own, locked, to finish closing.
    pub fn adopt(&self, _stack: &mut Stack<Card>, orphan: Orphan) {
        let mut orphans = self.orphans.lock();
        // There is room for every socket slot, and the orphan has one.
        if let Err(lost) = orphans.push_within_capacity(orphan) {
            drop(orphans);
            warn!("Net: {}: no room for an orphan; its memory stays lent", self.name);
            core::mem::forget(lost);
        }
    }

    /// Takes the orphans whose connections have gone out of `stack`, and
    /// their memory back; resets those past their deadline. Returns the
    /// next deadline of those left.
    fn reap(&self, stack: &mut Stack<Card>, now_ns: u64) -> Option<u64> {
        let mut orphans = self.orphans.lock();
        let mut next: Option<u64> = None;
        let mut index = 0;
        while index < orphans.len() {
            let orphan = &mut orphans[index];
            let state = stack.socket_mut::<tcp::Socket<'static>>(orphan.handle).map(|s| s.state());
            // On the loopback link a connection in TIME_WAIT goes at once,
            // with its memory and socket slot, rather than holding them for
            // its ten seconds, which short connections to the machine itself
            // would run out of: the FIN it acknowledged had its ACK at once
            // (the smoltcp patch), and the link loses and delays nothing, so
            // there is no stray segment for TIME_WAIT to wait out.
            let gone = match state {
                Some(tcp::State::Closed) | None => true,
                Some(tcp::State::TimeWait) => self.is_loopback(),
                Some(_) => false,
            };
            if gone {
                let orphan = orphans.swap_remove(index);
                end_orphan(stack, orphan, self.name.as_str());
                continue;
            }
            // On a card, it keeps only its slot through TIME-WAIT, as Linux
            // keeps a small record of its own: what it sent is acknowledged,
            // and what it received no one will read (ADR 0188).
            if state == Some(tcp::State::TimeWait)
                && let Some(leases) = orphan.leases.take()
            {
                memory::shed_tcp(stack, orphan.handle, leases, self.name.as_str());
            }
            if now_ns >= orphan.deadline_ns {
                // Closed now; the stack sends the reset, and the next look
                // takes it apart.
                if let Some(socket) = stack.socket_mut::<tcp::Socket<'static>>(orphan.handle) {
                    socket.abort();
                }
            } else {
                next = Some(next.map_or(orphan.deadline_ns, |at| at.min(orphan.deadline_ns)));
            }
            index += 1;
        }
        next
    }
}

/// Takes `orphan` out of `stack`, the interface `name`'s, and its memory
/// back, if it still has any.
fn end_orphan(stack: &mut Stack<Card>, orphan: Orphan, name: &str) {
    match orphan.leases {
        Some(leases) => memory::end_tcp(stack, orphan.handle, leases, name),
        // Its buffers are empty: nothing lent is left in it.
        None => drop(stack.remove_tcp(orphan.handle)),
    }
}

/// The loopback interface, made by [`init`].
static LO: Once<Interface> = Once::new();
/// The cards' interfaces, in the order the cards were found.
static INTERFACES: [Once<Interface>; MAX_CARDS] = [const { Once::new() }; MAX_CARDS];
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// Every interface: `lo` first, then the cards.
pub fn interfaces() -> impl Iterator<Item = &'static Interface> {
    let cards = COUNT.load(Ordering::Acquire).min(MAX_CARDS);
    LO.get().into_iter().chain(INTERFACES[..cards].iter().filter_map(Once::get))
}

/// The interface numbered `index`, as Linux numbers them (`ifindex`).
pub fn by_index(index: u32) -> Option<&'static Interface> {
    index.checked_sub(1).and_then(|number| numbered(number as usize))
}

/// Why [`configure`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigureError {
    /// No interface has that number.
    NoInterface,
    /// It is `lo`, whose addresses are its own and the cards'.
    Loopback,
}

/// Gives the interface numbered `index` `addresses`, the first its own,
/// and `router` as its default route, or none: radix-network(2)'s
/// `configure`, through its control right. `lo` takes the addresses as
/// its own, so that a packet to them comes back to the machine; the
/// sockets that wait on networking look again.
///
/// # Errors
///
/// When there is no such interface, or it is `lo`.
pub fn configure(
    index: u32,
    addresses: &[Ipv4Cidr],
    router: Option<Ipv4Address>,
) -> Result<(), ConfigureError> {
    let interface = by_index(index).ok_or(ConfigureError::NoInterface)?;
    if interface.is_loopback() {
        return Err(ConfigureError::Loopback);
    }
    interface.stack.lock().configure(addresses, router);
    addresses_changed();
    info!(
        "Net: {} is set to {} address{}{}",
        interface.name,
        addresses.len(),
        if addresses.len() == 1 { "" } else { "es" },
        if router.is_some() { ", with a router" } else { "" },
    );
    interface.kick();
    changed();
    Ok(())
}

/// The interface a packet to `to` goes out through: `lo` for the
/// machine's own addresses, else the first card on `to`'s network, else
/// the first with a router.
pub fn route(to: Ipv4Address) -> Option<&'static Interface> {
    if let Some(lo) = LO.get()
        && lo.stack.lock().reaches(to)
    {
        return Some(lo);
    }
    let mut cards = interfaces().filter(|interface| !interface.is_loopback());
    let mut routed = None;
    for interface in cards.by_ref() {
        let stack = interface.stack.lock();
        if stack.reaches(to) {
            return Some(interface);
        }
        if routed.is_none() && stack.routes_to(to) {
            routed = Some(interface);
        }
    }
    routed
}

/// What networking has done so far, counted: what socket calls wait on.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// The pollers of every network socket.
pub static POLLED: Polled = Polled::new();

/// The key socket calls wait on.
pub fn key() -> usize {
    ptr::from_ref(&GENERATION) as usize
}

/// The count of changes so far: a socket call that finds it cannot go on
/// waits until it moves.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// What a socket can do may have changed: those who wait look again, and
/// so do the pollers.
pub fn changed() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
    wait::wake_all(key());
    POLLED.changed();
}

/// The address and router of the `ip=` and `gateway=` flags.
static FLAGS: Once<(Option<Ipv4Cidr>, Option<Ipv4Address>)> = Once::new();

/// Reads the `ip=` and `gateway=` flags, before the PCI scan, and starts
/// the loopback interface.
pub fn init(ip: Option<&str>, gateway: Option<&str>) {
    let address = ip.and_then(|text| {
        let parsed = radix_net::parse_cidr(text);
        if parsed.is_none() {
            warn!("Net: ip={text} is not an IPv4 address with a prefix, such as 10.0.2.15/24");
        }
        parsed
    });
    let router = gateway.and_then(|text| {
        let parsed = text.parse::<Ipv4Address>().ok();
        if parsed.is_none() {
            warn!("Net: gateway={text} is not an IPv4 address");
        }
        parsed
    });
    let _ = FLAGS.set((address, router));
    if let Err(why) = start_loopback() {
        error!("Net: the loopback interface could not start: {why}");
    }
}

/// Starts `lo`.
fn start_loopback() -> Result<(), &'static str> {
    let ring = forever(LOOPBACK_RING).ok_or("out of memory")?;
    let card = Card::Loopback(Loopback::new(ring, LOOPBACK_MTU));
    let (slots, sockets) = slots(SOCKETS).ok_or("out of memory")?;
    let memory = Memory {
        receive: forever(LOOPBACK_MTU).ok_or("out of memory")?,
        transmit: forever(LOOPBACK_MTU).ok_or("out of memory")?,
        sockets,
    };
    let mut stack = Stack::new(card, memory, radix_random::u64(), sched::now_ns());
    stack.set_ipv4_addresses(&[Ipv4Cidr::new(LOOPBACK, 8)]);
    let mut name = TextBuf::<8>::new();
    let _ = name.write_str("lo");
    let interface = interface(name, 0, stack, slots).ok_or("out of memory")?;
    if LO.set(interface).is_err() {
        return Err("it was started already");
    }
    let lo = LO.get().ok_or("it is not there")?;
    info!("Net: lo is the loopback interface, at {LOOPBACK}/8 and every card's address");
    run_thread(lo)
}

/// An interface named `name`, numbered `number`, of `stack`, whose socket
/// slots `slots` lent.
fn interface(
    name: TextBuf<8>,
    number: usize,
    stack: Stack<Card>,
    slots: Lease<SocketStorage<'static>, Kmalloc>,
) -> Option<Interface> {
    let mut orphans = Vec::new_in(kmalloc());
    orphans.try_reserve_exact(stack.socket_slots()).ok()?;
    Some(Interface {
        name,
        number,
        stack: Mutex::new(stack),
        orphans: Mutex::new(orphans),
        slots: Mutex::new(slots),
        thread: Once::new(),
        wake_at: AtomicU64::new(u64::MAX),
    })
}

/// Starts `interface`'s thread.
fn run_thread(interface: &'static Interface) -> Result<(), &'static str> {
    let options = Options::new().held().policy(Policy::Fair { nice: -5 });
    match sched::spawn(interface.name.as_str(), run, interface.number, options) {
        Ok(thread) => {
            let _ = interface.thread.set(thread.clone());
            sched::release(&thread);
            Ok(())
        }
        Err(_) => Err("its thread could not start"),
    }
}

/// The most addresses `lo` has: its own and the cards', as many as fit.
const LOOPBACK_ADDRESSES: usize = 8;

/// Gives `lo` the cards' addresses with its own, after a card's changed:
/// each card's first, then their others, while there is room.
fn addresses_changed() {
    let Some(lo) = LO.get() else { return };
    let mut addresses = [Ipv4Cidr::new(LOOPBACK, 8); LOOPBACK_ADDRESSES];
    let mut count = 1;
    for first in [true, false] {
        for interface in interfaces().filter(|interface| !interface.is_loopback()) {
            let stack = interface.stack.lock();
            let (skip, take) = if first { (0, 1) } else { (1, usize::MAX) };
            for cidr in stack.ipv4_addresses().skip(skip).take(take) {
                if count == LOOPBACK_ADDRESSES {
                    break;
                }
                addresses[count] = Ipv4Cidr::new(cidr.address(), 32);
                count += 1;
            }
        }
    }
    lo.stack.lock().set_ipv4_addresses(&addresses[..count]);
}

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

/// `len` zero bytes from the heap, for as long as the kernel runs.
fn forever(len: usize) -> Option<&'static mut [u8]> {
    radix_mm::bytes::zeroed(len, kmalloc()).ok().map(Vec::leak)
}

/// `len` values made by `make`, on the heap.
fn values<T>(make: fn() -> T, len: usize) -> Option<Vec<T, Kmalloc>> {
    let mut items = Vec::new_in(kmalloc());
    items.try_reserve_exact(len).ok()?;
    while items.len() < len && items.push_within_capacity(make()).is_ok() {}
    Some(items)
}

/// Slots for `len` sockets, lent from the heap.
fn slots(
    len: usize,
) -> Option<(Lease<SocketStorage<'static>, Kmalloc>, &'static mut [SocketStorage<'static>])> {
    values(|| SocketStorage::EMPTY, len).map(lease::lend)
}

/// The leases of a pinger's memory, which comes back when it closes.
struct PingLeases {
    receive_meta: Lease<icmp::PacketMetadata, Kmalloc>,
    receive: Lease<u8, Kmalloc>,
    transmit_meta: Lease<icmp::PacketMetadata, Kmalloc>,
    transmit: Lease<u8, Kmalloc>,
}

/// Where a pinger's memory is once it came back, as raw pointers: what
/// it was lent as is not used again.
struct PingPlaces {
    receive_meta: *mut [icmp::PacketMetadata],
    receive: *mut [u8],
    transmit_meta: *mut [icmp::PacketMetadata],
    transmit: *mut [u8],
}

impl PingPlaces {
    /// Where `memory` is: its slices end here.
    fn of(memory: PingMemory) -> PingPlaces {
        let PingMemory { receive_meta, receive, transmit_meta, transmit } = memory;
        PingPlaces {
            receive_meta: ptr::from_mut(receive_meta),
            receive: ptr::from_mut(receive),
            transmit_meta: ptr::from_mut(transmit_meta),
            transmit: ptr::from_mut(transmit),
        }
    }
}

impl PingLeases {
    /// Memory for a pinger, lent from the heap: room for four echo
    /// messages of up to 128 bytes each way.
    fn lend() -> Option<(PingLeases, PingMemory)> {
        let receive_meta = values(|| icmp::PacketMetadata::EMPTY, 4)?;
        let receive = radix_mm::bytes::zeroed(512, kmalloc()).ok()?;
        let transmit_meta = values(|| icmp::PacketMetadata::EMPTY, 4)?;
        let transmit = radix_mm::bytes::zeroed(512, kmalloc()).ok()?;
        let (receive_meta_lease, receive_meta) = lease::lend(receive_meta);
        let (receive_lease, receive) = lease::lend(receive);
        let (transmit_meta_lease, transmit_meta) = lease::lend(transmit_meta);
        let (transmit_lease, transmit) = lease::lend(transmit);
        let leases = PingLeases {
            receive_meta: receive_meta_lease,
            receive: receive_lease,
            transmit_meta: transmit_meta_lease,
            transmit: transmit_lease,
        };
        Some((leases, PingMemory { receive_meta, receive, transmit_meta, transmit }))
    }

    /// Takes the memory back, from where it came back to: false if some of
    /// it was not where it was lent, and stays lent.
    #[allow(unsafe_code)]
    fn end(self, places: &PingPlaces) -> bool {
        // SAFETY: `places` is where the pinger's memory came back to, as
        // the return value of `Pinger::new` or `Pinger::close`, turned into
        // raw pointers by `PingPlaces::of` and not used otherwise: nothing
        // else reaches the memory, and no call still running has it as an
        // argument. `end` checks each is where it was lent.
        let ended = unsafe {
            [
                self.receive_meta.end(places.receive_meta).is_ok(),
                self.receive.end(places.receive).is_ok(),
                self.transmit_meta.end(places.transmit_meta).is_ok(),
                self.transmit.end(places.transmit).is_ok(),
            ]
        };
        ended.into_iter().all(|ended| ended)
    }
}

fn on_interrupt(index: usize) {
    if let Some(interface) = INTERFACES.get(index).and_then(Once::get) {
        interface.kick();
    }
}

/// Sets up the virtio network card `function`. Its frame holds the stack
/// before it goes in place: kept out of the PCI scan's
/// (tools/xtask/src/frames.rs allows it).
#[inline(never)]
pub fn bind(function: &Function) {
    let index = COUNT.load(Ordering::Relaxed);
    if index >= MAX_CARDS {
        warn!("Net: too many network cards; the one at {} is left alone", function.address());
        return;
    }
    if let Err(why) = start(function, index) {
        warn!("Net: could not set up the virtio network card at {}: {why}", function.address());
    }
}

/// Why a card could not be set up.
enum Failure {
    /// The card's driver refused it.
    Card(radix_virtio::Error),
    Other(&'static str),
}

impl From<&'static str> for Failure {
    fn from(why: &'static str) -> Failure {
        Failure::Other(why)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Card(err) => write!(f, "{err}"),
            Failure::Other(why) => f.write_str(why),
        }
    }
}

fn start(function: &Function, index: usize) -> Result<(), Failure> {
    function.enable_memory_and_dma();
    let mut map_bar = |_: usize, bar: Bar| match bar {
        Bar::Memory { address, size, .. } => {
            <Arch as Devices>::map_device(address, usize::try_from(size).ok()?)
        }
        Bar::Io { .. } => None,
    };
    let transport = Transport::find(function, &mut map_bar).map_err(Failure::Card)?;
    // One MSI-X entry, for both queues, if the card and the kernel have
    // them; otherwise the card is polled.
    let interrupt = MsiX::find(function).and_then(|msix| {
        let (bar, offset) = msix.table_location();
        let Some(Bar::Memory { address, .. }) = function.bar(bar) else { return None };
        let table = <Arch as Devices>::map_device(address + offset, msix.table_len())?;
        let (vector, message) =
            <Arch as Devices>::allocate_interrupt(function.address(), on_interrupt, index)?;
        msix.enable_masked(function);
        msix.set_entry(&table, 0, message);
        msix.unmask_all(function);
        Some(vector)
    });
    let entry = if interrupt.is_some() { 0 } else { NO_VECTOR };
    // A random, locally administered, unicast address, for a card that
    // has none of its own.
    let mut fallback = [0u8; 6];
    radix_random::fill(&mut fallback);
    fallback[0] = (fallback[0] & !0x01) | 0x02;
    let mut alloc = |pages: usize| <Arch as Devices>::alloc_dma(function.address(), pages, false);
    let net = Net::new(transport, entry, fallback, &mut alloc).map_err(|err| {
        if let Some(vector) = interrupt {
            <Arch as Devices>::free_interrupt(vector);
        }
        Failure::Card(err)
    })?;
    // From here a failure drops the card, which stops it.
    let info = net.info();
    let card = Card::Virtio { net, vector: interrupt };
    let (slots, sockets) = slots(SOCKETS).ok_or("out of memory")?;
    let memory = Memory {
        receive: forever(FRAME).ok_or("out of memory")?,
        transmit: forever(FRAME).ok_or("out of memory")?,
        sockets,
    };
    let mut stack = Stack::new(card, memory, radix_random::u64(), sched::now_ns());
    let flags = FLAGS.get().copied().unwrap_or((None, None));
    if index == 0
        && let (Some(address), gateway) = flags
    {
        stack.set_ipv4(address, gateway);
    }
    let mut name = TextBuf::<8>::new();
    let _ = write!(name, "net{index}");
    let interface =
        interface(name.clone(), index + 1, stack, slots).ok_or(Failure::Other("out of memory"))?;
    if INTERFACES[index].set(interface).is_err() {
        return Err(Failure::Other("its slot is taken"));
    }
    COUNT.store(index + 1, Ordering::Release);
    let Some(interface) = INTERFACES[index].get() else {
        return Err(Failure::Other("its slot is empty"));
    };
    let mac = info.mac;
    let mut how = TextBuf::<24>::new();
    let _ = match interrupt {
        Some(vector) => write!(how, "MSI-X vector {vector:#x}"),
        None => how.write_str("polled"),
    };
    info!(
        "Net: {name} is a virtio network card at {}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, \
         {} receive and {} transmit slots, {how}",
        function.address(),
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        info.receive_slots,
        info.transmit_slots,
    );
    let address = interface.stack.lock().ipv4();
    if let Some(address) = address {
        match flags.1 {
            Some(gateway) if index == 0 => {
                info!("Net: {name} has the address {address}, through the router {gateway}");
            }
            _ => info!("Net: {name} has the address {address}"),
        }
        addresses_changed();
    }
    run_thread(interface).map_err(Failure::Other)
}

/// The interface numbered `number`: `lo`, or a card's.
fn numbered(number: usize) -> Option<&'static Interface> {
    match number {
        0 => LO.get(),
        n => INTERFACES.get(n - 1).and_then(Once::get),
    }
}

/// An interface's thread: polls its stack whenever there may be work,
/// and tells socket calls when that may have changed what they can do.
fn run(number: usize) {
    let Some(interface) = numbered(number) else { return };
    // A card is looked at this often even when nothing is due, in case
    // it has no interrupt; the loopback interface is polled by whoever
    // sends on it, and its thread only wakes for its timers.
    let idle = (!interface.is_loopback()).then_some(IDLE_NS);
    loop {
        let (wake_at, changed) = {
            let mut stack = interface.stack.lock();
            let now = sched::now_ns();
            let changed = stack.poll(now);
            if stack.link().failed() {
                error!("Net: {}: the card stopped with an error", interface.name);
                return;
            }
            let orphans = interface.reap(&mut stack, now);
            let now = sched::now_ns();
            let delay = [stack.poll_delay(now), idle].into_iter().flatten().min();
            let wake_at =
                [delay.map(|delay| now.saturating_add(delay)), orphans].into_iter().flatten().min();
            // Under the stack's lock, so that a poller that sets a sooner
            // timer after this sees when the thread means to wake.
            interface.wake_at.store(wake_at.unwrap_or(u64::MAX), Ordering::Relaxed);
            (wake_at, changed)
        };
        if changed {
            self::changed();
        }
        match wake_at {
            Some(at) if at <= sched::now_ns() => sched::yield_now(),
            Some(at) => sched::block_until(at, "net"),
            None => sched::block("net"),
        }
    }
}

/// The first interface, if a card was found.
fn first() -> Option<&'static Interface> {
    INTERFACES[0].get()
}

/// How many echo requests `selftest=net` sends.
const PINGS: u16 = 4;
/// How long it waits for each reply.
const PING_WAIT_NS: u64 = 2_000_000_000;

/// `selftest=net`: pings the router from the first interface, giving it
/// QEMU's user-network address and router first if it has no address.
pub fn selftest() {
    match ping_router() {
        Ok(fastest) => info!(
            "selftest net: passed; the router answered {PINGS} pings, the fastest in {}.{:03} ms",
            fastest / 1_000_000,
            fastest / 1_000 % 1_000
        ),
        Err(why) => error!("selftest net: failed; {why}"),
    }
}

/// Pings the router with a pinger lent memory for the test, and takes
/// the memory back after: the test fails if it does not come back.
fn ping_router() -> Result<u64, &'static str> {
    let interface = first().ok_or("there is no network card")?;
    let (address, gateway) = match FLAGS.get().copied().unwrap_or((None, None)) {
        (Some(address), Some(gateway)) => (address, gateway),
        _ => (Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24), Ipv4Address::new(10, 0, 2, 2)),
    };
    let (leases, memory) = PingLeases::lend().ok_or("out of memory")?;
    let made = {
        let mut stack = interface.stack.lock();
        if stack.ipv4().is_none() {
            stack.set_ipv4(address, Some(gateway));
            info!(
                "Net: {} has the address {address}, through the router {gateway}",
                interface.name
            );
        }
        Pinger::new(&mut stack, memory, 0x5254)
    };
    addresses_changed();
    let (result, back) = match made {
        Ok(pinger) => {
            let result = pings(interface, &pinger, gateway);
            match pinger.close(&mut interface.stack.lock()) {
                Ok(memory) => (result, PingPlaces::of(memory)),
                Err(_) => return Err("the pinger's socket was not where it was made"),
            }
        }
        Err(memory) => (Err("no socket slot is free"), PingPlaces::of(memory)),
    };
    if !leases.end(&back) {
        return Err("the pinger's memory did not come back");
    }
    result
}

/// Sends PINGS echo requests to `gateway` with `pinger`, one at a time,
/// and returns the fastest reply's round trip, in nanoseconds.
fn pings(
    interface: &Interface,
    pinger: &Pinger,
    gateway: Ipv4Address,
) -> Result<u64, &'static str> {
    let mut fastest = u64::MAX;
    for seq in 1..=PINGS {
        let sent = sched::now_ns();
        pinger
            .send(&mut interface.stack.lock(), gateway, seq)
            .map_err(|_| "the echo request could not be queued")?;
        interface.kick();
        let deadline = sent.saturating_add(PING_WAIT_NS);
        let rtt = loop {
            if let Some((got, from)) = pinger.reply(&mut interface.stack.lock())
                && got == seq
                && from == gateway
            {
                break sched::now_ns().saturating_sub(sent);
            }
            if sched::now_ns() >= deadline {
                return Err("the router did not answer within 2 seconds");
            }
            sched::sleep_ns(1_000_000);
        };
        info!(
            "Net: {gateway} answered ping {seq} in {}.{:03} ms",
            rtt / 1_000_000,
            rtt / 1_000 % 1_000
        );
        fastest = fastest.min(rtt);
    }
    Ok(fastest)
}
