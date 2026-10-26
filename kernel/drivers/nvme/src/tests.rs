// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated NVMe controller: its registers, its
//! admin commands, and namespaces that carry out the commands in its I/O
//! queues, at once or when a test says, failing with the status a test
//! sets on blocks it marks.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Dma, Mmio, Registers, fake_memory, fake_memory_write};

use crate::command::{self, COMMAND_SIZE, PAGE, Status, admin, cns, nvm};
use crate::identify::{self, Unusable};
use crate::{DEPTH, Error, Lane, MEMORY_PAGES, Nvme, Op, Outcome};

/// Its registers' length: the controller's, and doorbells for 17 queues.
const REGS: usize = 0x1000 + 17 * 8;

/// A namespace of the simulated controller.
struct Space {
    id: u32,
    blocks: u64,
    /// The format's block size shift and metadata bytes.
    shift: u8,
    metadata: u16,
    read_only: bool,
    /// The blocks written; the others hold their number's pattern.
    written: BTreeMap<u64, Vec<u8>>,
}

/// What block `block` of a namespace holds before it is written.
fn pattern(block: u64) -> Vec<u8> {
    (block * 512..block * 512 + 512).map(|i| (i % 251) as u8).collect()
}

/// A queue of the simulated controller: where it is, its size, and how
/// far the controller has got.
#[derive(Clone, Copy)]
struct Queue {
    base: u64,
    entries: u16,
    /// For a submission queue, the next entry to read; for a completion
    /// queue, the next to write.
    at: u16,
    phase: bool,
    /// A submission queue's completion queue; a completion queue's MSI-X
    /// entry.
    other: u16,
}

#[derive(Default)]
struct State {
    cc: u32,
    csts: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    sqs: HashMap<u16, Queue>,
    cqs: HashMap<u16, Queue>,
    spaces: Vec<Space>,
    /// The version it says it implements.
    version: u32,
    /// MDTS and the queue pairs it grants.
    mdts: u8,
    grants: u16,
    write_cache: bool,
    /// Blocks whose reads and writes fail, with the status they fail with.
    bad: HashMap<u64, u16>,
    /// Whether I/O commands wait for [`Sim::finish`] instead of finishing
    /// as they are read.
    hold: bool,
    /// I/O commands read and not finished: their queue and bytes.
    held: Vec<(u16, [u8; COMMAND_SIZE])>,
    /// Whether it fails as soon as it is enabled.
    fatal: bool,
    /// Every command it took: queue, opcode, namespace, dwords 10 to 12.
    commands: Vec<(u16, u8, u32, [u32; 3])>,
    deallocated: Vec<(u32, u64, u32)>,
    flushes: u32,
    /// The MSI-X entries it has signalled.
    interrupts: Vec<u16>,
    /// The doorbell writes it has seen: queue and whether it was a tail.
    doorbells: Vec<(u16, bool)>,
    /// How many times it was enabled.
    enables: u32,
    shutdowns: u32,
}

/// The simulated controller.
#[derive(Default)]
struct Sim(Mutex<State>);

const CAP: u64 = 2047 | (1 << 16) | (15 << 24) | (1 << 37) | (4 << 52);

fn read_command(base: u64, index: u16) -> [u8; COMMAND_SIZE] {
    let mut bytes = [0u8; COMMAND_SIZE];
    fake_memory(base + u64::from(index) * COMMAND_SIZE as u64, &mut bytes);
    bytes
}

