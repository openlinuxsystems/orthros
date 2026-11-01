// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Google's virtual network card, gVNIC (ADR 0197), which Google Compute
//! Engine gives its virtual machines, in its GQI-QPL queue format.
//!
//! The card is set up through an admin queue in a page of memory: the
//! driver describes itself, asks the card to describe itself, gives it
//! the memory its counters and interrupt doorbells go in, registers the
//! pages its frames go through (queue page lists, QPLs), and creates its
//! queues. Its registers, in BAR 0, and its doorbells, in BAR 2, are big
//! endian, and so is everything in the admin queue and the rings.
//!
//! In the GQI-QPL format the card reads and writes frames only in pages
//! the driver registered: a frame to send is copied into the transmit
//! queue's pages, which the descriptors name by their offset, and a
//! received frame is written into the receive queue's page for its
//! descriptor, 2 bytes in. The card counts the transmit descriptors it
//! is done with in a counter in memory, and marks each receive
//! descriptor it writes with a sequence number from 1 to 7, so the driver
//! knows a new one from an old one. Both are as FreeBSD's gve driver
//! (BSD-3-Clause, credits/freebsd-network-drivers.md) drives them; the
//! DQO format of Google's newer machine types is left for later.
//!
//! One queue pair, with the ring sizes the card gives. Each frame to send
//! takes a slot of [`SLOT`] bytes in the transmit pages, and one
//! descriptor, or two when it is longer than the card wants in its
//! first ([`MIN_PACKET_DESCRIPTOR`]). With an interrupt, the driver
//! acknowledges the queues' notification blocks once nothing is left to
//! take, and the card raises the next event; polled, they stay masked.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use radix_hw::{Dma, Mmio, device_barrier};
use radix_nic::{Error, Info, Interrupt, Link, MAX_FRAME, MIN_FRAME, Nic, PAGE, SLOT, usable_mac};

/// Google's PCI vendor ID.
pub const VENDOR: u16 = 0x1ae0;
/// gVNIC's PCI device ID.
pub const DEVICE: u16 = 0x0042;
/// The most descriptors a ring may have: the card says how many its
/// rings have, and a card that says more is refused.
pub const MAX_RING: usize = 4096;
/// The most bytes of a frame the first of its transmit descriptors
/// holds, as FreeBSD and Linux give the card; the rest goes in a second.
pub const MIN_PACKET_DESCRIPTOR: usize = 182;

/// The registers, in BAR 0.
mod reg {
    pub const DEVICE_STATUS: usize = 0;
    pub const DRIVER_STATUS: usize = 4;
    pub const MAX_TX_QUEUES: usize = 8;
    pub const MAX_RX_QUEUES: usize = 12;
    pub const ADMINQ_PFN: usize = 16;
    pub const ADMINQ_DOORBELL: usize = 20;
    pub const ADMINQ_EVENT_COUNTER: usize = 24;
    pub const DRIVER_VERSION: usize = 31;
    pub const ADMINQ_BASE_ADDRESS_HI: usize = 32;
    pub const ADMINQ_BASE_ADDRESS_LO: usize = 36;
    pub const ADMINQ_LENGTH: usize = 40;
    pub const END: usize = ADMINQ_LENGTH + 2;

    /// DEVICE_STATUS: the card asks to be reset; its link is up; it was
    /// reset, as the driver asked.
    pub const STATUS_RESET: u32 = 1 << 1;
    pub const STATUS_LINK: u32 = 1 << 2;
    pub const STATUS_IS_RESET: u32 = 1 << 4;
    /// DRIVER_STATUS: the driver runs; the driver resets the card.
    pub const DRIVER_RUN: u32 = 1 << 0;
    pub const DRIVER_RESET: u32 = 1 << 1;
}

