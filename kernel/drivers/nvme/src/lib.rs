// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! NVMe, the controller of solid-state drives on PCI Express (the *NVM
//! Express Base Specification 2.0* and the *NVM Command Set Specification
//! 1.0*), for controllers of version 1.0 and later.
//!
//! An NVMe controller is one PCI function with its registers in BAR 0:
//! its capabilities and configuration, then a doorbell for each queue. It
//! takes commands through queues in memory: an admin queue pair, for
//! setting it up and asking what it is, and as many I/O queue pairs as
//! the host makes, each a submission queue of commands and a completion
//! queue of answers, with its own interrupt. Its storage is in
//! *namespaces*, each numbered blocks, which the commands name.
//!
//! - [`Nvme`] takes the controller over: disables it, gives it an admin
//!   queue, enables it, asks what it is ([`identify`]), asks for I/O
//!   queues, lists its namespaces, makes a [`Lane`] for each queue pair,
//!   and resets the controller and makes its queues again after an error.
//!   It shuts the controller down before the machine is powered off.
//! - [`Lane`] runs one I/O queue pair: reads, writes (with Force Unit
//!   Access for writes through the cache), flushes and deallocations, up
//!   to 63 in flight, their data in PRP lists ([`command::prps`]).
//! - [`command`] builds the commands and reads their completions.
//! - [`Shadow`] holds the shadow doorbells of a controller that takes
//!   them, as ones that hypervisors emulate may: a lane writes its
//!   doorbell registers, each an exit from the virtual machine, only when
//!   the controller asks.
//!
//! The crate is a compartment: it has no `unsafe` code, and reaches the
//! controller only through the [`Mmio`] windows and [`Dma`] memory that
//! `radix-hw` gives it. Nothing here sleeps or takes a lock: the kernel
//! passes in how to wait (`delay`) where the controller needs time, and
//! admin commands are polled. The tests run the driver against a
//! simulated controller on the host, and Kani proves that reading what a
//! controller says cannot go wrong, whatever it says, and that a command's
//! PRP lists describe its buffers.

#![cfg_attr(not(any(test, kani)), no_std)]
#![forbid(unsafe_code)]

pub mod command;
pub mod identify;
mod lane;
mod shadow;

#[cfg(test)]
mod tests;

use core::fmt;

use command::{COMMAND_SIZE, COMPLETION_SIZE, Command, Completion, Status, cns};
pub use lane::{DEPTH, ENTRIES, Lane, MEMORY_PAGES, SLOTS};
use radix_hw::{Dma, Mmio, device_barrier};
pub use shadow::Shadow;

/// The PCI class, subclass and programming interface of an NVMe
/// controller: mass storage, non-volatile memory, NVM Express.
pub const PCI_CLASS: (u8, u8, u8) = (0x01, 0x08, 0x02);
/// The BAR that holds the controller's registers.
pub const BAR: usize = 0;

/// The controller's registers (3.1.3).
mod reg {
    /// Capabilities, 64 bits.
    pub const CAP: usize = 0x00;
    /// Version.
    pub const VS: usize = 0x08;
    /// Configuration.
    pub const CC: usize = 0x14;
    /// Status.
    pub const CSTS: usize = 0x1c;
    /// Admin queue attributes: its two queues' sizes.
    pub const AQA: usize = 0x24;
    /// Admin submission and completion queue addresses, 64 bits each.
    pub const ASQ: usize = 0x28;
    pub const ACQ: usize = 0x30;
    /// Where the doorbells start.
    pub const DOORBELLS: usize = 0x1000;

    /// CC: enable.
    pub const CC_EN: u32 = 1 << 0;
    /// CC: 64-byte submission and 16-byte completion queue entries, the
    /// NVM command set, 4 KiB pages and round-robin arbitration (all 0).
    pub const CC_ENTRIES: u32 = (6 << 16) | (4 << 20);
    /// CC: a normal shutdown.
    pub const CC_SHUTDOWN: u32 = 1 << 14;
    /// CC's shutdown field.
    pub const CC_SHN: u32 = 3 << 14;
    /// CSTS: ready.
    pub const CSTS_RDY: u32 = 1 << 0;
    /// CSTS: the controller failed.
    pub const CSTS_CFS: u32 = 1 << 1;
    /// CSTS's shutdown status field, and its value once done.
    pub const CSTS_SHST: u32 = 3 << 2;
    pub const CSTS_SHUTDOWN_DONE: u32 = 2 << 2;
}