fn dword(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn qword(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// The pages a command's data are in, each (address, length), for
/// `bytes` bytes from PRP1 and PRP2.
fn data_pages(prp1: u64, prp2: u64, bytes: u64) -> Vec<(u64, u64)> {
    let first = (PAGE - prp1 % PAGE).min(bytes);
    let mut pages = std::vec![(prp1, first)];
    let mut left = bytes - first;
    if left == 0 {
        return pages;
    }
    if left <= PAGE {
        pages.push((prp2, left));
        return pages;
    }
    let mut entry = prp2;
    while left > 0 {
        let mut word = [0u8; 8];
        fake_memory(entry, &mut word);
        let take = left.min(PAGE);
        pages.push((u64::from_le_bytes(word), take));
        left -= take;
        entry += 8;
    }
    pages
}

impl State {
    /// Writes a completion of `id` with `status` and `result` to
    /// completion queue `cq`, and signals its interrupt.
    fn complete(&mut self, cq: u16, sq_head: u16, id: u16, status: u16, result: u32) {
        let queue = self.cqs.get_mut(&cq).unwrap();
        let mut entry = [0u8; 16];
        entry[0..4].copy_from_slice(&result.to_le_bytes());
        entry[8..10].copy_from_slice(&sq_head.to_le_bytes());
        let last = u32::from(id) | (u32::from(queue.phase) << 16) | (u32::from(status) << 17);
        entry[12..16].copy_from_slice(&last.to_le_bytes());
        fake_memory_write(queue.base + u64::from(queue.at) * 16, &entry);
        queue.at = (queue.at + 1) % queue.entries;
        if queue.at == 0 {
            queue.phase = !queue.phase;
        }
        if cq != 0 {
            let vector = queue.other;
            self.interrupts.push(vector);
        }
    }

    /// Carries out the commands written to submission queue `sq` up to
    /// `tail`.
    fn run(&mut self, sq: u16, tail: u16) {
        loop {
            let Some(queue) = self.sqs.get_mut(&sq) else { return };
            if queue.at == tail {
                return;
            }
            let bytes = read_command(queue.base, queue.at);
            queue.at = (queue.at + 1) % queue.entries;
            if sq == 0 {
                self.admin(&bytes);
            } else if self.hold {
                self.held.push((sq, bytes));
            } else {
                self.io(sq, &bytes);
            }
        }
    }

    fn admin(&mut self, bytes: &[u8; COMMAND_SIZE]) {
        let (opcode, id) = (bytes[0], u16::from_le_bytes([bytes[2], bytes[3]]));
        let (namespace, prp1) = (dword(bytes, 4), qword(bytes, 24));
        let (cdw10, cdw11) = (dword(bytes, 40), dword(bytes, 44));
        self.commands.push((0, opcode, namespace, [cdw10, cdw11, 0]));
        let head = self.sqs[&0].at;
        let mut result = 0;
        let status = match opcode {
            admin::IDENTIFY => {
                let mut data = std::vec![0u8; identify::SIZE];
                let status = match cdw10 as u8 {
                    cns::CONTROLLER => {
                        data[0..2].copy_from_slice(&0x1b36u16.to_le_bytes());
                        data[4..24].copy_from_slice(b"SIM0001             ");
                        data[24..64].fill(b' ');
                        data[24..37].copy_from_slice(b"Simulated SSD");
                        data[64..72].copy_from_slice(b"1.0     ");
                        data[77] = self.mdts;
                        data[80..84].copy_from_slice(&self.version.to_le_bytes());
                        data[88..92].copy_from_slice(&2_000_000u32.to_le_bytes());
                        data[516..520].copy_from_slice(&8u32.to_le_bytes());
                        data[520..522].copy_from_slice(&(1u16 << 2).to_le_bytes());
                        data[525] = u8::from(self.write_cache);
                        0
                    }
                    cns::ACTIVE_NAMESPACES => {
                        let mut ids: Vec<u32> = self.spaces.iter().map(|s| s.id).collect();
                        ids.sort_unstable();
                        for (i, id) in ids.into_iter().filter(|&id| id > namespace).enumerate() {
                            data[4 * i..4 * i + 4].copy_from_slice(&id.to_le_bytes());
                        }
                        0
                    }
                    cns::NAMESPACE => match self.spaces.iter().find(|s| s.id == namespace) {
                        Some(space) => {
                            data[0..8].copy_from_slice(&space.blocks.to_le_bytes());
                            data[8..16].copy_from_slice(&space.blocks.to_le_bytes());
                            // Writes of 4 KiB are best, and of 8 KiB whole
                            // after a power failure.
                            data[24] = (1 << 4) | (1 << 1);
                            data[44..46].copy_from_slice(&15u16.to_le_bytes());
                            data[25] = 1;
                            data[26] = 1;
                            // Format 0 is 4 KiB blocks; format 1 the one in use.
                            data[128..132].copy_from_slice(&(12u32 << 16).to_le_bytes());
                            let format = u32::from(space.metadata) | (u32::from(space.shift) << 16);
                            data[132..136].copy_from_slice(&format.to_le_bytes());
                            data[64..66].copy_from_slice(&7u16.to_le_bytes());
                            data[99] = u8::from(space.read_only);
                            0
                        }
                        None => 0x0b | (1 << 14),
                    },
                    cns::NVM_CONTROLLER => {
                        data[4..8].copy_from_slice(&(1u32 << 20).to_le_bytes());
                        0
                    }
                    _ => 0x02 | (1 << 14),
                };
                fake_memory_write(prp1, &data);
                status
            }
            admin::SET_FEATURES if cdw10 == 7 => {
                let wanted = (cdw11 & 0xffff) as u16 + 1;
                let granted = wanted.min(self.grants);
                result = u32::from(granted - 1) | (u32::from(granted - 1) << 16);
                0
            }
            admin::CREATE_CQ => {
                let id = cdw10 as u16;
                let entries = (cdw10 >> 16) as u16 + 1;
                let vector = (cdw11 >> 16) as u16;
                if id == 0 || id > self.grants || self.cqs.contains_key(&id) {
                    0x101 | (1 << 14)
                } else {
                    let queue = Queue { base: prp1, entries, at: 0, phase: true, other: vector };
                    self.cqs.insert(id, queue);
                    0
                }
            }
            admin::CREATE_SQ => {
                let id = cdw10 as u16;
                let entries = (cdw10 >> 16) as u16 + 1;
                let cq = (cdw11 >> 16) as u16;
                if !self.cqs.contains_key(&cq) || self.sqs.contains_key(&id) {
                    0x100 | (1 << 14)
                } else {
                    let queue = Queue { base: prp1, entries, at: 0, phase: true, other: cq };
                    self.sqs.insert(id, queue);
                    0
                }
            }
            admin::DELETE_SQ => {
                self.sqs.remove(&(cdw10 as u16));
                0
            }
            admin::DELETE_CQ => {
                self.cqs.remove(&(cdw10 as u16));
                0
            }
            _ => 0x01 | (1 << 14),
        };
        self.complete(0, head, id, status, result);
    }

    fn io(&mut self, sq: u16, bytes: &[u8; COMMAND_SIZE]) {
        let (opcode, id) = (bytes[0], u16::from_le_bytes([bytes[2], bytes[3]]));
        let namespace = dword(bytes, 4);
        let (prp1, prp2) = (qword(bytes, 24), qword(bytes, 32));
        let (cdw10, cdw11, cdw12) = (dword(bytes, 40), dword(bytes, 44), dword(bytes, 48));
        self.commands.push((sq, opcode, namespace, [cdw10, cdw11, cdw12]));
        let queue = self.sqs[&sq];
        let block = u64::from(cdw10) | (u64::from(cdw11) << 32);
        let blocks = u64::from(cdw12 & 0xffff) + 1;
        let bad = (block..block + blocks).find_map(|b| self.bad.get(&b).copied());
        let space = self.spaces.iter_mut().find(|s| s.id == namespace);
        let status = match (opcode, space) {
            (_, None) => 0x0b | (1 << 14),
            (nvm::READ | nvm::WRITE, Some(_)) if bad.is_some() => bad.unwrap_or(0),
            (nvm::READ | nvm::WRITE, Some(space)) if block + blocks > space.blocks => {
                0x80 | (1 << 14)
            }
            (nvm::READ, Some(space)) => {
                let mut bytes = Vec::new();
                for b in block..block + blocks {
                    bytes.extend(space.written.get(&b).cloned().unwrap_or_else(|| pattern(b)));
                }
                let mut at = 0;
                for (phys, len) in data_pages(prp1, prp2, bytes.len() as u64) {
                    fake_memory_write(phys, &bytes[at..at + len as usize]);
                    at += len as usize;
                }
                0
            }
            (nvm::WRITE, Some(space)) if space.read_only => 0x20 | (1 << 14),
            (nvm::WRITE, Some(space)) => {
                let mut bytes = std::vec![0u8; blocks as usize * 512];
                let mut at = 0;
                for (phys, len) in data_pages(prp1, prp2, bytes.len() as u64) {
                    fake_memory(phys, &mut bytes[at..at + len as usize]);
                    at += len as usize;
                }
                for (i, chunk) in bytes.chunks(512).enumerate() {
                    space.written.insert(block + i as u64, chunk.to_vec());
                }
                0
            }
            (nvm::FLUSH, Some(_)) => {
                self.flushes += 1;
                0
            }
            (nvm::DATASET_MANAGEMENT, Some(_)) => {
                let mut range = [0u8; 16];
                fake_memory(prp1, &mut range);
                self.deallocated.push((namespace, qword(&range, 8), dword(&range, 4)));
                0
            }
            _ => 0x01 | (1 << 14),
        };
        self.complete(queue.other, queue.at, id, status, 0);
    }
}

impl Sim {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    /// Finishes the held commands, in the order they came.
    fn finish(&self) {
        let mut state = self.state();
        let held = core::mem::take(&mut state.held);
        for (sq, bytes) in held {
            state.io(sq, &bytes);
        }
    }

    /// Answers held command `index` with `status`, out of turn.
    fn answer(&self, index: usize, status: u16) {
        let mut state = self.state();
        let (sq, bytes) = state.held.remove(index);
        let queue = state.sqs[&sq];
        let id = u16::from_le_bytes([bytes[2], bytes[3]]);
        state.complete(queue.other, queue.at, id, status, 0);
    }
}

impl Registers for Sim {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let state = self.state();
        let value = match offset {
            0x00 => CAP,
            0x04 => CAP >> 32,
            0x08 => u64::from(state.version),
            0x14 => u64::from(state.cc),
            0x1c => u64::from(state.csts),
            0x24 => u64::from(state.aqa),
            _ => 0,
        };
        if size == 4 { value & 0xffff_ffff } else { value }
    }

    fn write(&self, offset: usize, _size: usize, value: u64) {
        let mut state = self.state();
        let value32 = value as u32;
        match offset {
            0x14 => {
                let was = state.cc;
                state.cc = value32;
                if was & 1 == 0 && value32 & 1 != 0 {
                    state.enables += 1;
                    if state.fatal {
                        state.csts |= 2;
                        return;
                    }
                    let size = (state.aqa & 0xfff) as u16 + 1;
                    let (asq, acq) = (state.asq, state.acq);
                    state.sqs.insert(
                        0,
                        Queue { base: asq, entries: size, at: 0, phase: true, other: 0 },
                    );
                    state.cqs.insert(
                        0,
                        Queue { base: acq, entries: size, at: 0, phase: true, other: 0 },
                    );
                    state.csts |= 1;
                } else if was & 1 != 0 && value32 & 1 == 0 {
                    state.sqs.clear();
                    state.cqs.clear();
                    state.held.clear();
                    state.csts &= !1;
                }
                if value32 & (3 << 14) == 1 << 14 {
                    state.shutdowns += 1;
                    state.csts = (state.csts & !(3 << 2)) | (2 << 2);
                }
            }
            0x24 => state.aqa = value32,
            0x28 => state.asq = (state.asq & !0xffff_ffff) | u64::from(value32),
            0x2c => state.asq = (state.asq & 0xffff_ffff) | (u64::from(value32) << 32),
            0x30 => state.acq = (state.acq & !0xffff_ffff) | u64::from(value32),
            0x34 => state.acq = (state.acq & 0xffff_ffff) | (u64::from(value32) << 32),
            doorbell if doorbell >= 0x1000 => {
                let index = (doorbell - 0x1000) / 4;
                let queue = (index / 2) as u16;
                let tail = index % 2 == 0;
                state.doorbells.push((queue, tail));
                if tail {
                    state.run(queue, value32 as u16);
                }
            }
            _ => {}
        }
    }
}

fn space(id: u32, blocks: u64) -> Space {
    Space { id, blocks, shift: 9, metadata: 0, read_only: false, written: BTreeMap::new() }
}

/// A simulated controller with namespaces 1 (a mebibyte) and 2 (64 KiB)
/// and the driver set up on it.
fn setup() -> (Arc<Sim>, Nvme) {
    let sim = Arc::new(Sim::default());
    {
        let mut state = sim.state();
        state.version = 0x0001_0400;
        state.mdts = 5;
        state.grants = 16;
        state.write_cache = true;
        state.spaces.push(space(1, 2048));
        state.spaces.push(space(2, 128));
    }
    let regs = Mmio::fake(sim.clone(), REGS);
    let nvme = Nvme::new(regs, &mut alloc, &mut |_| {}).unwrap();
    (sim, nvme)
}

fn alloc(pages: usize) -> Option<Dma> {
    Some(Dma::fake(pages * 4096))
}

/// A lane of queue pair `id`, with MSI-X entry `id`.
fn lane(nvme: &mut Nvme, id: u16) -> Lane {
    nvme.lane(id, Some(id), &mut alloc, &mut |_| {}).unwrap()
}

/// Page-aligned host memory of `pages` pages: its first address. It is
/// never given back.
fn pages(pages: usize) -> u64 {
    let memory: &Dma = Box::leak(Box::new(Dma::fake((pages + 1) * 4096)));
    (memory.phys() + PAGE - 1) & !(PAGE - 1)
}

fn read_host(phys: u64, len: usize) -> Vec<u8> {
    let mut out = std::vec![0u8; len];
    fake_memory(phys, &mut out);
    out
}

/// Waits for the lane's next completion.
fn next(lane: &mut Lane) -> (u8, Outcome) {
    lane.complete().expect("a completion")
}

#[test]
fn takes_the_controller_and_says_what_it_is() {
    let (sim, nvme) = setup();
    let identity = nvme.identity();
    assert_eq!(identify::trimmed(&identity.model), "Simulated SSD");
    assert_eq!(identify::trimmed(&identity.serial), "SIM0001");
    assert_eq!(identity.max_pages, 32);
    assert!(identity.write_cache && identity.deallocate);
    assert_eq!(nvme.version(), (1, 4, 0));
    assert_eq!(nvme.deallocate_blocks(), 1 << 20);
    let state = sim.state();
    assert_eq!(state.enables, 1);
    // 64-byte and 16-byte entries, enabled.
    assert_eq!(state.cc, (6 << 16) | (4 << 20) | 1);
}

#[test]
fn refuses_a_controller_without_4k_pages_or_the_nvm_set() {
    struct Odd;
    impl Registers for Odd {
        fn read(&self, offset: usize, _size: usize) -> u64 {
            match offset {
                // MPSMIN 1: 8 KiB pages at least.
                4 => (1 << 5) | (1 << 16),
                _ => 0,
            }
        }
        fn write(&self, _offset: usize, _size: usize, _value: u64) {}
    }
    let regs = Mmio::fake(Arc::new(Odd), REGS);
    assert_eq!(Nvme::new(regs, &mut alloc, &mut |_| {}).err(), Some(Error::Unsupported));
}

#[test]
fn a_controller_that_fails_at_once_is_refused() {
    let sim = Arc::new(Sim::default());
    sim.state().fatal = true;
    let regs = Mmio::fake(sim.clone(), REGS);
    assert_eq!(Nvme::new(regs, &mut alloc, &mut |_| {}).err(), Some(Error::Failed));
}

#[test]
fn grants_at_most_what_was_asked_and_the_controller_has() {
    let (sim, mut nvme) = setup();
    assert_eq!(nvme.request_queues(4, &mut |_| {}).unwrap(), 4);
    sim.state().grants = 2;
    assert_eq!(nvme.request_queues(8, &mut |_| {}).unwrap(), 2);
    assert_eq!(nvme.lane(3, None, &mut alloc, &mut |_| {}).err(), Some(Error::BadRequest));
}

#[test]
fn lists_the_namespaces_it_takes_and_why_not_the_others() {
    let (sim, mut nvme) = setup();
    {
        let mut state = sim.state();
        state.spaces.push(Space { shift: 12, ..space(3, 64) });
        state.spaces.push(Space { metadata: 8, ..space(4, 64) });
        state.spaces.push(Space { read_only: true, ..space(5, 64) });
    }
    let mut found = Vec::new();
    nvme.namespaces(&mut |_| {}, &mut |id, answer| found.push((id, answer))).unwrap();
    let ids: Vec<u32> = found.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, [1, 2, 3, 4, 5]);
    let one = found[0].1.unwrap();
    assert_eq!((one.info.blocks, one.info.block_size, one.info.read_only), (2048, 4096, false));
    assert_eq!(found[2].1.err(), Some(Unusable::BlockSize(4096)));
    assert_eq!(found[3].1.err(), Some(Unusable::Metadata));
    assert!(found[4].1.unwrap().info.read_only);
}