/// Admin commands, and what they answer.
mod admin {
    pub const DESCRIBE_DEVICE: u32 = 0x1;
    pub const CONFIGURE_DEVICE_RESOURCES: u32 = 0x2;
    pub const REGISTER_PAGE_LIST: u32 = 0x3;
    pub const CREATE_TX_QUEUE: u32 = 0x5;
    pub const CREATE_RX_QUEUE: u32 = 0x6;
    pub const SET_DRIVER_PARAMETER: u32 = 0xb;
    pub const VERIFY_DRIVER_COMPATIBILITY: u32 = 0xf;

    pub const PASSED: u32 = 0x1;
    pub const UNIMPLEMENTED: u32 = 0xffff_fffe;

    /// The device descriptor's version, and the GQI-QPL option in it.
    pub const DESCRIPTOR_VERSION: u32 = 1;
    pub const OPTION_GQI_QPL: u16 = 0x3;
    pub const GQI_QPL_FORMAT: u8 = 0x2;
    /// SET_DRIVER_PARAMETER's MTU.
    pub const PARAMETER_MTU: u32 = 0x1;
    /// The capability bit of the GQI-QPL format, in the driver's
    /// description.
    pub const CAPABILITY_GQI_QPL: u64 = 1 << 0;
}

/// An interrupt doorbell's bits.
const IRQ_ACK: u32 = 1 << 31;
const IRQ_MASK: u32 = 1 << 30;
const IRQ_EVENT: u32 = 1 << 29;
/// A transmit descriptor's types: a packet's first, and a segment of it.
const TXD_STD: u8 = 0x00;
const TXD_SEG: u8 = 0x20;
/// The bytes before a received frame in its page.
const RX_PAD: usize = 2;
/// The receive buffers' bytes, as the card is told.
const RX_BUFFER: u16 = 2048;
/// A receive descriptor's flags, above its 3-bit sequence number: an
/// error, and a frame that goes on in the next descriptor.
const RXF_ERR: u16 = 1 << (3 + 8);
const RXF_PKT_CONT: u16 = 1 << (3 + 10);
/// The admin queue's commands, of 64 bytes, in its page.
const COMMAND: usize = 64;
const ADMIN_SLOTS: u32 = (PAGE / COMMAND) as u32;
/// How long an admin command may take, in microseconds.
const ADMIN_TIMEOUT_US: u32 = 2_000_000;
/// Each notification block's interrupt doorbell index, a cache line
/// apart.
const IRQ_DB_STRIDE: usize = 64;
/// The most frames in flight to send: two to each page of transmit
/// pages, which are a sixteenth of the ring.
const MAX_TX_SLOTS: usize = MAX_RING / 16 * 2;
/// What the card is told the driver is, ended by a newline.
const VERSION: &[u8] = b"GVE-Dukovo-1.0.0\n";

fn be16(memory: &Dma, at: usize) -> u16 {
    u16::from_be(memory.read16(at))
}

fn be32(memory: &Dma, at: usize) -> u32 {
    u32::from_be(memory.read32(at))
}

fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_be_bytes());
}

fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

fn put64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_be_bytes());
}

/// Pages for `bytes`.
fn pages(bytes: usize) -> usize {
    bytes.div_ceil(PAGE)
}

/// The card's registers and doorbells.
struct Bars {
    regs: Mmio,
    doors: Mmio,
    /// Whether the admin queue was given by its whole address, as cards
    /// of revision 1 take it, rather than by its page number.
    base_address: bool,
}

impl Bars {
    fn read(&self, offset: usize) -> u32 {
        u32::from_be(self.regs.read32(offset))
    }

    fn write(&self, offset: usize, value: u32) {
        self.regs.write32(offset, value.to_be());
    }

    fn ring(&self, offset: usize, value: u32) {
        self.doors.write32(offset, value.to_be());
    }