/// Entries in each admin queue.
const ADMIN_ENTRIES: u16 = 32;
/// The most namespaces of a controller the driver takes.
pub const MAX_NAMESPACES: usize = 64;
/// Where the admin queues and the buffer for Identify data are in the
/// admin memory, a page each.
const ASQ: usize = 0;
const ACQ: usize = 4096;
const BUFFER: usize = 8192;
const ADMIN_PAGES: usize = 3;

/// How long an admin command may take, and how often the driver looks.
const ADMIN_US: u32 = 10_000_000;
const ADMIN_POLL_US: u32 = 10;
/// How often the driver looks while the controller is enabled or
/// disabled.
const READY_POLL_US: u32 = 1_000;
/// The shortest and longest waits for a shutdown, in microseconds.
const SHUTDOWN_MIN_US: u32 = 5_000_000;
const SHUTDOWN_MAX_US: u32 = 60_000_000;

/// What a request does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Reads blocks into the buffers.
    Read,
    /// Writes the buffers to blocks.
    Write,
    /// Writes the buffers to blocks with Force Unit Access: durable when
    /// done.
    WriteThrough,
    /// Makes every completed write to the namespace durable.
    Flush,
    /// Deallocates this many blocks: the drive may forget what they hold.
    Deallocate(u64),
}

/// How a request ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It worked.
    Done,
    /// The controller failed it for good.
    Failed,
    /// It failed in a way worth trying again, or the controller was reset
    /// while it was in flight: issue it again.
    Retry,
}

/// What can go wrong with the controller or a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The controller's registers are too small for what it says it has.
    Registers,
    /// It does not do what the driver needs: the NVM command set, 4 KiB
    /// memory pages.
    Unsupported,
    /// It did not become ready, or stop, in the time it says it takes.
    Timeout,
    /// It says it failed (CSTS.CFS), or it is gone (its registers read
    /// all ones).
    Failed,
    /// An admin command failed with this status.
    Command(Status),
    /// An admin command was not answered.
    NoAnswer,
    /// No memory for its queues.
    NoMemory,
    /// The request is malformed: not whole blocks, too many buffers, or
    /// buffers a PRP list cannot describe.
    BadRequest,
    /// The lane has no room for the request now; try once a request in
    /// flight is done.
    Full,
    /// The controller is gone, or stopped answering.
    Dead,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Registers => f.write_str("its registers are too small"),
            Error::Unsupported => {
                f.write_str("it lacks the NVM command set or 4 KiB pages, which are needed")
            }
            Error::Timeout => f.write_str("it did not become ready in the time it gives"),
            Error::Failed => f.write_str("it says it failed"),
            Error::Command(status) => write!(
                f,
                "a command failed: {} (type {}, code {:#04x})",
                status.describe(),
                status.kind(),
                status.code()
            ),
            Error::NoAnswer => f.write_str("a command went unanswered"),
            Error::NoMemory => f.write_str("out of memory"),
            Error::BadRequest => f.write_str("a malformed request"),
            Error::Full => f.write_str("no room for the request"),
            Error::Dead => f.write_str("the controller stopped answering"),
        }
    }
}

/// A namespace the controller has, and what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Namespace {
    /// Its ID, which commands name.
    pub id: u32,
    /// What it is.
    pub info: identify::Namespace,
}

/// Waits up to `limit_us` microseconds, `step_us` at a time, for `done`.
fn wait_for(
    delay: &mut dyn FnMut(u32),
    limit_us: u32,
    step_us: u32,
    mut done: impl FnMut() -> bool,
) -> bool {
    let mut waited = 0u32;
    loop {
        if done() {
            return true;
        }
        if waited >= limit_us {
            return false;
        }
        delay(step_us);
        waited = waited.saturating_add(step_us);
    }
}

