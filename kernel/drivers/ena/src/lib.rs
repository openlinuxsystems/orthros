// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Amazon's Elastic Network Adapter, ENA (ADR 0197): the network card of
//! every current EC2 instance, a PCI function of the Nitro card.
//!
//! The card is driven through queues in memory rather than registers: an
//! admin queue the driver sends commands on (to read the card's features,
//! set its options and create its queues), the admin completion queue
//! the card answers on, and an asynchronous event queue (AENQ) the card
//! tells of its link and its errors on. Each queue's ownership goes by a
//! phase bit, which flips each time the queue wraps. Registers are few:
//! the queues' addresses, a reset, and doorbells, which the card names
//! when it creates a queue. Some cards answer register reads only by
//! DMA (readless mode): the driver asks for a register by writing its
//! offset, and the card writes its value into memory. The layouts and the
//! sequences here follow Amazon's ena-com, as FreeBSD carries it
//! (BSD-3-Clause, credits/freebsd-network-drivers.md).
//!
//! One queue pair, with the descriptors and frames in host memory (the
//! low-latency queues some instance types prefer, with descriptors in the
//! card's memory, are left for later): a receive queue of up to
//! [`DEPTH`] [`Slots`] slots with its completion queue, and a transmit
//! queue with its own, each frame in one descriptor that asks for a
//! completion. Admin commands are polled, as they are only sent while the
//! card starts. With an interrupt, the card raises the queue pair's
//! vector and masks it, and the driver unmasks it once nothing is left to
//! take; the kernel gives every MSI-X vector the same message, so the
//! vector the queues name does not matter. The event queue is looked at
//! each time nothing is left to receive.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use radix_hw::{Dma, Mmio, device_barrier};
use radix_nic::{
    Error, Info, Interrupt, Link, MAX_FRAME, MIN_FRAME, Nic, PAGE, SLOT, Slots, usable_mac,
};

/// Amazon's PCI vendor ID (Annapurna Labs).
pub const VENDOR: u16 = 0x1d0f;
/// The most descriptors a queue is given: the card says how many it
/// takes, and a queue gets the fewer, rounded down to a power of two.
pub const DEPTH: usize = 256;

/// The card's PCI device IDs: the physical function, the virtual function
/// EC2 gives instances, and their variants with a reserved BAR.
pub fn identify(vendor: u16, device: u16) -> bool {
    vendor == VENDOR && matches!(device, 0x0ec2 | 0x1ec2 | 0xec20 | 0xec21)
}

/// The registers, in BAR 0.
mod reg {
    pub const VERSION: usize = 0x00;
    pub const CONTROLLER_VERSION: usize = 0x04;
    pub const CAPS: usize = 0x08;
    pub const AQ_BASE_LO: usize = 0x10;
    pub const AQ_BASE_HI: usize = 0x14;
    pub const AQ_CAPS: usize = 0x18;
    pub const ACQ_BASE_LO: usize = 0x20;
    pub const ACQ_BASE_HI: usize = 0x24;
    pub const ACQ_CAPS: usize = 0x28;
    pub const AQ_DB: usize = 0x2c;
    pub const AENQ_CAPS: usize = 0x34;
    pub const AENQ_BASE_LO: usize = 0x38;
    pub const AENQ_BASE_HI: usize = 0x3c;
    pub const AENQ_HEAD_DB: usize = 0x40;
    pub const INTR_MASK: usize = 0x4c;
    pub const DEV_CTL: usize = 0x54;
    pub const DEV_STS: usize = 0x58;
    pub const MMIO_REG_READ: usize = 0x5c;
    pub const MMIO_RESP_LO: usize = 0x60;
    pub const MMIO_RESP_HI: usize = 0x64;
    pub const END: usize = MMIO_RESP_HI + 4;

    /// CAPS: the reset's time limit, in 100 ms, the bits of DMA address
    /// the card takes, and an admin command's time limit, in 100 ms.
    pub const CAPS_RESET_TIMEOUT: (u32, u32) = (1, 0x1f);
    pub const CAPS_DMA_WIDTH: (u32, u32) = (8, 0xff);
    pub const CAPS_ADMIN_TIMEOUT: (u32, u32) = (16, 0xf);
    /// DEV_CTL: reset the card (for a normal reason, 0, in bits 28-31).
    pub const DEV_RESET: u32 = 1 << 0;
    /// DEV_STS.
    pub const READY: u32 = 1 << 0;
    pub const RESET_IN_PROGRESS: u32 = 1 << 3;
}