#[test]
fn reads_and_writes_through_prp_lists() {
    let (sim, mut nvme) = setup();
    nvme.request_queues(2, &mut |_| {}).unwrap();
    let mut lane = lane(&mut nvme, 1);
    // One page from an offset, two pages, and eight through a PRP list.
    for (slot, count, offset) in [(0u8, 1usize, 512u64), (1, 2, 0), (2, 8, 1024)] {
        let base = pages(count);
        let mut data = Vec::new();
        for i in 0..count as u64 {
            let start = if i == 0 { offset } else { 0 };
            data.push((base + i * PAGE + start, (PAGE - start) as u32));
        }
        let block = 100 * u64::from(slot);
        let bytes: u64 = data.iter().map(|&(_, len)| u64::from(len)).sum();
        let text: Vec<u8> = (0..bytes).map(|i| (i * 7 + u64::from(slot)) as u8).collect();
        let mut at = 0;
        for &(phys, len) in &data {
            fake_memory_write(phys, &text[at..at + len as usize]);
            at += len as usize;
        }
        lane.submit(slot, Op::Write, 1, block, &data, true).unwrap();
        assert_eq!(next(&mut lane), (slot, Outcome::Done));
        for &(phys, len) in &data {
            fake_memory_write(phys, &std::vec![0u8; len as usize]);
        }
        lane.submit(slot, Op::Read, 1, block, &data, true).unwrap();
        assert_eq!(next(&mut lane), (slot, Outcome::Done));
        let mut back = Vec::new();
        for &(phys, len) in &data {
            back.extend(read_host(phys, len as usize));
        }
        assert_eq!(back, text);
        let state = sim.state();
        let written = &state.spaces[0].written;
        assert_eq!(written[&block], text[..512]);
    }
    let state = sim.state();
    assert!(state.interrupts.iter().all(|&vector| vector == 1));
}