/// An NVMe controller, taken over and enabled.
pub struct Nvme {
    regs: Mmio,
    cap: u64,
    version: u32,
    /// The admin queues and the buffer for Identify data.
    admin: Dma,
    tail: u16,
    head: u16,
    phase: bool,
    /// The next admin command's identifier.
    next_id: u16,
    identity: identify::Controller,
    /// The most blocks one deallocation covers.
    deallocate_blocks: u32,
    /// The I/O queue pairs the controller granted.
    granted: u16,
    /// The shadow doorbells it was given, if it takes them.
    shadow: Option<&'static Shadow>,
}

impl fmt::Debug for Nvme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Nvme")
            .field("cap", &format_args!("{:#x}", self.cap))
            .field("version", &format_args!("{:#x}", self.version))
            .finish_non_exhaustive()
    }
}

/// Only a controller whose setup failed is dropped: it is disabled, so
/// that it stops using the memory it was given.
impl Drop for Nvme {
    fn drop(&mut self) {
        let config = self.regs.read32(reg::CC);
        self.regs.write32(reg::CC, config & !reg::CC_EN);
    }
}

impl Nvme {
    /// Takes over the controller whose registers are `regs`: disables it,
    /// gives it an admin queue, enables it and asks what it is. `alloc`
    /// gives zeroed DMA memory in pages, and `delay` waits the given
    /// number of microseconds.
    pub fn new(
        regs: Mmio,
        alloc: &mut dyn FnMut(usize) -> Option<Dma>,
        delay: &mut dyn FnMut(u32),
    ) -> Result<Nvme, Error> {
        if regs.len() < reg::DOORBELLS + 8 {
            return Err(Error::Registers);
        }
        // Two halves, low first, as some controllers take only 32-bit
        // accesses.
        let cap = u64::from(regs.read32(reg::CAP)) | (u64::from(regs.read32(reg::CAP + 4)) << 32);
        // The NVM command set (CSS bit 0), and 4 KiB pages within the
        // ones it takes (MPSMIN 0).
        let nvm = (cap >> 37) & 1 != 0;
        let min_page = (cap >> 48) & 0xf;
        if cap == u64::MAX || !nvm || min_page != 0 {
            return Err(Error::Unsupported);
        }
        let admin = alloc(ADMIN_PAGES).ok_or(Error::NoMemory)?;
        let version = regs.read32(reg::VS);
        let mut nvme = Nvme {
            regs,
            cap,
            version,
            admin,
            tail: 0,
            head: 0,
            phase: true,
            next_id: 0,
            identity: identify::Controller::UNKNOWN,
            deallocate_blocks: u32::MAX,
            granted: 0,
            shadow: None,
        };
        nvme.enable(delay)?;
        let buffer = nvme.admin.phys() + BUFFER as u64;
        nvme.command(Command::identify(cns::CONTROLLER, 0, buffer), delay)?;
        let mut data = [0u8; identify::HEAD];
        nvme.admin.read(BUFFER, &mut data);
        nvme.identity = identify::controller(&data);
        // The most blocks one range covers, which controllers of 1.4 and
        // later may limit; one that does not answer has no limit.
        if nvme.version >= 0x0001_0400 && nvme.identity.deallocate {
            let mut ask = Command::identify(cns::NVM_CONTROLLER, 0, buffer);
            ask.dwords[1] = 0;
            if nvme.command(ask, delay).is_ok() {
                nvme.admin.read(BUFFER, &mut data);
                match identify::range_limit(&data) {
                    0 => {}
                    limit => nvme.deallocate_blocks = limit,
                }
            }
        }
        Ok(nvme)
    }

    /// The most entries its queues take (MQES + 1).
    fn max_entries(&self) -> u32 {
        (self.cap & 0xffff) as u32 + 1
    }

    /// How long it says it takes to become ready or to stop, in
    /// microseconds (CAP.TO, in half seconds).
    fn ready_us(&self) -> u32 {
        let units = ((self.cap >> 24) & 0xff).max(1) as u32;
        units * 500_000
    }

    /// The space between doorbells, in bytes.
    fn stride(&self) -> usize {
        4 << ((self.cap >> 32) & 0xf)
    }

    /// The tail doorbell of submission queue `queue` and the head doorbell
    /// of completion queue `queue`, if its registers reach that far.
    fn doorbells(&self, queue: u16) -> Option<(Mmio, Mmio)> {
        let stride = self.stride();
        let sq = reg::DOORBELLS + 2 * usize::from(queue) * stride;
        Some((self.regs.window(sq, 4)?, self.regs.window(sq + stride, 4)?))
    }