    /// Gives the card the admin queue at `address`: by its page number,
    /// as every card takes it, or, above 16 TiB, by its whole address,
    /// as cards of revision 1 take it.
    fn point_admin(&mut self, address: u64, revision: u8) -> Result<(), Error> {
        if let Ok(page) = u32::try_from(address / PAGE as u64) {
            self.write(reg::ADMINQ_PFN, page);
            return Ok(());
        }
        if revision < 1 {
            return Err(Error::Unsupported("an admin queue above 16 TiB"));
        }
        self.base_address = true;
        self.regs.write16(reg::ADMINQ_LENGTH, (PAGE as u16).to_be());
        self.write(reg::ADMINQ_BASE_ADDRESS_HI, (address >> 32) as u32);
        self.write(reg::ADMINQ_BASE_ADDRESS_LO, address as u32);
        self.write(reg::DRIVER_STATUS, reg::DRIVER_RUN);
        Ok(())
    }

    /// Takes the admin queue back, which makes the card let go of all it
    /// was given, waited for without sleeping for as long as an admin
    /// command may take.
    fn release(&self) {
        if self.base_address {
            self.write(reg::DRIVER_STATUS, reg::DRIVER_RESET);
        } else {
            self.write(reg::ADMINQ_PFN, 0);
        }
        for _ in 0..ADMIN_TIMEOUT_US {
            let done = if self.base_address {
                self.read(reg::DEVICE_STATUS) & reg::STATUS_IS_RESET != 0
            } else {
                self.read(reg::ADMINQ_PFN) == 0
            };
            if done {
                break;
            }
            core::hint::spin_loop();
        }
    }
}

/// Sends `command`, whose opcode is `opcode`, on the admin queue in
/// `memory`, of which `produced` commands were sent, and waits for the
/// card to run it.
fn send(
    bars: &Bars,
    memory: &Dma,
    produced: &mut u32,
    opcode: u32,
    mut command: [u8; COMMAND],
    delay: &mut dyn FnMut(u32),
) -> Result<(), Error> {
    put32(&mut command, 0, opcode);
    let at = (*produced % ADMIN_SLOTS) as usize * COMMAND;
    memory.write(at, &command);
    *produced = produced.wrapping_add(1);
    device_barrier();
    bars.write(reg::ADMINQ_DOORBELL, *produced);
    let sent = *produced;
    if !radix_nic::wait_for(delay, ADMIN_TIMEOUT_US, 1000, || {
        bars.read(reg::ADMINQ_EVENT_COUNTER) == sent
    }) {
        return Err(Error::Timeout("an admin command"));
    }
    device_barrier();
    match be32(memory, at + 4) {
        admin::PASSED => Ok(()),
        admin::UNIMPLEMENTED => Err(Error::Unsupported("an admin command")),
        _ => Err(Error::Device("it refused an admin command")),
    }
}

/// The admin queue while the card describes itself, before the rest of
/// its memory is had: taken back if starting fails there.
struct Probe<'a> {
    bars: &'a Bars,
    memory: Option<Dma>,
    produced: u32,
}

impl Probe<'_> {
    fn run(
        &mut self,
        opcode: u32,
        command: [u8; COMMAND],
        delay: &mut dyn FnMut(u32),
    ) -> Result<(), Error> {
        let Some(memory) = &self.memory else { return Err(Error::NoMemory) };
        send(self.bars, memory, &mut self.produced, opcode, command, delay)
    }
}

impl Drop for Probe<'_> {
    fn drop(&mut self) {
        if self.memory.is_some() {
            self.bars.release();
        }
    }
}

/// Where each part of the rings' memory is: the queues' resources, the
/// interrupt doorbells' indexes, the counters, and the rings.
#[derive(Clone, Copy)]
struct Layout {
    irq_dbs: usize,
    counters: usize,
    tx_ring: usize,
    rx_ring: usize,
    rx_data: usize,
    pages: usize,
}