#[test]
fn writes_through_with_fua_and_flushes_and_deallocates() {
    let (sim, mut nvme) = setup();
    nvme.request_queues(1, &mut |_| {}).unwrap();
    let mut lane = lane(&mut nvme, 1);
    let page = pages(1);
    lane.submit(0, Op::WriteThrough, 2, 5, &[(page, 4096)], true).unwrap();
    lane.submit(1, Op::Flush, 2, 0, &[], true).unwrap();
    lane.submit(2, Op::Deallocate(64), 2, 10, &[], true).unwrap();
    let mut outcomes: Vec<_> = (0..3).map(|_| next(&mut lane)).collect();
    outcomes.sort_by_key(|outcome| outcome.0);
    assert_eq!(outcomes, [(0, Outcome::Done), (1, Outcome::Done), (2, Outcome::Done)]);
    let state = sim.state();
    let write = state.commands.iter().find(|c| c.0 != 0 && c.1 == nvm::WRITE).unwrap();
    assert_eq!(write.3[2], 7 | (1 << 30), "8 blocks, with FUA");
    assert_eq!(state.flushes, 1);
    assert_eq!(state.deallocated, [(2, 10, 64)]);
}

#[test]
fn refuses_buffers_a_prp_list_cannot_describe() {
    let (_sim, mut nvme) = setup();
    nvme.request_queues(1, &mut |_| {}).unwrap();
    let mut lane = lane(&mut nvme, 1);
    let base = pages(3);
    let bad = [
        // The first buffer ends short of its page, with another after it.
        std::vec![(base, 512), (base + PAGE, 4096)],
        // The second starts inside its page.
        std::vec![(base, 4096), (base + PAGE + 512, 512)],
        // Not whole blocks.
        std::vec![(base, 100)],
        // More pages than the controller takes in one command.
        (0..33).map(|i| (base + i * PAGE, 4096)).collect(),
    ];
    for data in bad {
        assert_eq!(lane.submit(0, Op::Read, 1, 0, &data, true), Err(Error::BadRequest));
    }
    assert_eq!(lane.submit(0, Op::Flush, 1, 0, &[(base, 512)], true), Err(Error::BadRequest));
    assert!(lane.room());
}