/// Admin commands and features.
mod admin {
    pub const CREATE_SQ: u8 = 1;
    pub const CREATE_CQ: u8 = 3;
    pub const GET_FEATURE: u8 = 8;
    pub const SET_FEATURE: u8 = 9;

    pub const DEVICE_ATTRIBUTES: u8 = 1;
    pub const MAX_QUEUES_NUM: u8 = 2;
    pub const MAX_QUEUES_EXT: u8 = 7;
    pub const MTU: u8 = 14;
    pub const AENQ_CONFIG: u8 = 26;
    pub const LINK_CONFIG: u8 = 27;

    /// The version of MAX_QUEUES_EXT's answer that is read.
    pub const MAX_QUEUES_EXT_VERSION: u8 = 1;
    /// CREATE_SQ's directions, placement and completion policy.
    pub const DIRECTION_TX: u8 = 1;
    pub const DIRECTION_RX: u8 = 2;
    pub const PLACEMENT_HOST: u8 = 1;
    pub const PHYSICALLY_CONTIGUOUS: u8 = 1 << 0;
    /// CREATE_CQ: interrupts on.
    pub const INTERRUPT_MODE: u8 = 1 << 5;
}

/// The event queue's groups, which are also the bits of its
/// configuration: those the driver takes.
mod event {
    pub const LINK_CHANGE: u16 = 0;
    pub const FATAL_ERROR: u16 = 1;
    pub const WARNING: u16 = 2;
    pub const NOTIFICATION: u16 = 3;
    pub const DEVICE_REQUEST_RESET: u16 = 7;
    pub const WANTED: u32 = 1 << LINK_CHANGE
        | 1 << FATAL_ERROR
        | 1 << WARNING
        | 1 << NOTIFICATION
        | 1 << DEVICE_REQUEST_RESET;
}

/// Each admin, completion and event queue entry's bytes.
const ENTRY: usize = 64;
/// The admin queue's commands, and its completion queue's.
const ADMIN_DEPTH: usize = 32;
/// The event queue's entries.
const AENQ_DEPTH: usize = 16;
/// The admin memory: the admin queue, its completion queue, the event
/// queue and the answer to a register read, in two pages.
const AQ: usize = 0;
const ACQ: usize = AQ + ADMIN_DEPTH * ENTRY;
const AENQ: usize = ACQ + ADMIN_DEPTH * ENTRY;
const RESPONSE: usize = AENQ + AENQ_DEPTH * ENTRY;
const ADMIN_PAGES: usize = 2;
const _: () = assert!(RESPONSE + 8 <= ADMIN_PAGES * PAGE);

/// The queues' memory, in four pages: each submission queue's 16-byte
/// descriptors, the receive completions' 16 bytes and the transmit
/// completions' 8.
const TX_SQ: usize = 0;
const RX_SQ: usize = TX_SQ + DEPTH * 16;
const RX_CQ: usize = RX_SQ + DEPTH * 16;
const TX_CQ: usize = RX_CQ + DEPTH * 16;
const QUEUE_PAGES: usize = 4;
const _: () = assert!(TX_CQ + DEPTH * 8 <= QUEUE_PAGES * PAGE);
/// The fewest descriptors a queue is driven with.
const MIN_DEPTH: usize = 16;

/// A transmit descriptor's length and control word.
mod tx {
    pub const PHASE: u32 = 1 << 24;
    pub const FIRST: u32 = 1 << 26;
    pub const LAST: u32 = 1 << 27;
    pub const COMP_REQ: u32 = 1 << 28;
}

/// A receive descriptor's control byte, and its completion's status.
mod rx {
    pub const PHASE: u8 = 1 << 0;
    pub const FIRST: u8 = 1 << 2;
    pub const LAST: u8 = 1 << 3;
    pub const COMP_REQ: u8 = 1 << 4;
    pub const DONE_PHASE: u32 = 1 << 24;
    pub const DONE_FIRST: u32 = 1 << 26;
    pub const DONE_LAST: u32 = 1 << 27;
}