impl Layout {
    fn new(blocks: usize, counters: usize, tx: usize, rx: usize) -> Layout {
        // The two queues' resources, of 64 bytes each, in the first page.
        let irq_dbs = PAGE;
        let counters_at = irq_dbs + pages(blocks * IRQ_DB_STRIDE) * PAGE;
        let tx_ring = counters_at + pages(counters * 4) * PAGE;
        let rx_ring = tx_ring + pages(tx * 16) * PAGE;
        let rx_data = rx_ring + pages(rx * 64) * PAGE;
        let end = rx_data + pages(rx * 8) * PAGE;
        Layout { irq_dbs, counters: counters_at, tx_ring, rx_ring, rx_data, pages: end / PAGE }
    }
}
const TX_RESOURCES: usize = 0;
const RX_RESOURCES: usize = 64;

/// The card, with all the memory it was given: dropped, it takes its
/// admin queue back first, which stops it, before any of that memory
/// goes.
struct Device {
    bars: Bars,
    admin: Dma,
    produced: u32,
    rings: Dma,
    tx_pages: Dma,
    rx_pages: Dma,
    released: bool,
}

impl Device {
    fn run(
        &mut self,
        opcode: u32,
        command: [u8; COMMAND],
        delay: &mut dyn FnMut(u32),
    ) -> Result<(), Error> {
        send(&self.bars, &self.admin, &mut self.produced, opcode, command, delay)
    }

