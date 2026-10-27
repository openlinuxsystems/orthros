// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! One I/O queue pair: a submission queue the driver writes commands
//! into, and the completion queue the controller answers in (NVM Express
//! Base Specification 2.0, 3.3).
//!
//! A lane's memory is one block of DMA memory: the submission queue
//! ([`ENTRIES`] commands, a page), the completion queue (a page, of which
//! it uses a quarter), and a kibibyte for each request slot: its PRP list,
//! or a Dataset Management range. A slot's number is its command's
//! identifier, so a completion names the slot it finishes. At most
//! [`DEPTH`] commands are in flight, one fewer than the queue's entries:
//! a full queue would look empty.
//!
//! The driver writes a command at the queue's tail and moves the tail
//! doorbell past it; the controller writes a completion at the
//! completion queue's head, with the phase tag that tells a new entry
//! from the last pass's, and the driver moves the head doorbell past the
//! entries it has read. With shadow doorbells ([`Shadow`]), a doorbell
//! moves in memory, and its register is written only when the controller
//! asks.

use radix_hw::{Dma, Mmio, device_barrier};

use crate::command::{self, COMMAND_SIZE, COMPLETION_SIZE, Command, Completion, LIST_ENTRIES};
use crate::shadow::Shadow;
use crate::{Error, Op, Outcome};

/// Entries in each queue of a lane.
pub const ENTRIES: u16 = 64;
/// Commands in flight at once.
pub const DEPTH: u32 = ENTRIES as u32 - 1;
/// Request slots: each queue entry's.
pub const SLOTS: usize = ENTRIES as usize;

const SQ: usize = 0;
const CQ: usize = 4096;
const LISTS: usize = 8192;
const LIST_SIZE: usize = LIST_ENTRIES * 8;
/// Pages of DMA memory a lane takes.
pub const MEMORY_PAGES: usize = (LISTS + SLOTS * LIST_SIZE) / 4096;

/// An I/O queue pair, made on the controller.
pub struct Lane {
    memory: Dma,
    /// The tail doorbell of its submission queue and the head doorbell
    /// of its completion queue.
    sq_doorbell: Mmio,
    cq_doorbell: Mmio,
    /// The controller's shadow doorbells, if it takes them, and where the
    /// lane's two are in them.
    shadow: Option<(&'static Shadow, usize, usize)>,
    /// Its queues' ID.
    id: u16,
    /// Its MSI-X entry, if its completions interrupt.
    vector: Option<u16>,
    /// The most pages one command's data take.
    max_pages: usize,
    /// Whether the controller takes scatter gather lists.
    sgls: bool,
    tail: u16,
    head: u16,
    phase: bool,
    /// Whether the tail moved since its doorbell was last written.
    unrung: bool,
    /// Slots in flight.
    busy: u64,
    /// Finished slots not yet handed out, by how they ended.
    done: u64,
    failed: u64,
    retry: u64,
    /// Whether the controller is gone: every request fails.
    dead: bool,
    /// The status of the last command that failed, for the log, and of
    /// each slot's last command.
    last_error: command::Status,
    statuses: [command::Status; SLOTS],
    errors: u32,
}

impl core::fmt::Debug for Lane {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Lane")
            .field("id", &self.id)
            .field("busy", &format_args!("{:#x}", self.busy))
            .finish_non_exhaustive()
    }
}

impl Lane {
    /// A lane of queues `id` in `memory` ([`MEMORY_PAGES`] zeroed pages),
    /// with these doorbells: made on the controller once the controller
    /// has created its queues ([`crate::Nvme::lane`]).
    pub(crate) fn new(
        memory: Dma,
        doorbells: (Mmio, Mmio),
        id: u16,
        vector: Option<u16>,
        max_pages: usize,
        sgls: bool,
    ) -> Lane {
        Lane {
            memory,
            sq_doorbell: doorbells.0,
            cq_doorbell: doorbells.1,
            shadow: None,
            id,
            vector,
            max_pages,
            sgls,
            tail: 0,
            head: 0,
            phase: true,
            unrung: false,
            busy: 0,
            done: 0,
            failed: 0,
            retry: 0,
            dead: false,
            last_error: command::Status::SUCCESS,
            statuses: [command::Status::SUCCESS; SLOTS],
            errors: 0,
        }
    }