#[test]
fn keeps_at_most_63_in_flight_and_rings_once_for_a_batch() {
    let (sim, mut nvme) = setup();
    nvme.request_queues(1, &mut |_| {}).unwrap();
    let mut lane = lane(&mut nvme, 1);
    sim.state().hold = true;
    let page = pages(1);
    let before = sim.state().doorbells.len();
    for slot in 0..DEPTH as u8 {
        let last = u32::from(slot) + 1 == DEPTH;
        lane.submit(slot, Op::Read, 1, u64::from(slot), &[(page, 512)], last).unwrap();
    }
    assert!(!lane.room());
    assert_eq!(lane.submit(63, Op::Read, 1, 0, &[(page, 512)], true), Err(Error::Full));
    assert_eq!(lane.submit(5, Op::Read, 1, 0, &[(page, 512)], true), Err(Error::BadRequest));
    // One doorbell for the 63.
    assert_eq!(sim.state().doorbells.len(), before + 1);
    assert_eq!(sim.state().held.len(), DEPTH as usize);
    sim.finish();
    let mut slots: Vec<u8> = std::iter::from_fn(|| lane.complete()).map(|(slot, _)| slot).collect();
    slots.sort_unstable();
    assert_eq!(slots, (0..DEPTH as u8).collect::<Vec<_>>());
    assert!(lane.room());
}