/// What an unmask register takes to let the queue's interrupt through
/// again, with no delay between frames and their interrupt.
const UNMASK: u32 = 1 << 30;
/// How long a register read by DMA may take, in microseconds.
const READ_TIMEOUT_US: u32 = 200_000;
/// How long an admin command may take when the card does not say.
const ADMIN_TIMEOUT_US: u32 = 3_000_000;
/// The MTU the card is set to: Ethernet's.
const MTU: u32 = 1500;

/// The card's registers, and how they are read.
struct Registers {
    regs: Mmio,
    /// The admin queues and the register-read answer.
    memory: Dma,
    /// Whether the card answers register reads by DMA.
    readless: bool,
    /// The last register read's number.
    sequence: u16,
    /// Whether the card was reset to stop it.
    halted: bool,
}

impl Drop for Registers {
    /// A card that failed to start is stopped before its memory goes.
    fn drop(&mut self) {
        self.halt();
    }
}

impl Registers {
    /// Tells the card where to write the answers to register reads, which
    /// it forgets as it resets.
    fn point_response(&self) {
        if self.readless {
            let address = self.memory.phys() + RESPONSE as u64;
            self.regs.write32(reg::MMIO_RESP_LO, address as u32);
            self.regs.write32(reg::MMIO_RESP_HI, (address >> 32) as u32);
        }
    }

    /// The register at `offset`: read from it, or asked for and waited
    /// for, a microsecond at a time, in readless mode.
    fn read(&mut self, offset: usize, delay: &mut dyn FnMut(u32)) -> Result<u32, Error> {
        if !self.readless {
            return Ok(self.regs.read32(offset));
        }
        self.sequence = self.sequence.wrapping_add(1);
        let sequence = self.sequence;
        // An answer the card has not written yet cannot match.
        self.memory.write16(RESPONSE, sequence.wrapping_add(0xdead));
        device_barrier();
        self.regs.write32(reg::MMIO_REG_READ, ((offset as u32) << 16) | u32::from(sequence));
        let mut waited = 0;
        while self.memory.read16(RESPONSE) != sequence {
            if waited >= READ_TIMEOUT_US {
                return Err(Error::Timeout("reading a register"));
            }
            delay(1);
            waited += 1;
        }
        device_barrier();
        if usize::from(self.memory.read16(RESPONSE + 2)) != offset {
            return Err(Error::Device("it answered for another register"));
        }
        Ok(self.memory.read32(RESPONSE + 4))
    }

    /// Waits up to `ms` milliseconds for the reset to be in progress, or
    /// not, as `in_progress` says.
    fn wait_reset(
        &mut self,
        in_progress: bool,
        ms: u32,
        delay: &mut dyn FnMut(u32),
    ) -> Result<(), Error> {
        for waited in 0..=ms {
            let status = self.read(reg::DEV_STS, delay)?;
            if (status & reg::RESET_IN_PROGRESS != 0) == in_progress {
                return Ok(());
            }
            if waited < ms {
                delay(1000);
            }
        }
        Err(Error::Timeout("resetting"))
    }

    /// Stops the card with a reset, which stops its queues and its DMA,
    /// waited for without sleeping, as the stack's thread may not, for as
    /// long as a register read may take.
    fn halt(&mut self) {
        if self.halted {
            return;
        }
        self.halted = true;
        self.regs.write32(reg::DEV_CTL, reg::DEV_RESET);
        self.point_response();
        let mut spin = |_: u32| core::hint::spin_loop();
        for _ in 0..1000 {
            let status = self.read(reg::DEV_STS, &mut spin);
            if !matches!(status, Ok(status) if status & reg::RESET_IN_PROGRESS == 0) {
                break;
            }
        }
        self.regs.write32(reg::DEV_CTL, 0);
    }

    /// Resets the card, as ena-com's `ena_com_dev_reset` does.
    fn reset(&mut self, delay: &mut dyn FnMut(u32)) -> Result<(), Error> {
        if self.read(reg::DEV_STS, delay)? & reg::READY == 0 {
            return Err(Error::Device("it is not ready"));
        }
        let caps = self.read(reg::CAPS, delay)?;
        let timeout = field(caps, reg::CAPS_RESET_TIMEOUT) * 100;
        if timeout == 0 {
            return Err(Error::Device("it gives no time for a reset"));
        }
        self.regs.write32(reg::DEV_CTL, reg::DEV_RESET);
        self.point_response();
        self.wait_reset(true, timeout, delay)?;
        self.regs.write32(reg::DEV_CTL, 0);
        self.wait_reset(false, timeout, delay)
    }
}