    /// Its queues' ID.
    pub fn id(&self) -> u16 {
        self.id
    }

    /// Its MSI-X entry, if its completions interrupt.
    pub fn vector(&self) -> Option<u16> {
        self.vector
    }

    /// Has the lane's doorbells written to `shadow`, at these offsets, or
    /// to their registers alone: as the controller was set up when its
    /// queues were made.
    pub(crate) fn use_shadow(&mut self, shadow: Option<(&'static Shadow, usize, usize)>) {
        self.shadow = shadow;
    }

    /// The physical addresses of its submission and completion queues.
    pub(crate) fn queues(&self) -> (u64, u64) {
        (self.memory.phys() + SQ as u64, self.memory.phys() + CQ as u64)
    }

    /// The most pages one request's data take.
    pub fn max_pages(&self) -> usize {
        self.max_pages
    }

    /// Whether a request finds room now: a slot of the controller's queue
    /// to go in.
    pub fn room(&self) -> bool {
        self.dead || self.busy.count_ones() < DEPTH
    }

    /// Whether the controller is gone.
    pub fn dead(&self) -> bool {
        self.dead
    }

    /// How many commands failed since boot, and the last one's status.
    pub fn errors(&self) -> (u32, command::Status) {
        (self.errors, self.last_error)
    }

    /// The status the controller gave the last command of `slot`.
    pub fn status(&self, slot: u8) -> command::Status {
        self.statuses.get(usize::from(slot)).copied().unwrap_or(command::Status::SUCCESS)
    }

    /// Starts a request as `slot`: `op` on namespace `namespace`, whose
    /// logical blocks are `lba_bytes` long (512 to 4096), from block
    /// `block`, with `data` buffers (physical address, length) that
    /// together cover whole blocks, each inside a page and all of them
    /// one run of pages ([`command::prps`]); none for a flush or a
    /// deallocation. The doorbell is written at once if `ring`, or by
    /// [`Lane::ring`]. The buffers must stay the request's alone until
    /// [`Lane::complete`] hands the slot back.
    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &mut self,
        slot: u8,
        op: Op,
        namespace: u32,
        lba_bytes: u32,
        block: u64,
        data: &[(u64, u32)],
        ring: bool,
    ) -> Result<(), Error> {
        if self.dead {
            return Err(Error::Dead);
        }
        let index = usize::from(slot);
        if index >= SLOTS || self.busy & (1 << index) != 0 {
            return Err(Error::BadRequest);
        }
        if !self.room() {
            return Err(Error::Full);
        }
        let list_at = LISTS + index * LIST_SIZE;
        let list_phys = self.memory.phys() + list_at as u64;
        let mut command = match op {
            Op::Read | Op::Write | Op::WriteThrough => {
                let bytes: u64 = data.iter().map(|&(_, len)| u64::from(len)).sum();
                let lba = u64::from(lba_bytes);
                if !lba.is_power_of_two() || !(512..=4096).contains(&lba) {
                    return Err(Error::BadRequest);
                }
                let blocks = bytes / lba;
                if data.len() > self.max_pages
                    || !bytes.is_multiple_of(lba)
                    || !(1..=65_536).contains(&blocks)
                {
                    return Err(Error::BadRequest);
                }
                // Every controller takes the same buffers, those PRPs can
                // describe, which keeps them within the transfer limit;
                // long runs of them go as a scatter gather list to those
                // that take one.
                let prps = command::prps(data, list_phys).map_err(|_| Error::BadRequest)?;
                let write = op != Op::Read;
                let fua = op == Op::WriteThrough;
                let mut command = Command::transfer(write, fua, namespace, block, blocks as u32);
                let memory = &self.memory;
                let sgl = if self.sgls {
                    command::sgl(data, list_phys, |i, address, rest| {
                        memory.write64(list_at + 16 * i, address);
                        memory.write64(list_at + 16 * i + 8, rest);
                    })
                } else {
                    None
                };
                (command.prp1, command.prp2) = match sgl {
                    Some(pointer) => {
                        command.sgl = true;
                        pointer
                    }
                    None => {
                        if data.len() > 2 {
                            for (i, &(phys, _)) in data[1..].iter().enumerate() {
                                memory.write64(list_at + 8 * i, phys);
                            }
                        }
                        prps
                    }
                };
                command
            }
            Op::Flush => {
                if !data.is_empty() {
                    return Err(Error::BadRequest);
                }
                Command::flush(namespace)
            }
            Op::Deallocate(blocks) => {
                let blocks = u32::try_from(blocks).map_err(|_| Error::BadRequest)?;
                if !data.is_empty() || blocks == 0 {
                    return Err(Error::BadRequest);
                }
                self.memory.write(list_at, &command::range(block, blocks));
                Command::deallocate(namespace, list_phys)
            }
        };
        command.id = u16::from(slot);
        let at = SQ + usize::from(self.tail) * COMMAND_SIZE;
        for (i, word) in command.bytes().as_chunks::<8>().0.iter().enumerate() {
            self.memory.write64(at + 8 * i, u64::from_le_bytes(*word));
        }
        self.tail = (self.tail + 1) % ENTRIES;
        self.busy |= 1 << index;
        self.unrung = true;
        if ring {
            self.ring();
        }
        Ok(())
    }