#[test]
fn wraps_both_queues_many_times() {
    let (sim, mut nvme) = setup();
    nvme.request_queues(1, &mut |_| {}).unwrap();
    let mut lane = lane(&mut nvme, 1);
    let page = pages(1);
    for i in 0..300u64 {
        let slot = (i % 7) as u8;
        lane.submit(slot, Op::Read, 1, i % 2048, &[(page, 512)], true).unwrap();
        assert_eq!(next(&mut lane), (slot, Outcome::Done));
        assert_eq!(read_host(page, 512), pattern(i % 2048));
    }
    assert!(lane.complete().is_none());
    let reads = sim.state().commands.iter().filter(|c| c.0 != 0 && c.1 == nvm::READ).count();
    assert_eq!(reads, 300);
}

#[test]
fn do_not_retry_fails_and_others_go_again() {
    let (sim, mut nvme) = setup();
    nvme.request_queues(1, &mut |_| {}).unwrap();
    let mut lane = lane(&mut nvme, 1);
    {
        let mut state = sim.state();
        // An unrecovered read error with DNR and one without, an invalid
        // field with DNR, and an internal error without.
        state.bad.insert(1, 0x281 | (1 << 14));
        state.bad.insert(2, 0x281);
        state.bad.insert(3, 0x002 | (1 << 14));
        state.bad.insert(4, 0x006);
    }
    let page = pages(1);
    for (block, outcome) in
        [(1, Outcome::Failed), (2, Outcome::Retry), (3, Outcome::Failed), (4, Outcome::Retry)]
    {
        lane.submit(0, Op::Read, 1, block, &[(page, 512)], true).unwrap();
        assert_eq!(next(&mut lane), (0, outcome));
    }
    let (errors, last) = lane.errors();
    assert_eq!(errors, 4);
    assert_eq!(last, Status(0x006));
    assert_eq!(lane.status(0), last);
    assert_eq!(last.describe(), "internal error");
    assert_eq!(Status(0x281).describe(), "unrecovered read error");
}