    /// Its NVMe version, as (major, minor, tertiary).
    pub fn version(&self) -> (u16, u8, u8) {
        ((self.version >> 16) as u16, (self.version >> 8) as u8, self.version as u8)
    }

    /// What it says it is.
    pub fn identity(&self) -> &identify::Controller {
        &self.identity
    }

    /// The most blocks one deallocation covers.
    pub fn deallocate_blocks(&self) -> u32 {
        self.deallocate_blocks
    }

    /// Reads the SMART / Health Information log ([`command::LOG_HEALTH`])
    /// into `out`: what the controller says of its temperature, spare
    /// capacity, wear and errors. An admin command, polled.
    pub fn health(
        &mut self,
        delay: &mut dyn FnMut(u32),
        out: &mut [u8; command::LOG_HEALTH_SIZE],
    ) -> Result<(), Error> {
        let buffer = self.admin.phys() + BUFFER as u64;
        let size = command::LOG_HEALTH_SIZE as u32;
        self.command(Command::log_page(command::LOG_HEALTH, size, buffer), delay)?;
        device_barrier();
        self.admin.read(BUFFER, out);
        Ok(())
    }

    /// Whether it says it failed, or is gone: its registers read all ones
    /// once a device has left the bus.
    pub fn failed(&self) -> bool {
        let status = self.regs.read32(reg::CSTS);
        status == u32::MAX || status & reg::CSTS_CFS != 0
    }

    /// Disables the controller, gives it its admin queues and enables it
    /// again: it forgets every I/O queue and what was in them.
    fn enable(&mut self, delay: &mut dyn FnMut(u32)) -> Result<(), Error> {
        let limit = self.ready_us();
        let regs = self.regs.clone();
        let config = regs.read32(reg::CC);
        if config & reg::CC_EN != 0 {
            regs.write32(reg::CC, config & !reg::CC_EN);
        }
        // A controller being disabled stays ready until it is done.
        let stopped = wait_for(delay, limit, READY_POLL_US, || {
            let status = regs.read32(reg::CSTS);
            status != u32::MAX && status & reg::CSTS_RDY == 0
        });
        if !stopped {
            return Err(if self.failed() { Error::Failed } else { Error::Timeout });
        }
        self.admin.zero();
        self.tail = 0;
        self.head = 0;
        self.phase = true;
        let size = u32::from(ADMIN_ENTRIES - 1);
        regs.write32(reg::AQA, size | (size << 16));
        for (at, phys) in [(reg::ASQ, ASQ), (reg::ACQ, ACQ)] {
            let phys = self.admin.phys() + phys as u64;
            regs.write32(at, phys as u32);
            regs.write32(at + 4, (phys >> 32) as u32);
        }
        device_barrier();
        regs.write32(reg::CC, reg::CC_ENTRIES | reg::CC_EN);
        let mut failed = false;
        let ready = wait_for(delay, limit, READY_POLL_US, || {
            let status = regs.read32(reg::CSTS);
            failed = status == u32::MAX || status & reg::CSTS_CFS != 0;
            failed || status & reg::CSTS_RDY != 0
        });
        match (ready, failed) {
            (_, true) => Err(Error::Failed),
            (false, _) => Err(Error::Timeout),
            (true, false) => Ok(()),
        }
    }

    /// Sends an admin command and waits for its answer: its result.
    fn command(&mut self, mut command: Command, delay: &mut dyn FnMut(u32)) -> Result<u32, Error> {
        let (sq, cq) = self.doorbells(0).ok_or(Error::Registers)?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        command.id = id;
        self.admin.write(ASQ + usize::from(self.tail) * COMMAND_SIZE, &command.bytes());
        self.tail = (self.tail + 1) % ADMIN_ENTRIES;
        device_barrier();
        sq.write32(0, u32::from(self.tail));
        let mut answer = None;
        let answered = wait_for(delay, ADMIN_US, ADMIN_POLL_US, || {
            // Older answers, to commands given up on, are read past.
            while answer.is_none() {
                let at = ACQ + usize::from(self.head) * COMPLETION_SIZE;
                if (self.admin.read32(at + 12) & (1 << 16) != 0) != self.phase {
                    break;
                }
                device_barrier();
                let mut bytes = [0u8; COMPLETION_SIZE];
                self.admin.read(at, &mut bytes);
                self.head = (self.head + 1) % ADMIN_ENTRIES;
                if self.head == 0 {
                    self.phase = !self.phase;
                }
                cq.write32(0, u32::from(self.head));
                let entry = Completion::parse(&bytes);
                if entry.id == id {
                    answer = Some(entry);
                }
            }
            answer.is_some()
        });
        match answer {
            Some(entry) if entry.status.worked() => Ok(entry.result),
            Some(entry) => Err(Error::Command(entry.status)),
            None if !answered && self.failed() => Err(Error::Failed),
            None => Err(Error::NoAnswer),
        }
    }