    /// Tells the controller of the commands written since it last heard.
    pub fn ring(&mut self) {
        if self.unrung && !self.dead {
            device_barrier();
            let register = match self.shadow {
                Some((shadow, at, _)) => shadow.update(at, self.tail),
                None => true,
            };
            if register {
                self.sq_doorbell.write32(0, u32::from(self.tail));
            }
            self.unrung = false;
        }
    }

    /// The next finished request: its slot and how it ended. Runs in the
    /// lane's interrupt handler, or in threads that poll. Reads every new
    /// completion first, then tells the controller how far it has read.
    pub fn complete(&mut self) -> Option<(u8, Outcome)> {
        if self.done | self.failed | self.retry == 0 && !self.dead {
            self.collect();
        }
        for (set, outcome) in [
            (&mut self.done, Outcome::Done),
            (&mut self.failed, Outcome::Failed),
            (&mut self.retry, Outcome::Retry),
        ] {
            if *set != 0 {
                let slot = set.trailing_zeros();
                *set &= !(1 << slot);
                return Some((slot as u8, outcome));
            }
        }
        None
    }

    /// Takes in the completions the controller has written since it last
    /// looked.
    fn collect(&mut self) {
        let mut moved = false;
        loop {
            let at = CQ + usize::from(self.head) * COMPLETION_SIZE;
            // The phase tag first: the rest of an entry is the
            // controller's until it flips.
            if (self.memory.read32(at + 12) & (1 << 16) != 0) != self.phase {
                break;
            }
            device_barrier();
            let dwords = core::array::from_fn(|i| self.memory.read32(at + 4 * i));
            let entry = Completion::from_dwords(dwords);
            self.head = (self.head + 1) % ENTRIES;
            if self.head == 0 {
                self.phase = !self.phase;
            }
            moved = true;
            let index = usize::from(entry.id);
            // A completion for no command in flight is the controller's
            // mistake: left alone.
            if index >= SLOTS || self.busy & (1 << index) == 0 {
                continue;
            }
            let bit = 1 << index;
            self.busy &= !bit;
            self.statuses[index] = entry.status;
            if entry.status.worked() {
                self.done |= bit;
            } else {
                self.errors = self.errors.wrapping_add(1);
                self.last_error = entry.status;
                if entry.status.fails() {
                    self.failed |= bit;
                } else {
                    self.retry |= bit;
                }
            }
        }
        if moved {
            let register = match self.shadow {
                Some((shadow, _, at)) => shadow.update(at, self.head),
                None => true,
            };
            if register {
                self.cq_doorbell.write32(0, u32::from(self.head));
            }
        }
    }

    /// Starts afresh before the controller is reset, which forgets the
    /// lane's queues: every request in flight goes again, and the queues
    /// are empty, to be made again on the controller ([`crate::Nvme::restart`]).
    pub fn restart(&mut self) {
        self.retry |= self.busy;
        self.busy = 0;
        self.tail = 0;
        self.head = 0;
        self.phase = true;
        self.unrung = false;
        self.memory.write(CQ, &[0u8; SLOTS * COMPLETION_SIZE]);
    }

    /// Gives up on the controller: every request in flight, and every one
    /// after, fails.
    pub fn kill(&mut self) {
        self.dead = true;
        self.failed |= self.busy | self.retry;
        self.retry = 0;
        self.busy = 0;
    }
}