    fn release(&mut self) {
        if !self.released {
            self.released = true;
            self.bars.release();
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.release();
    }
}

/// What the card says of itself.
struct Description {
    max_pages: u64,
    tx: usize,
    rx: usize,
    mtu: u16,
    counters: usize,
    rx_pages: usize,
    mac: [u8; 6],
}

/// A gVNIC card, set up and running.
pub struct Gve {
    device: Device,
    layout: Layout,
    /// The rings' descriptors.
    tx_count: usize,
    rx_count: usize,
    /// The transmit pages' slots, one to a frame in flight.
    tx_slots: usize,
    /// Transmit descriptors written, and done as the card counts them;
    /// frames handed to the card, and taken back.
    tx_req: u32,
    tx_done: u32,
    tx_sent: u32,
    tx_freed: u32,
    /// Each frame's descriptors, by its slot.
    tx_descriptors: [u8; MAX_TX_SLOTS],
    tx_doorbell: usize,
    tx_counter: usize,
    tx_irq: usize,
    tx_ring: bool,
    /// Receive descriptors taken, and buffers given back; the sequence
    /// number the next descriptor takes.
    rx_taken: u32,
    rx_filled: u32,
    rx_sequence: u16,
    /// Whether the descriptors taken are in the middle of a frame that
    /// goes on in more of them.
    rx_continued: bool,
    rx_doorbell: usize,
    rx_irq: usize,
    rx_ring: bool,
    info: Info,
    interrupt: Interrupt,
    fatal: bool,
    dropped: u64,
    stopped: bool,
}

/// Sets the card running, with its registers `regs` (BAR 0), its
/// doorbells `doors` (BAR 2) and its PCI revision `revision`, and
/// interrupts as `interrupt` says.
/// `fallback_mac` is its address if it gives none; `alloc` gives zeroed
/// DMA memory in pages, and `delay` waits the given number of
/// microseconds.
pub fn start(
    regs: Mmio,
    doors: Mmio,
    revision: u8,
    interrupt: Interrupt,
    fallback_mac: [u8; 6],
    alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    delay: &mut dyn FnMut(u32),
) -> Result<Gve, Error> {
    if regs.len() < reg::END {
        return Err(Error::Registers);
    }
    let mut bars = Bars { regs, doors, base_address: false };
    for &byte in VERSION {
        bars.regs.write8(reg::DRIVER_VERSION, byte);
    }
    let memory = alloc(1).ok_or(Error::NoMemory)?;
    let scratch = alloc(1).ok_or(Error::NoMemory)?;
    bars.point_admin(memory.phys(), revision)?;
    let mut probe = Probe { bars: &bars, memory: Some(memory), produced: 0 };
    let described = describe(&mut probe, &scratch, delay)?;
    let max_tx = bars.read(reg::MAX_TX_QUEUES) as usize;
    let max_rx = bars.read(reg::MAX_RX_QUEUES) as usize;
    if max_tx == 0 || max_rx == 0 || max_tx + max_rx > 256 {
        return Err(Error::Device("it has no queues, or too many"));
    }
    let blocks = max_tx + max_rx;
    let (tx_count, rx_count) = (described.tx, described.rx.min(described.rx_pages));
    let tx_page_count = tx_count / 16;
    let layout = Layout::new(blocks, described.counters, tx_count, rx_count);
    if (tx_page_count + rx_count) as u64 > described.max_pages {
        return Err(Error::Unsupported("as many registered pages as its rings need"));
    }

    // The memory, before the card is told of any of it.
    let rings = alloc(layout.pages).ok_or(Error::NoMemory)?;
    let tx_pages = alloc(tx_page_count).ok_or(Error::NoMemory)?;
    let rx_pages = alloc(rx_count).ok_or(Error::NoMemory)?;
    let lists = alloc(pages((tx_page_count + rx_count) * 8)).ok_or(Error::NoMemory)?;
    let (Some(admin), produced) = (probe.memory.take(), probe.produced) else {
        return Err(Error::NoMemory);
    };
    drop(probe);
    let mut device = Device { bars, admin, produced, rings, tx_pages, rx_pages, released: false };

    let mut command = [0u8; COMMAND];
    let rings_at = device.rings.phys();
    put64(&mut command, 8, rings_at + layout.counters as u64);
    put64(&mut command, 16, rings_at + layout.irq_dbs as u64);
    put32(&mut command, 24, described.counters as u32);
    put32(&mut command, 28, blocks as u32);
    put32(&mut command, 32, IRQ_DB_STRIDE as u32);
    put32(&mut command, 36, 0);
    command[40] = admin::GQI_QPL_FORMAT;
    device.run(admin::CONFIGURE_DEVICE_RESOURCES, command, delay)?;

    if described.mtu >= 1500 {
        let mut command = [0u8; COMMAND];
        put32(&mut command, 8, admin::PARAMETER_MTU);
        put64(&mut command, 16, 1500);
        device.run(admin::SET_DRIVER_PARAMETER, command, delay)?;
    }

    // The queue page lists: the transmit queue's as list 0, the receive
    // queue's after every transmit queue's, as FreeBSD numbers them.
    let rx_list = max_tx as u32;
    for (id, pages_at, count, list_at) in [
        (0, device.tx_pages.phys(), tx_page_count, 0),
        (rx_list, device.rx_pages.phys(), rx_count, tx_page_count * 8),
    ] {
        for page in 0..count {
            lists.write64(list_at + page * 8, (pages_at + (page * PAGE) as u64).to_be());
        }
        let mut command = [0u8; COMMAND];
        put32(&mut command, 8, id);
        put32(&mut command, 12, count as u32);
        put64(&mut command, 16, lists.phys() + list_at as u64);
        put64(&mut command, 24, PAGE as u64);
        device.run(admin::REGISTER_PAGE_LIST, command, delay)?;
    }

    // Each receive descriptor's buffer is the start of its own page.
    for slot in 0..rx_count {
        device.rings.write64(layout.rx_data + slot * 8, ((slot * PAGE) as u64).to_be());
    }
    let mut command = [0u8; COMMAND];
    put32(&mut command, 8, 0);
    put32(&mut command, 12, 0);
    put32(&mut command, 20, rx_list);
    put64(&mut command, 24, rings_at + RX_RESOURCES as u64);
    put64(&mut command, 32, rings_at + layout.rx_ring as u64);
    put64(&mut command, 40, rings_at + layout.rx_data as u64);
    put32(&mut command, 48, rx_list);
    put16(&mut command, 52, rx_count as u16);
    put16(&mut command, 54, RX_BUFFER);
    device.run(admin::CREATE_RX_QUEUE, command, delay)?;
    let mut command = [0u8; COMMAND];
    put32(&mut command, 8, 0);
    put64(&mut command, 16, rings_at + TX_RESOURCES as u64);
    put64(&mut command, 24, rings_at + layout.tx_ring as u64);
    put32(&mut command, 32, 0);
    put32(&mut command, 36, 0);
    put16(&mut command, 48, tx_count as u16);
    device.run(admin::CREATE_TX_QUEUE, command, delay)?;
    drop(lists);

    // Where the card put each queue's doorbell and counter, and its
    // notification blocks' interrupt doorbells.
    let rings = &device.rings;
    let door = |index: u32| {
        let offset = index as usize * 4;
        (offset + 4 <= device.bars.doors.len())
            .then_some(offset)
            .ok_or(Error::Device("it named a doorbell outside its doorbells"))
    };
    let tx_doorbell = door(be32(rings, TX_RESOURCES))?;
    let rx_doorbell = door(be32(rings, RX_RESOURCES))?;
    let tx_irq = door(be32(rings, layout.irq_dbs))?;
    let rx_irq = door(be32(rings, layout.irq_dbs + rx_list as usize * IRQ_DB_STRIDE))?;
    let counter = be32(rings, TX_RESOURCES + 4) as usize;
    if counter >= described.counters {
        return Err(Error::Device("it named a counter it does not have"));
    }

    let tx_slots = (tx_page_count * PAGE / SLOT).min(MAX_TX_SLOTS).min(tx_count);
    let info = Info {
        model: "gVNIC",
        mac: described.mac,
        receive_slots: rx_count as u16,
        transmit_slots: (tx_slots - 1) as u16,
    };
    let mac = if usable_mac(info.mac) { info.mac } else { fallback_mac };
    let mut gve = Gve {
        device,
        layout,
        tx_count,
        rx_count,
        tx_slots,
        tx_req: 0,
        tx_done: 0,
        tx_sent: 0,
        tx_freed: 0,
        tx_descriptors: [0; MAX_TX_SLOTS],
        tx_doorbell,
        tx_counter: layout.counters + counter * 4,
        tx_irq,
        tx_ring: false,
        rx_taken: 0,
        rx_filled: rx_count as u32,
        rx_sequence: 1,
        rx_continued: false,
        rx_doorbell,
        rx_irq,
        rx_ring: true,
        info: Info { mac, ..info },
        interrupt,
        fatal: false,
        dropped: 0,
        stopped: false,
    };
    // Every receive buffer to the card, and the interrupts on or off.
    gve.flush();
    let irq = if interrupt == Interrupt::Msi { 0 } else { IRQ_MASK };
    gve.device.bars.ring(gve.tx_irq, irq);
    gve.device.bars.ring(gve.rx_irq, irq);
    Ok(gve)
}

/// Tells the card what the driver is, and asks what the card is.
fn describe(
    admin: &mut Probe<'_>,
    scratch: &Dma,
    delay: &mut dyn FnMut(u32),
) -> Result<Description, Error> {
    // The driver's description: the GQI-QPL format alone. A card that
    // does not take it goes on without.
    let mut info = [0u8; 48];
    put64(&mut info, 16, admin::CAPABILITY_GQI_QPL);
    scratch.write(0, &info);
    let mut command = [0u8; COMMAND];
    put64(&mut command, 8, 304);
    put64(&mut command, 16, scratch.phys());
    match admin.run(admin::VERIFY_DRIVER_COMPATIBILITY, command, delay) {
        Ok(()) | Err(Error::Unsupported(_)) => {}
        Err(err) => return Err(err),
    }
    scratch.zero();
    let mut command = [0u8; COMMAND];
    put64(&mut command, 8, scratch.phys());
    put32(&mut command, 16, admin::DESCRIPTOR_VERSION);
    put32(&mut command, 20, PAGE as u32);
    admin.run(admin::DESCRIBE_DEVICE, command, delay)?;
    device_barrier();
    let read64 = |at| u64::from_be(scratch.read64(at));
    let mut mac = [0u8; 6];
    scratch.read(24, &mut mac);
    let described = Description {
        max_pages: read64(0),
        tx: usize::from(be16(scratch, 10)),
        rx: usize::from(be16(scratch, 12)),
        mtu: be16(scratch, 16),
        counters: usize::from(be16(scratch, 18)),
        rx_pages: usize::from(be16(scratch, 22)),
        mac,
    };
    // Its options, after the descriptor: GQI-QPL must be among them.
    let options = usize::from(be16(scratch, 30));
    let end = usize::from(be16(scratch, 32)).min(PAGE);
    let mut at = 40;
    let mut gqi_qpl = false;
    for _ in 0..options {
        if at + 8 > end {
            return Err(Error::Device("its options run past its description"));
        }
        let (id, length) = (be16(scratch, at), usize::from(be16(scratch, at + 2)));
        if at + 8 + length > end {
            return Err(Error::Device("its options run past its description"));
        }
        if id == admin::OPTION_GQI_QPL && be32(scratch, at + 4) == 0 && length >= 4 {
            gqi_qpl = true;
        }
        at += 8 + length;
    }
    if !gqi_qpl {
        return Err(Error::Unsupported("the GQI-QPL queue format"));
    }
    let ring = |n: usize| (64..=MAX_RING).contains(&n) && n.is_power_of_two();
    if !ring(described.tx) || !ring(described.rx) || described.rx_pages < 64 {
        return Err(Error::Unsupported("its ring sizes"));
    }
    if described.counters == 0 {
        return Err(Error::Device("it has no counters"));
    }
    Ok(described)
}

impl Gve {
    /// Takes back the transmit slots of the frames the card sent, as its
    /// counter says.
    fn reclaim(&mut self) {
        let done = be32(&self.device.rings, self.tx_counter);
        while self.tx_freed != self.tx_sent {
            let slot = self.tx_freed as usize % self.tx_slots;
            let descriptors = u32::from(self.tx_descriptors[slot]);
            if done.wrapping_sub(self.tx_done) < descriptors {
                break;
            }
            self.tx_done = self.tx_done.wrapping_add(descriptors);
            self.tx_freed = self.tx_freed.wrapping_add(1);
        }
        if done.wrapping_sub(self.tx_done) as usize > self.tx_count {
            // More done than was ever handed to it.
            self.fatal = true;
        }
    }