#[test]
fn completions_come_out_of_order_and_strays_are_ignored() {
    let (sim, mut nvme) = setup();
    nvme.request_queues(1, &mut |_| {}).unwrap();
    let mut lane = lane(&mut nvme, 1);
    sim.state().hold = true;
    let page = pages(1);
    for slot in 0..3u8 {
        lane.submit(slot, Op::Read, 1, 0, &[(page, 512)], true).unwrap();
    }
    sim.answer(2, 0);
    assert_eq!(next(&mut lane), (2, Outcome::Done));
    // A completion for a slot not in flight.
    {
        let mut state = sim.state();
        let at = state.sqs[&1].at;
        state.complete(1, at, 40, 0, 0);
    }
    assert!(lane.complete().is_none());
    sim.finish();
    assert_eq!(next(&mut lane), (0, Outcome::Done));
    assert_eq!(next(&mut lane), (1, Outcome::Done));
}

#[test]
fn a_reset_makes_the_queues_again_and_sends_what_was_in_flight_again() {
    let (sim, mut nvme) = setup();
    nvme.request_queues(2, &mut |_| {}).unwrap();
    let mut lanes = [lane(&mut nvme, 1), lane(&mut nvme, 2)];
    sim.state().hold = true;
    let page = pages(1);
    lanes[0].submit(4, Op::Read, 1, 9, &[(page, 512)], true).unwrap();
    lanes[1].submit(7, Op::Write, 1, 9, &[(page, 512)], true).unwrap();
    // The controller never answers: the kernel resets it.
    for lane in &mut lanes {
        lane.restart();
    }
    nvme.restart(&mut |_| {}).unwrap();
    for lane in &lanes {
        nvme.remake(lane, &mut |_| {}).unwrap();
    }
    assert_eq!(sim.state().enables, 2);
    assert_eq!(next(&mut lanes[0]), (4, Outcome::Retry));
    assert_eq!(next(&mut lanes[1]), (7, Outcome::Retry));
    sim.state().hold = false;
    lanes[0].submit(4, Op::Read, 1, 9, &[(page, 512)], true).unwrap();
    assert_eq!(next(&mut lanes[0]), (4, Outcome::Done));
    lanes[1].submit(7, Op::Read, 2, 1, &[(page, 512)], true).unwrap();
    assert_eq!(next(&mut lanes[1]), (7, Outcome::Done));
    assert_eq!(read_host(page, 512), pattern(1));
}