/// The bits of `value` that `(shift, mask)` names.
fn field(value: u32, (shift, mask): (u32, u32)) -> u32 {
    (value >> shift) & mask
}

/// The admin queue and its completion queue, as the driver goes round
/// them: one command at a time.
struct Admin {
    tail: u16,
    phase: bool,
    head: u16,
    done_phase: bool,
    timeout_us: u32,
}

impl Admin {
    /// Sends `command` (its opcode at byte 2) and waits for its
    /// completion, which it returns.
    fn run(
        &mut self,
        card: &Registers,
        mut command: [u8; ENTRY],
        delay: &mut dyn FnMut(u32),
    ) -> Result<[u8; ENTRY], Error> {
        let index = usize::from(self.tail) % ADMIN_DEPTH;
        command[0..2].copy_from_slice(&(index as u16).to_le_bytes());
        command[3] = u8::from(self.phase);
        card.memory.write(AQ + index * ENTRY, &command);
        self.tail = self.tail.wrapping_add(1);
        if usize::from(self.tail) % ADMIN_DEPTH == 0 {
            self.phase = !self.phase;
        }
        device_barrier();
        card.regs.write32(reg::AQ_DB, u32::from(self.tail));
        let at = ACQ + usize::from(self.head) % ADMIN_DEPTH * ENTRY;
        let done = radix_nic::wait_for(delay, self.timeout_us, 20, || {
            (card.memory.read8(at + 3) & 1 != 0) == self.done_phase
        });
        if !done {
            return Err(Error::Timeout("an admin command"));
        }
        device_barrier();
        let mut completion = [0u8; ENTRY];
        card.memory.read(at, &mut completion);
        self.head = self.head.wrapping_add(1);
        if usize::from(self.head) % ADMIN_DEPTH == 0 {
            self.done_phase = !self.done_phase;
        }
        let id = u16::from_le_bytes([completion[0], completion[1]]) & 0xfff;
        if usize::from(id) != index {
            return Err(Error::Device("it completed another admin command"));
        }
        if completion[2] != 0 {
            return Err(Error::Device("it refused an admin command"));
        }
        Ok(completion)
    }

    /// The feature `feature`, in the version `version`, as GET_FEATURE
    /// answers.
    fn get(
        &mut self,
        card: &Registers,
        feature: u8,
        version: u8,
        delay: &mut dyn FnMut(u32),
    ) -> Result<[u8; ENTRY], Error> {
        let mut command = [0u8; ENTRY];
        command[2] = admin::GET_FEATURE;
        command[17] = feature;
        command[18] = version;
        self.run(card, command, delay)
    }

    /// Sets the feature `feature` to `payload`, from byte 20.
    fn set(
        &mut self,
        card: &Registers,
        feature: u8,
        payload: &[u32],
        delay: &mut dyn FnMut(u32),
    ) -> Result<(), Error> {
        let mut command = [0u8; ENTRY];
        command[2] = admin::SET_FEATURE;
        command[17] = feature;
        for (i, word) in payload.iter().enumerate() {
            put32(&mut command, 20 + i * 4, *word);
        }
        self.run(card, command, delay).map(|_| ())
    }
}

fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn get32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn get16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// A submission queue, as the driver fills it: where it is, the next
/// descriptor, the phase it takes, and its doorbell.
struct Sq {
    base: usize,
    tail: u16,
    phase: bool,
    doorbell: usize,
    /// Whether descriptors were added since the doorbell was rung.
    ring: bool,
}

impl Sq {
    fn index(&self, depth: usize) -> usize {
        usize::from(self.tail) % depth
    }

    fn advance(&mut self, depth: usize) {
        self.tail = self.tail.wrapping_add(1);
        if usize::from(self.tail) % depth == 0 {
            self.phase = !self.phase;
        }
        self.ring = true;
    }
}

/// A completion queue, as the driver takes from it.
struct Cq {
    base: usize,
    /// Its entries' bytes.
    entry: usize,
    head: u16,
    phase: bool,
    /// Where the card is told how far the driver took, if it asks to be.
    head_doorbell: Option<usize>,
    /// The head the card was last told of.
    told: u16,
    unmask: usize,
}

impl Cq {
    fn at(&self, depth: usize) -> usize {
        self.base + usize::from(self.head) % depth * self.entry
    }