    /// The next received frame, copied into `frame`: `None` when the card
    /// has none, `Some(None)` for one dropped.
    fn next_received(&mut self, frame: &mut [u8]) -> Option<Option<usize>> {
        if self.fatal {
            return None;
        }
        let slot = self.rx_taken as usize % self.rx_count;
        let at = self.layout.rx_ring + slot * 64;
        let flags = be16(&self.device.rings, at + 62);
        if flags & 7 != self.rx_sequence {
            return None;
        }
        device_barrier();
        let len = usize::from(be16(&self.device.rings, at + 60));
        self.rx_taken = self.rx_taken.wrapping_add(1);
        self.rx_sequence = if self.rx_sequence == 7 { 1 } else { self.rx_sequence + 1 };
        // Its buffer goes back as it was: the frame is copied out before
        // the doorbell tells the card.
        self.rx_filled = self.rx_filled.wrapping_add(1);
        self.rx_ring = true;
        let continued = core::mem::replace(&mut self.rx_continued, flags & RXF_PKT_CONT != 0);
        if self.rx_continued {
            return Some(None);
        }
        let len = len.wrapping_sub(RX_PAD);
        if continued
            || flags & RXF_ERR != 0
            || !(MIN_FRAME..=MAX_FRAME).contains(&len)
            || len > frame.len()
        {
            self.dropped += 1;
            return Some(None);
        }
        self.device.rx_pages.read(slot * PAGE + RX_PAD, &mut frame[..len]);
        Some(Some(len))
    }