#[test]
fn a_controller_that_does_not_come_back_kills_its_lanes() {
    let (sim, mut nvme) = setup();
    nvme.request_queues(1, &mut |_| {}).unwrap();
    let mut lane = lane(&mut nvme, 1);
    sim.state().hold = true;
    let page = pages(1);
    lane.submit(1, Op::Read, 1, 0, &[(page, 512)], true).unwrap();
    sim.state().fatal = true;
    lane.restart();
    assert_eq!(nvme.restart(&mut |_| {}), Err(Error::Failed));
    assert!(nvme.failed());
    lane.kill();
    assert_eq!(next(&mut lane), (1, Outcome::Failed));
    assert!(lane.dead());
    assert_eq!(lane.submit(1, Op::Read, 1, 0, &[(page, 512)], true), Err(Error::Dead));
}

#[test]
fn shuts_down() {
    let (sim, mut nvme) = setup();
    assert!(nvme.shutdown(&mut |_| {}));
    let state = sim.state();
    assert_eq!(state.shutdowns, 1);
    assert_eq!(state.cc & (3 << 14), 1 << 14);
}

#[test]
fn a_lane_takes_its_memory_in_pages() {
    assert_eq!(MEMORY_PAGES, 18);
    assert_eq!(command::LIST_ENTRIES * 8 * crate::SLOTS, 65_536);
}

#[test]
fn commands_are_laid_out_as_the_specification_has_them() {
    let mut command = command::Command::transfer(true, true, 3, 0x1_0000_0002, 8);
    command.id = 0x1234;
    command.prp1 = 0x1000;
    command.prp2 = 0x2000;
    let bytes = command.bytes();
    assert_eq!(dword(&bytes, 0), 0x1234_0001);
    assert_eq!(dword(&bytes, 4), 3);
    assert_eq!(qword(&bytes, 24), 0x1000);
    assert_eq!(qword(&bytes, 32), 0x2000);
    assert_eq!((dword(&bytes, 40), dword(&bytes, 44)), (2, 1));
    assert_eq!(dword(&bytes, 48), 7 | (1 << 30));
    let queue = command::Command::create_cq(2, 64, 0x3000, Some(5));
    assert_eq!(dword(&queue.bytes(), 40), 2 | (63 << 16));
    assert_eq!(dword(&queue.bytes(), 44), 3 | (5 << 16));
}

#[test]
fn a_block_is_what_is_best_written_and_whole_after_a_power_failure() {
    let mut data = [0u8; identify::HEAD];
    data[0] = 64;
    data[130] = 9;
    // 4 KiB is best written, but only single blocks are whole.
    data[24] = 1 << 4;
    data[64] = 7;
    assert_eq!(identify::namespace(&data, 1).map(|n| n.block_size), Ok(512));
    // The controller's writes of 16 blocks are whole.
    assert_eq!(identify::namespace(&data, 16).map(|n| n.block_size), Ok(4096));
    // The namespace's own say of 2 blocks wins over the controller's.
    data[24] |= 1 << 1;
    data[44] = 1;
    assert_eq!(identify::namespace(&data, 16).map(|n| n.block_size), Ok(1024));
    // A namespace that does not say what is best is written in blocks.
    data[24] = 0;
    assert_eq!(identify::namespace(&data, 16).map(|n| n.block_size), Ok(512));
}