    fn advance(&mut self, depth: usize) {
        self.head = self.head.wrapping_add(1);
        if usize::from(self.head) % depth == 0 {
            self.phase = !self.phase;
        }
    }
}

/// An ENA card, set up and running.
pub struct Ena {
    card: Registers,
    queues: Dma,
    depth: usize,
    tx_slots: Slots,
    rx_slots: Slots,
    tx: Sq,
    tx_done: Cq,
    rx: Sq,
    rx_done: Cq,
    /// The transmit slots free to take, as a stack of their numbers.
    tx_free: [u16; DEPTH],
    tx_free_count: usize,
    /// Which transmit slots the card has, and which receive slots.
    tx_busy: [bool; DEPTH],
    rx_posted: [bool; DEPTH],
    /// The event queue's head and the phase its next entry takes.
    aenq_head: u16,
    aenq_phase: bool,
    info: Info,
    link: Link,
    interrupt: Interrupt,
    /// Whether the card said it failed, or did what a working one never
    /// does.
    fatal: bool,
    dropped: u64,
    stopped: bool,
}

/// Resets the card, whose registers are `regs` (BAR 0) and whose PCI
/// revision is `revision` (its low bit says register reads work), and
/// sets it running, with interrupts as `interrupt` says. `fallback_mac`
/// is its address if it gives none; `alloc` gives zeroed DMA memory in
/// pages, and `delay` waits the given number of microseconds.
pub fn start(
    regs: Mmio,
    revision: u8,
    interrupt: Interrupt,
    fallback_mac: [u8; 6],
    alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    delay: &mut dyn FnMut(u32),
) -> Result<Ena, Error> {
    if regs.len() < reg::END {
        return Err(Error::Registers);
    }
    // All the memory first, so that a card that fails to start is reset
    // (as its registers are dropped) before any of it is given back.
    let queues = alloc(QUEUE_PAGES).ok_or(Error::NoMemory)?;
    let tx_slots = Slots::new(DEPTH, alloc).ok_or(Error::NoMemory)?;
    let rx_slots = Slots::new(DEPTH, alloc).ok_or(Error::NoMemory)?;
    let memory = alloc(ADMIN_PAGES).ok_or(Error::NoMemory)?;
    let mut card =
        Registers { regs, memory, readless: revision & 1 == 0, sequence: 0, halted: false };
    card.point_response();
    card.reset(delay)?;
    let version = card.read(reg::VERSION, delay)?;
    let controller = card.read(reg::CONTROLLER_VERSION, delay)?;
    if version == u32::MAX || controller == u32::MAX {
        return Err(Error::Device("its version cannot be read"));
    }
    let caps = card.read(reg::CAPS, delay)?;
    if field(caps, reg::CAPS_DMA_WIDTH) < 32 {
        return Err(Error::Unsupported("DMA addresses of fewer than 32 bits"));
    }
    let timeout_us = match field(caps, reg::CAPS_ADMIN_TIMEOUT) {
        0 => ADMIN_TIMEOUT_US,
        tenths => tenths * 100_000,
    };
    // The admin queues, polled.
    if card.read(reg::DEV_STS, delay)? & reg::READY == 0 {
        return Err(Error::Device("it is not ready after its reset"));
    }
    let base = card.memory.phys();
    let queue_caps = |depth: usize| depth as u32 | ((ENTRY as u32) << 16);
    for (lo, hi, offset) in [
        (reg::AQ_BASE_LO, reg::AQ_BASE_HI, AQ),
        (reg::ACQ_BASE_LO, reg::ACQ_BASE_HI, ACQ),
        (reg::AENQ_BASE_LO, reg::AENQ_BASE_HI, AENQ),
    ] {
        let address = base + offset as u64;
        card.regs.write32(lo, address as u32);
        card.regs.write32(hi, (address >> 32) as u32);
    }
    card.regs.write32(reg::AQ_CAPS, queue_caps(ADMIN_DEPTH));
    card.regs.write32(reg::ACQ_CAPS, queue_caps(ADMIN_DEPTH));
    card.regs.write32(reg::AENQ_CAPS, queue_caps(AENQ_DEPTH));
    card.regs.write32(reg::INTR_MASK, 1);
    let mut admin = Admin { tail: 0, phase: true, head: 0, done_phase: true, timeout_us };

    let attributes = admin.get(&card, admin::DEVICE_ATTRIBUTES, 0, delay)?;
    let supported = get32(&attributes, 16);
    let has = |feature: u8| supported & (1 << feature) != 0;
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&attributes[32..38]);
    let mac = if usable_mac(mac) { mac } else { fallback_mac };
    let max_mtu = get32(&attributes, 40);
    if max_mtu != 0 && max_mtu < MTU {
        return Err(Error::Unsupported("an MTU of 1500"));
    }
    let (tx_depth, rx_depth) = if has(admin::MAX_QUEUES_EXT) {
        let ext = admin.get(&card, admin::MAX_QUEUES_EXT, admin::MAX_QUEUES_EXT_VERSION, delay)?;
        if ext[8] != admin::MAX_QUEUES_EXT_VERSION {
            return Err(Error::Device("it answered for its queues in another version"));
        }
        (get32(&ext, 28).min(get32(&ext, 32)), get32(&ext, 36).min(get32(&ext, 40)))
    } else if has(admin::MAX_QUEUES_NUM) {
        let queues = admin.get(&card, admin::MAX_QUEUES_NUM, 0, delay)?;
        let depth = get32(&queues, 12).min(get32(&queues, 20));
        (depth, depth)
    } else {
        (DEPTH as u32, DEPTH as u32)
    };
    let depth = power_of_two_at_most(tx_depth.min(rx_depth).min(DEPTH as u32) as usize);
    if depth < MIN_DEPTH {
        return Err(Error::Unsupported("queues of 16 descriptors"));
    }
    if has(admin::AENQ_CONFIG) {
        let groups = admin.get(&card, admin::AENQ_CONFIG, 0, delay)?;
        admin.set(&card, admin::AENQ_CONFIG, &[0, get32(&groups, 8) & event::WANTED], delay)?;
    }
    if has(admin::MTU) {
        admin.set(&card, admin::MTU, &[MTU], delay)?;
    }
    let mut link = Link::default();
    if has(admin::LINK_CONFIG) {
        let config = admin.get(&card, admin::LINK_CONFIG, 0, delay)?;
        link.mbps = get32(&config, 8);
        link.full_duplex = get32(&config, 16) & (1 << 1) != 0;
    }

    // The queue pair: each completion queue, then its submission queue.
    let register = |offset: u32| {
        let offset = offset as usize;
        (offset.is_multiple_of(4) && offset + 4 <= card.regs.len())
            .then_some(offset)
            .ok_or(Error::Device("it named a doorbell outside its registers"))
    };
    let mut queue_pair = |direction: u8, cq: usize, entry: usize, sq: usize| {
        let mut command = [0u8; ENTRY];
        command[2] = admin::CREATE_CQ;
        command[4] = admin::INTERRUPT_MODE;
        command[5] = (entry / 4) as u8;
        command[6..8].copy_from_slice(&(depth as u16).to_le_bytes());
        // The queue pair's vector, after the admin queue's.
        put32(&mut command, 8, 1);
        let address = queues.phys() + cq as u64;
        put32(&mut command, 12, address as u32);
        command[16..18].copy_from_slice(&((address >> 32) as u16).to_le_bytes());
        let created = admin.run(&card, command, delay)?;
        let index = get16(&created, 8);
        if usize::from(get16(&created, 10)) != depth {
            return Err(Error::Device("it made a completion queue of another depth"));
        }
        let head_doorbell = match get32(&created, 16) {
            0 => None,
            offset => Some(register(offset)?),
        };
        let done = Cq {
            base: cq,
            entry,
            head: 0,
            phase: true,
            head_doorbell,
            told: 0,
            unmask: register(get32(&created, 20))?,
        };
        let mut command = [0u8; ENTRY];
        command[2] = admin::CREATE_SQ;
        command[4] = direction << 5;
        command[6] = admin::PLACEMENT_HOST;
        command[7] = admin::PHYSICALLY_CONTIGUOUS;
        command[8..10].copy_from_slice(&index.to_le_bytes());
        command[10..12].copy_from_slice(&(depth as u16).to_le_bytes());
        let address = queues.phys() + sq as u64;
        put32(&mut command, 12, address as u32);
        command[16..18].copy_from_slice(&((address >> 32) as u16).to_le_bytes());
        let created = admin.run(&card, command, delay)?;
        let submit = Sq {
            base: sq,
            tail: 0,
            phase: true,
            doorbell: register(get32(&created, 12))?,
            ring: false,
        };
        Ok((submit, done))
    };
    let (tx, tx_done) = queue_pair(admin::DIRECTION_TX, TX_CQ, 8, TX_SQ)?;
    let (rx, rx_done) = queue_pair(admin::DIRECTION_RX, RX_CQ, 16, RX_SQ)?;

    let mut tx_free = [0u16; DEPTH];
    for (i, slot) in tx_free.iter_mut().enumerate().take(depth - 1) {
        // Taken from the top, so the first frame takes slot 0.
        *slot = (depth - 2 - i) as u16;
    }
    let info = Info {
        model: "ENA",
        mac,
        // One receive descriptor is left out, as ena-com leaves it, and
        // one transmit one.
        receive_slots: (depth - 1) as u16,
        transmit_slots: (depth - 1) as u16,
    };
    let mut ena = Ena {
        card,
        queues,
        depth,
        tx_slots,
        rx_slots,
        tx,
        tx_done,
        rx,
        rx_done,
        tx_free,
        tx_free_count: depth - 1,
        tx_busy: [false; DEPTH],
        rx_posted: [false; DEPTH],
        aenq_head: AENQ_DEPTH as u16,
        aenq_phase: true,
        info,
        link,
        interrupt,
        fatal: false,
        dropped: 0,
        stopped: false,
    };
    // The event queue, from its first entry; the card tells of its link
    // there.
    ena.card.regs.write32(reg::AENQ_HEAD_DB, AENQ_DEPTH as u32);
    for slot in 0..depth - 1 {
        ena.post(slot);
    }
    ena.flush();
    if interrupt == Interrupt::Msi {
        ena.unmask();
    }
    Ok(ena)
}