    /// Whether a received frame waits.
    fn received(&self) -> bool {
        let slot = self.rx_taken as usize % self.rx_count;
        be16(&self.device.rings, self.layout.rx_ring + slot * 64 + 62) & 7 == self.rx_sequence
    }

    /// Writes transmit descriptor `n` of the ring.
    fn write_descriptor(
        &mut self,
        n: u32,
        kind: u8,
        count: u8,
        len: usize,
        segment: usize,
        offset: usize,
    ) {
        let mut descriptor = [0u8; 16];
        descriptor[0] = kind;
        if kind == TXD_STD {
            descriptor[3] = count;
            put16(&mut descriptor, 4, len as u16);
        }
        put16(&mut descriptor, 6, segment as u16);
        put64(&mut descriptor, 8, offset as u64);
        let at = self.layout.tx_ring + n as usize % self.tx_count * 16;
        self.device.rings.write(at, &descriptor);
    }
}

impl Nic for Gve {
    fn info(&self) -> Info {
        self.info
    }

    fn link(&self) -> Link {
        let up = !self.stopped && self.device.bars.read(reg::DEVICE_STATUS) & reg::STATUS_LINK != 0;
        Link { up, mbps: 0, full_duplex: up }
    }

    fn failed(&self) -> bool {
        if self.fatal {
            return true;
        }
        let status = self.device.bars.read(reg::DEVICE_STATUS);
        status == u32::MAX || status & reg::STATUS_RESET != 0
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        if self.stopped {
            return None;
        }
        for pass in 0..2 {
            while let Some(taken) = self.next_received(frame) {
                if taken.is_some() {
                    return taken;
                }
            }
            if pass == 1 || self.fatal || self.interrupt != Interrupt::Msi {
                break;
            }
            // Nothing more: the notification blocks acknowledged, so that
            // the card raises the next event, and a last look for a frame
            // that came in between.
            self.device.bars.ring(self.rx_irq, IRQ_ACK | IRQ_EVENT);
            self.device.bars.ring(self.tx_irq, IRQ_ACK | IRQ_EVENT);
            device_barrier();
            if !self.received() {
                break;
            }
        }
        None
    }