    /// Asks for `wanted` I/O queue pairs; returns how many the controller
    /// granted, at least one.
    pub fn request_queues(
        &mut self,
        wanted: u16,
        delay: &mut dyn FnMut(u32),
    ) -> Result<u16, Error> {
        let result = self.command(Command::set_queues(wanted.max(1)), delay)?;
        let submission = (result & 0xffff) as u16;
        let completion = (result >> 16) as u16;
        self.granted = submission.min(completion).saturating_add(1).min(wanted.max(1));
        Ok(self.granted)
    }

    /// Gives the controller `shadow` doorbells, once it has granted I/O
    /// queues ([`Nvme::request_queues`]) and before its lanes are made: the
    /// lanes write their doorbell registers only when it asks. Fails if it
    /// does not take them ([`identify::Controller::shadow_doorbells`]), or
    /// has more queues than the shadow holds, or refuses them, and then
    /// its lanes write their registers every time.
    pub fn use_shadow(
        &mut self,
        shadow: &'static Shadow,
        delay: &mut dyn FnMut(u32),
    ) -> Result<(), Error> {
        self.shadow = None;
        // Each queue pair's two doorbells, the admin queue's first.
        let room = 2 * (usize::from(self.granted) + 1) * self.stride();
        if !self.identity.shadow_doorbells || self.granted == 0 || room > Shadow::ROOM {
            return Err(Error::Unsupported);
        }
        self.give_shadow(shadow, delay)?;
        self.shadow = Some(shadow);
        Ok(())
    }

    /// Sends Doorbell Buffer Config, with `shadow` zeroed.
    fn give_shadow(&mut self, shadow: &Shadow, delay: &mut dyn FnMut(u32)) -> Result<(), Error> {
        shadow.clear();
        let (doorbells, events) = shadow.addresses();
        self.command(Command::doorbell_buffer(doorbells, events), delay).map(|_| ())
    }

    /// Whether its lanes use shadow doorbells.
    pub fn shadowed(&self) -> bool {
        self.shadow.is_some()
    }

    /// Calls `found` with each active namespace and what it is, or why the
    /// driver cannot take it, up to the first [`MAX_NAMESPACES`]. A
    /// controller before version 1.1, which does not list its active
    /// namespaces, has each ID up to the highest one it has asked, and
    /// those it has not made are left out.
    pub fn namespaces(
        &mut self,
        delay: &mut dyn FnMut(u32),
        found: &mut dyn FnMut(u32, Result<Namespace, identify::Unusable>),
    ) -> Result<(), Error> {
        let buffer = self.admin.phys() + BUFFER as u64;
        let mut data = [0u8; identify::HEAD];
        let mut ids = [0u32; MAX_NAMESPACES];
        let mut count = 0;
        if self.version >= 0x0001_0100 {
            self.command(Command::identify(cns::ACTIVE_NAMESPACES, 0, buffer), delay)?;
            let mut last = 0;
            while count < ids.len() {
                let id = self.admin.read32(BUFFER + 4 * count);
                if !identify::continues(last, id) {
                    break;
                }
                ids[count] = id;
                last = id;
                count += 1;
            }
        } else {
            let highest = self.identity.namespaces.min(ids.len() as u32);
            for id in 1..=highest {
                ids[count] = id;
                count += 1;
            }
        }
        for &id in &ids[..count] {
            match self.command(Command::identify(cns::NAMESPACE, id, buffer), delay) {
                Ok(_) => {}
                // An ID not in use on an older controller.
                Err(Error::Command(_)) => continue,
                Err(err) => return Err(err),
            }
            self.admin.read(BUFFER, &mut data);
            match identify::namespace(&data, self.identity.atomic_blocks) {
                Err(identify::Unusable::Empty) => {}
                answer => found(id, answer.map(|info| Namespace { id, info })),
            }
        }
        Ok(())
    }