/// The largest power of two no larger than `n`, or 0.
fn power_of_two_at_most(n: usize) -> usize {
    if n == 0 { 0 } else { 1 << (usize::BITS - 1 - n.leading_zeros()) }
}

impl Ena {
    /// Gives the card receive slot `slot` to fill.
    fn post(&mut self, slot: usize) {
        let at = self.rx.base + self.rx.index(self.depth) * 16;
        let address = self.rx_slots.phys(slot);
        let mut descriptor = [0u8; 16];
        descriptor[0..2].copy_from_slice(&(SLOT as u16).to_le_bytes());
        descriptor[3] =
            rx::FIRST | rx::LAST | rx::COMP_REQ | if self.rx.phase { rx::PHASE } else { 0 };
        descriptor[4..6].copy_from_slice(&(slot as u16).to_le_bytes());
        put32(&mut descriptor, 8, address as u32);
        descriptor[12..14].copy_from_slice(&((address >> 32) as u16).to_le_bytes());
        self.queues.write(at, &descriptor);
        self.rx_posted[slot] = true;
        self.rx.advance(self.depth);
    }

    /// Takes back the transmit slots of the frames the card sent.
    fn reclaim(&mut self) {
        while !self.fatal {
            let at = self.tx_done.at(self.depth);
            if (self.queues.read8(at + 3) & 1 != 0) != self.tx_done.phase {
                break;
            }
            device_barrier();
            let slot = usize::from(self.queues.read16(at));
            self.tx_done.advance(self.depth);
            if slot >= self.depth || !self.tx_busy[slot] {
                self.fatal = true;
                break;
            }
            self.tx_busy[slot] = false;
            self.tx_free[self.tx_free_count] = slot as u16;
            self.tx_free_count += 1;
        }
    }