    fn can_send(&mut self) -> bool {
        if self.stopped || self.fatal {
            return false;
        }
        self.reclaim();
        let in_flight = self.tx_sent.wrapping_sub(self.tx_freed) as usize;
        let descriptors = self.tx_req.wrapping_sub(self.tx_done) as usize;
        !self.fatal && in_flight + 1 < self.tx_slots && descriptors + 2 <= self.tx_count
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) || !self.can_send() {
            return false;
        }
        let slot = self.tx_sent as usize % self.tx_slots;
        let offset = slot * SLOT;
        self.device.tx_pages.write(offset, frame);
        let first = frame.len().min(MIN_PACKET_DESCRIPTOR);
        let count = if first < frame.len() { 2 } else { 1 };
        self.write_descriptor(self.tx_req, TXD_STD, count, frame.len(), first, offset);
        if count == 2 {
            self.write_descriptor(
                self.tx_req + 1,
                TXD_SEG,
                0,
                0,
                frame.len() - first,
                offset + first,
            );
        }
        self.tx_descriptors[slot] = count;
        self.tx_req = self.tx_req.wrapping_add(u32::from(count));
        self.tx_sent = self.tx_sent.wrapping_add(1);
        self.tx_ring = true;
        true
    }

    fn flush(&mut self) {
        if self.stopped {
            return;
        }
        device_barrier();
        if core::mem::take(&mut self.tx_ring) {
            self.device.bars.ring(self.tx_doorbell, self.tx_req);
        }
        if core::mem::take(&mut self.rx_ring) {
            self.device.bars.ring(self.rx_doorbell, self.rx_filled);
        }
    }

    fn dropped(&self) -> u64 {
        self.dropped
    }

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.device.bars.ring(self.tx_irq, IRQ_MASK);
        self.device.bars.ring(self.rx_irq, IRQ_MASK);
        self.device.release();
    }
}

impl Drop for Gve {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