    /// Makes I/O queue pair `id` (from 1 to what [`Nvme::request_queues`]
    /// granted) as a lane, interrupting with MSI-X entry `vector` if it
    /// has one.
    pub fn lane(
        &mut self,
        id: u16,
        vector: Option<u16>,
        alloc: &mut dyn FnMut(usize) -> Option<Dma>,
        delay: &mut dyn FnMut(u32),
    ) -> Result<Lane, Error> {
        if id == 0 || id > self.granted || u32::from(ENTRIES) > self.max_entries() {
            return Err(Error::BadRequest);
        }
        let doorbells = self.doorbells(id).ok_or(Error::Registers)?;
        let memory = alloc(MEMORY_PAGES).ok_or(Error::NoMemory)?;
        let mut lane =
            Lane::new(memory, doorbells, id, vector, self.identity.max_pages, self.identity.sgls);
        self.remake(&mut lane, delay)?;
        Ok(lane)
    }

    /// Creates `lane`'s queues on the controller: when the lane is made,
    /// and again after [`Nvme::restart`]. The lane writes its doorbells to
    /// the shadow doorbells the controller has now, if any.
    pub fn remake(&mut self, lane: &mut Lane, delay: &mut dyn FnMut(u32)) -> Result<(), Error> {
        let (sq, cq) = lane.queues();
        let id = lane.id();
        // Where the lane's doorbells are in the shadow, as in the
        // registers; `use_shadow` checked that they fit.
        let stride = self.stride();
        let at = 2 * usize::from(id) * stride;
        lane.use_shadow(self.shadow.map(|shadow| (shadow, at, at + stride)));
        self.command(Command::create_cq(id, ENTRIES, cq, lane.vector()), delay)?;
        if let Err(err) = self.command(Command::create_sq(id, ENTRIES, sq), delay) {
            let _ = self.command(Command::delete(id, true), delay);
            return Err(err);
        }
        Ok(())
    }

    /// Resets the controller after an error or a request that went
    /// unanswered: it forgets its I/O queues, and gets as many again.
    /// Each lane is to be restarted first ([`Lane::restart`], so that every
    /// request in flight goes again), and made again after
    /// ([`Nvme::remake`]); if the controller does not come back, each is
    /// to be killed ([`Lane::kill`]), and its requests fail. The
    /// controller forgets its shadow doorbells too, and is given them
    /// again; if it refuses them now, its lanes write their registers.
    pub fn restart(&mut self, delay: &mut dyn FnMut(u32)) -> Result<(), Error> {
        self.enable(delay)?;
        let wanted = self.granted;
        if self.request_queues(wanted, delay)? < wanted {
            return Err(Error::Unsupported);
        }
        if let Some(shadow) = self.shadow.take() {
            match self.give_shadow(shadow, delay) {
                Ok(()) => self.shadow = Some(shadow),
                Err(Error::Command(_)) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// Tells the controller the machine is about to lose power, and waits
    /// for it to finish (a normal shutdown): it writes out its cache and
    /// what it keeps of itself. Returns whether it finished in the time it
    /// says it takes, at least 5 and at most 60 seconds. It takes no
    /// commands afterwards.
    pub fn shutdown(&mut self, delay: &mut dyn FnMut(u32)) -> bool {
        let regs = self.regs.clone();
        let config = regs.read32(reg::CC);
        regs.write32(reg::CC, (config & !reg::CC_SHN) | reg::CC_SHUTDOWN);
        let limit = self.identity.shutdown_us.clamp(SHUTDOWN_MIN_US, SHUTDOWN_MAX_US);
        wait_for(delay, limit, READY_POLL_US, || {
            let status = regs.read32(reg::CSTS);
            status == u32::MAX || status & reg::CSTS_SHST == reg::CSTS_SHUTDOWN_DONE
        })
    }
}