    /// The next received frame, copied into `frame`: `None` when the card
    /// has none, `Some(None)` for one dropped.
    fn next_received(&mut self, frame: &mut [u8]) -> Option<Option<usize>> {
        if self.fatal {
            return None;
        }
        let at = self.rx_done.at(self.depth);
        let status = self.queues.read32(at);
        if (status & rx::DONE_PHASE != 0) != self.rx_done.phase {
            return None;
        }
        device_barrier();
        let len = usize::from(self.queues.read16(at + 4));
        let slot = usize::from(self.queues.read16(at + 6));
        let offset = usize::from(self.queues.read8(at + 14));
        self.rx_done.advance(self.depth);
        if slot >= self.depth || !self.rx_posted[slot] {
            self.fatal = true;
            return None;
        }
        self.rx_posted[slot] = false;
        let whole = status & rx::DONE_FIRST != 0 && status & rx::DONE_LAST != 0;
        let fits = (MIN_FRAME..=MAX_FRAME).contains(&len) && len <= frame.len();
        let taken = if whole && fits && offset + len <= SLOT {
            self.rx_slots.read_at(slot, offset, &mut frame[..len]);
            Some(len)
        } else {
            // A frame in more than one buffer ends with the last.
            if status & rx::DONE_LAST != 0 {
                self.dropped += 1;
            }
            None
        };
        self.post(slot);
        Some(taken)
    }

