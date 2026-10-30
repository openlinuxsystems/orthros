// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Networking: the network cards and the TCP/IP stack on each (ADR 0180).
//!
//! The PCI scan hands each virtio network card to [`bind`], which sets
//! it up (`radix-virtio`), puts a stack on it (`radix-net`) and starts
//! its thread, `net0`, `net1` and on. The thread does all of the stack's
//! work: it polls the stack when the card's interrupt wakes it and when
//! the stack's next timer is due, and otherwise sleeps. Others reach the
//! stack through its lock, and wake the thread when they gave it work.
//!
//! Until `netd` configures interfaces (12a.4), the first card takes the
//! IPv4 address and router of the `ip=` and `gateway=` flags.
//!
//! `selftest=net` pings the router from the first card (QEMU's user
//! network answers at 10.0.2.2, which is what it uses without flags).

use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering};

use radix_hal::Devices;
use radix_hw::pci::{Bar, Function, MsiX};
use radix_klog::{TextBuf, error, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_net::smoltcp::iface::SocketStorage;
use radix_net::smoltcp::socket::icmp;
use radix_net::{FRAME, Ipv4Address, Ipv4Cidr, Link, Memory, PingMemory, Pinger, Stack};
use radix_sync::{Mutex, Once};
use radix_virtio::net::{MAX_FRAME, Net};
use radix_virtio::{NO_VECTOR, Transport};

use crate::Arch;
use crate::sched::{self, Options, Policy, ThreadRef};

/// The most network cards.
const MAX_CARDS: usize = 8;
/// The most sockets on one interface, until sockets come to programs.
const SOCKETS: usize = 16;
/// How long the thread sleeps when nothing is due: an interrupt wakes
/// it sooner, and a card without one is polled this often.
const IDLE_NS: u64 = 10_000_000;

/// A virtio network card, as the stack's link.
pub struct Card {
    net: Net,
    vector: Option<u32>,
}

impl Link for Card {
    fn mac(&self) -> [u8; 6] {
        self.net.info().mac
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        self.net.receive(frame)
    }

    fn can_send(&mut self) -> bool {
        self.net.can_send()
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        frame.len() <= MAX_FRAME && self.net.send(frame).is_ok()
    }

    fn flush(&mut self) {
        self.net.notify();
    }
}

/// Only a card whose setup failed is dropped: the card stops, and its
/// interrupt is let go.
impl Drop for Card {
    fn drop(&mut self) {
        self.net.reset();
        if let Some(vector) = self.vector {
            <Arch as Devices>::free_interrupt(vector);
        }
    }
}

/// One interface: its stack and the thread that runs it.
pub struct Interface {
    /// `net0`, `net1` and on.
    pub name: TextBuf<8>,
    stack: Mutex<Stack<Card>>,
    thread: Once<ThreadRef>,
}

impl Interface {
    /// Wakes the interface's thread, to poll its stack.
    fn kick(&self) {
        if let Some(thread) = self.thread.get() {
            sched::wake(thread);
        }
    }
}

/// The interfaces, in the order their cards were found.
static INTERFACES: [Once<Interface>; MAX_CARDS] = [const { Once::new() }; MAX_CARDS];
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// The address and router of the `ip=` and `gateway=` flags.
static FLAGS: Once<(Option<Ipv4Cidr>, Option<Ipv4Address>)> = Once::new();

/// Reads the `ip=` and `gateway=` flags, before the PCI scan.
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
}

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

/// `len` zero bytes from the heap, for as long as the kernel runs.
fn forever(len: usize) -> Option<&'static mut [u8]> {
    radix_mm::bytes::zeroed(len, kmalloc()).ok().map(Vec::leak)
}

/// `len` values made by `make` on the heap, for as long as the kernel
/// runs.
fn forever_of<T>(make: fn() -> T, len: usize) -> Option<&'static mut [T]> {
    let mut items = Vec::new_in(kmalloc());
    items.try_reserve_exact(len).ok()?;
    while items.len() < len && items.push_within_capacity(make()).is_ok() {}
    Some(items.leak())
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
    let card = Card { net, vector: interrupt };
    let memory = Memory {
        receive: forever(FRAME).ok_or("out of memory")?,
        transmit: forever(FRAME).ok_or("out of memory")?,
        sockets: forever_of(|| SocketStorage::EMPTY, SOCKETS).ok_or("out of memory")?,
    };
    let info = card.net.info();
    let mut stack = Stack::new(card, memory, radix_random::u64(), sched::now_ns());
    let flags = FLAGS.get().copied().unwrap_or((None, None));
    if index == 0
        && let (Some(address), gateway) = flags
    {
        stack.set_ipv4(address, gateway);
    }
    let mut name = TextBuf::<8>::new();
    let _ = write!(name, "net{index}");
    let interface = Interface { name: name.clone(), stack: Mutex::new(stack), thread: Once::new() };
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
    if let Some(address) = interface.stack.lock().ipv4() {
        match flags.1 {
            Some(gateway) if index == 0 => {
                info!("Net: {name} has the address {address}, through the router {gateway}");
            }
            _ => info!("Net: {name} has the address {address}"),
        }
    }
    let options = Options::new().held().policy(Policy::Fair { nice: -5 });
    match sched::spawn(name.as_str(), run, index, options) {
        Ok(thread) => {
            let _ = interface.thread.set(thread.clone());
            sched::release(&thread);
            Ok(())
        }
        Err(_) => Err(Failure::Other("its thread could not start")),
    }
}

/// An interface's thread: polls its stack whenever there may be work.
fn run(index: usize) {
    let Some(interface) = INTERFACES.get(index).and_then(Once::get) else { return };
    loop {
        let delay = {
            let mut stack = interface.stack.lock();
            stack.poll(sched::now_ns());
            if stack.link().net.failed() {
                error!("Net: {}: the card stopped with an error", interface.name);
                return;
            }
            stack.poll_delay(sched::now_ns())
        };
        let now = sched::now_ns();
        match delay {
            Some(0) => sched::yield_now(),
            Some(delay) => sched::block_until(now.saturating_add(delay.min(IDLE_NS)), "net"),
            None => sched::block_until(now.saturating_add(IDLE_NS), "net"),
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

fn ping_router() -> Result<u64, &'static str> {
    let interface = first().ok_or("there is no network card")?;
    let (address, gateway) = match FLAGS.get().copied().unwrap_or((None, None)) {
        (Some(address), Some(gateway)) => (address, gateway),
        _ => (Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24), Ipv4Address::new(10, 0, 2, 2)),
    };
    let memory = PingMemory {
        receive_meta: forever_of(|| icmp::PacketMetadata::EMPTY, 4).ok_or("out of memory")?,
        receive: forever(512).ok_or("out of memory")?,
        transmit_meta: forever_of(|| icmp::PacketMetadata::EMPTY, 4).ok_or("out of memory")?,
        transmit: forever(512).ok_or("out of memory")?,
    };
    let pinger = {
        let mut stack = interface.stack.lock();
        if stack.ipv4().is_none() {
            stack.set_ipv4(address, Some(gateway));
            info!(
                "Net: {} has the address {address}, through the router {gateway}",
                interface.name
            );
        }
        Pinger::new(&mut stack, memory, 0x5254).ok_or("no socket slot is free")?
    };
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