    /// Takes the card's events: its link, and its errors.
    fn events(&mut self) {
        let mut taken = false;
        loop {
            let at = AENQ + usize::from(self.aenq_head) % AENQ_DEPTH * ENTRY;
            if (self.card.memory.read8(at + 4) & 1 != 0) != self.aenq_phase {
                break;
            }
            device_barrier();
            match self.card.memory.read16(at) {
                event::LINK_CHANGE => self.link.up = self.card.memory.read32(at + 16) & 1 != 0,
                event::FATAL_ERROR | event::DEVICE_REQUEST_RESET => self.fatal = true,
                _ => {}
            }
            self.aenq_head = self.aenq_head.wrapping_add(1);
            if usize::from(self.aenq_head) % AENQ_DEPTH == 0 {
                self.aenq_phase = !self.aenq_phase;
            }
            taken = true;
        }
        if taken {
            self.card.regs.write32(reg::AENQ_HEAD_DB, u32::from(self.aenq_head));
        }
    }

    /// Lets the queue pair's interrupt through again.
    fn unmask(&self) {
        self.card.regs.write32(self.tx_done.unmask, UNMASK);
    }

    /// Whether a received frame waits.
    fn received(&self) -> bool {
        let status = self.queues.read32(self.rx_done.at(self.depth));
        (status & rx::DONE_PHASE != 0) == self.rx_done.phase
    }
}

impl Nic for Ena {
    fn info(&self) -> Info {
        self.info
    }

    fn link(&self) -> Link {
        if self.link.up { self.link } else { Link::default() }
    }

    fn failed(&self) -> bool {
        self.fatal
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
            if pass == 1 || self.fatal {
                break;
            }
            // Nothing more: the card's events, then its interrupt, and a
            // last look for a frame that came in between.
            self.events();
            if self.interrupt != Interrupt::Msi || self.fatal {
                break;
            }
            self.unmask();
            if !self.received() {
                break;
            }
        }
        None
    }

    fn can_send(&mut self) -> bool {
        if self.stopped {
            return false;
        }
        self.reclaim();
        !self.fatal && self.tx_free_count > 0
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) || !self.can_send() {
            return false;
        }
        self.tx_free_count -= 1;
        let slot = usize::from(self.tx_free[self.tx_free_count]);
        self.tx_busy[slot] = true;
        self.tx_slots.write(slot, frame);
        let address = self.tx_slots.phys(slot);
        let control = frame.len() as u32
            | ((slot as u32 >> 10) & 0x3f) << 16
            | if self.tx.phase { tx::PHASE } else { 0 }
            | tx::FIRST
            | tx::LAST
            | tx::COMP_REQ;
        let mut descriptor = [0u8; 16];
        put32(&mut descriptor, 0, control);
        put32(&mut descriptor, 4, (slot as u32 & 0x3ff) << 22);
        put32(&mut descriptor, 8, address as u32);
        put32(&mut descriptor, 12, (address >> 32) as u32 & 0xffff);
        let at = self.tx.base + self.tx.index(self.depth) * 16;
        self.queues.write(at, &descriptor);
        self.tx.advance(self.depth);
        true
    }

    fn flush(&mut self) {
        if self.stopped {
            return;
        }
        device_barrier();
        for queue in [&mut self.tx, &mut self.rx] {
            if queue.ring {
                queue.ring = false;
                self.card.regs.write32(queue.doorbell, u32::from(queue.tail));
            }
        }
        // The card is told how far its completions were taken, as
        // ena-com tells it, once a quarter of a queue's worth is.
        let threshold = (self.depth / 4) as u16;
        for done in [&mut self.tx_done, &mut self.rx_done] {
            if let Some(doorbell) = done.head_doorbell
                && done.head.wrapping_sub(done.told) >= threshold
            {
                done.told = done.head;
                self.card.regs.write32(doorbell, u32::from(done.head));
            }
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
        self.card.halt();
    }
}

impl Drop for Ena {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
