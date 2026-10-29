// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! USB Attached SCSI against a simulated UAS disk, with streams as at
//! SuperSpeed and without, as below it: commands answered in any order,
//! failures with their sense, and the disk breaking the protocol, going
//! silent or being unplugged.

use std::collections::{HashMap, VecDeque};
use std::vec::Vec;

use proptest::prelude::*;
use radix_scsi::{Capacity, key, op, probe};

use super::*;
use crate::Configuration;

/// A transfer that ended: its pipe, its number, how, and a status's
/// bytes.
type Ended = (Pipe, u16, Result<usize, PipeError>, Vec<u8>);

/// A transfer the host started that the disk has not ended.
#[derive(Clone, Debug)]
struct Started {
    pipe: Pipe,
    n: u16,
    /// What goes out: a Command IU, or data.
    out: Vec<u8>,
    /// How much may come in.
    length: usize,
}

/// Where the disk is in a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Taken: its data moves next, or its Ready IU goes first.
    Taken,
    /// Its Ready IU went.
    Ready,
    /// Its data moved: its status goes next.
    Moved,
}

/// A command the disk took.
#[derive(Clone, Debug)]
struct Job {
    tag: u16,
    stage: Stage,
    /// What it sends, or the block it writes to, or why it fails.
    answer: Result<(Vec<u8>, Option<usize>), Sense>,
    /// How much it moves which way.
    data: Option<(bool, usize)>,
    /// What moved.
    moved: usize,
}

/// What the disk does wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    /// Answers the command with this tag on the next tag's stream.
    WrongStream(u16),
    /// Stalls the data of the command with this tag.
    StallData(u16),
    /// Never answers the command with this tag.
    Silent(u16),
    /// Answers the command with this tag with a Response IU of this code.
    Respond(u16, u8),
    /// Sends a Read Ready IU for the command with this tag before its
    /// status, with streams.
    ReadyWithStreams(u16),
    /// Sends a Sense IU with this tag first, for no command.
    Stray(u16),
    /// Answers the command with this tag with this SCSI status.
    Status(u16, u8),
}

/// Which command the disk answers first, of those it can.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    First,
    Last,
}

struct Disk {
    streams: bool,
    blocks: Vec<[u8; 512]>,
    luns: u8,
    order: Order,
    faults: Vec<Fault>,
    started: Vec<Started>,
    jobs: Vec<Job>,
    /// The transfers that ended, for the host to hear of.
    ended: VecDeque<Ended>,
    /// What came in for each data transfer, by its number.
    came: HashMap<u16, Vec<u8>>,
    /// The Command IUs it took.
    ius: Vec<[u8; COMMAND_LENGTH]>,
    /// The most commands it held at once.
    most: usize,
    resets: u32,
    gone: bool,
    /// The milliseconds the host has waited.
    now: u64,
}

impl Disk {
    fn new(streams: bool, blocks: usize) -> Disk {
        let mut blocks = std::vec![[0u8; 512]; blocks];
        for (i, block) in blocks.iter_mut().enumerate() {
            block.fill(i as u8);
        }
        Disk {
            streams,
            blocks,
            luns: 1,
            order: Order::First,
            faults: Vec::new(),
            started: Vec::new(),
            jobs: Vec::new(),
            ended: VecDeque::new(),
            came: HashMap::new(),
            ius: Vec::new(),
            most: 0,
            resets: 0,
            gone: false,
            now: 0,
        }
    }

    fn fault(&self, find: impl Fn(&Fault) -> bool) -> Option<Fault> {
        self.faults.iter().copied().find(find)
    }

    /// What `cdb` to logical unit `lun` asks for.
    fn execute(&self, lun: u8, cdb: &[u8]) -> Result<(Vec<u8>, Option<usize>), Sense> {
        let fail = |asc| Sense { key: key::ILLEGAL_REQUEST, asc, ascq: 0 };
        if lun >= self.luns {
            return Err(fail(0x25));
        }
        let lba = cdb.get(2..6).map_or(0, |b| u32::from_be_bytes(b.try_into().unwrap()) as usize);
        let count = cdb.get(7..9).map_or(0, |b| usize::from(u16::from_be_bytes([b[0], b[1]])));
        let fits = lba + count <= self.blocks.len();
        match cdb[0] {
            op::INQUIRY => {
                let mut bytes = std::vec![0u8; 36];
                bytes[2] = 6;
                bytes[8..16].copy_from_slice(b"QEMU    ");
                bytes[16..32].copy_from_slice(b"QEMU HARDDISK   ");
                bytes[32..36].copy_from_slice(b"2.5+");
                Ok((bytes, None))
            }
            op::TEST_UNIT_READY | op::SYNCHRONIZE_CACHE_10 => Ok((Vec::new(), None)),
            op::READ_CAPACITY_10 => {
                let mut bytes = ((self.blocks.len() - 1) as u32).to_be_bytes().to_vec();
                bytes.extend(512u32.to_be_bytes());
                Ok((bytes, None))
            }
            op::MODE_SENSE_6 => Ok((std::vec![3, 0, 0x00, 0], None)),
            op::READ_10 if fits => {
                Ok((self.blocks[lba..lba + count].iter().flatten().copied().collect(), None))
            }
            op::WRITE_10 if fits => Ok((Vec::new(), Some(lba))),
            op::READ_10 | op::WRITE_10 => Err(fail(0x21)),
            _ => Err(fail(0x20)),
        }
    }

    fn start(&mut self, pipe: Pipe, n: u16, out: Vec<u8>, length: usize) {
        assert!(
            !self.started.iter().any(|s| s.pipe == pipe && s.n == n),
            "{pipe:?} {n} started twice"
        );
        if self.gone {
            self.ended.push_back((pipe, n, Err(PipeError::Gone), Vec::new()));
            return;
        }
        self.started.push(Started { pipe, n, out, length });
    }

    fn cancel(&mut self, pipe: Pipe, n: u16) {
        if let Some(at) = self.started.iter().position(|s| s.pipe == pipe && s.n == n) {
            self.started.remove(at);
            self.ended.push_back((pipe, n, Err(PipeError::TimedOut), Vec::new()));
        }
    }

    /// The transfer the disk takes next on `pipe`: on the stream `n` with
    /// streams, the first started without.
    fn take(&mut self, pipe: Pipe, n: u16) -> Option<Started> {
        let at = self
            .started
            .iter()
            .position(|s| s.pipe == pipe && (!self.streams || pipe == Pipe::Command || s.n == n))?;
        Some(self.started.remove(at))
    }

    /// Sends `iu` on the status pipe, on stream `on` with streams.
    fn status_iu(&mut self, on: u16, iu: &[u8]) -> bool {
        let Some(status) = self.take(Pipe::Status, on) else { return false };
        let n = iu.len().min(status.length);
        self.ended.push_back((Pipe::Status, status.n, Ok(n), iu[..n].to_vec()));
        true
    }

    fn sense_iu(tag: u16, status: u8, sense: Option<Sense>) -> Vec<u8> {
        let mut iu = std::vec![0u8; 16];
        iu[0] = 0x03;
        iu[2..4].copy_from_slice(&tag.to_be_bytes());
        iu[6] = status;
        if let Some(sense) = sense {
            let mut bytes = std::vec![0u8; 18];
            bytes[0] = 0x70;
            bytes[2] = sense.key;
            bytes[7] = 10;
            bytes[12] = sense.asc;
            bytes[13] = sense.ascq;
            iu[14..16].copy_from_slice(&(bytes.len() as u16).to_be_bytes());
            iu.extend(bytes);
        }
        iu
    }

    /// Does the next thing it can: takes a command, or moves a command's
    /// data or status. False if it can do nothing.
    fn step(&mut self) -> bool {
        if self.gone {
            return false;
        }
        if let Some(command) = self.take(Pipe::Command, 0) {
            let iu: [u8; COMMAND_LENGTH] = command.out.clone().try_into().unwrap();
            self.ius.push(iu);
            let tag = u16::from_be_bytes([iu[2], iu[3]]);
            let cdb_len = match iu[16] {
                op::READ_CAPACITY_10 | op::READ_10 | op::WRITE_10 | op::SYNCHRONIZE_CACHE_10 => 10,
                _ => 6,
            };
            let cdb = iu[16..16 + cdb_len].to_vec();
            let lun = iu[9];
            let answer = self.execute(lun, &cdb);
            let data = match cdb[0] {
                op::INQUIRY | op::MODE_SENSE_6 => Some((true, usize::from(cdb[4]))),
                op::READ_CAPACITY_10 => Some((true, 8)),
                op::READ_10 | op::WRITE_10 => {
                    let count = usize::from(u16::from_be_bytes([cdb[7], cdb[8]]));
                    Some((cdb[0] == op::READ_10, count * 512))
                }
                _ => None,
            };
            assert!(!self.jobs.iter().any(|j| j.tag == tag), "tag {tag} taken twice");
            self.jobs.push(Job { tag, stage: Stage::Taken, answer, data, moved: 0 });
            self.most = self.most.max(self.jobs.len());
            self.ended.push_back((Pipe::Command, command.n, Ok(COMMAND_LENGTH), Vec::new()));
            return true;
        }
        if let Some(Fault::Stray(tag)) = self.fault(|f| matches!(f, Fault::Stray(_))) {
            self.faults.retain(|f| *f != Fault::Stray(tag));
            return self.status_iu(tag, &Disk::sense_iu(tag, 0, None));
        }
        let order: Vec<usize> = match self.order {
            Order::First => (0..self.jobs.len()).collect(),
            Order::Last => (0..self.jobs.len()).rev().collect(),
        };
        order.into_iter().any(|at| self.advance(at))
    }

    /// Moves the job at `at` on, if it can.
    fn advance(&mut self, at: usize) -> bool {
        let job = self.jobs[at].clone();
        let tag = job.tag;
        if self.fault(|f| *f == Fault::Silent(tag)).is_some() {
            return false;
        }
        if let Some(Fault::Respond(_, code)) =
            self.fault(|f| matches!(f, Fault::Respond(t, _) if *t == tag))
        {
            let mut iu = std::vec![0x04, 0, 0, 0, 0, 0, 0, code];
            iu[2..4].copy_from_slice(&tag.to_be_bytes());
            if self.status_iu(tag, &iu) {
                self.jobs.remove(at);
                return true;
            }
            return false;
        }
        let failed = job.answer.is_err();
        match (job.stage, job.data) {
            // With streams, a failed command's status comes at once: its
            // data never moves.
            (Stage::Taken, Some((into_host, _))) if !failed => {
                if !self.streams {
                    let id = if into_host { 0x06 } else { 0x07 };
                    let mut iu = std::vec![id, 0, 0, 0];
                    iu[2..4].copy_from_slice(&tag.to_be_bytes());
                    if !self.status_iu(tag, &iu) {
                        return false;
                    }
                    self.jobs[at].stage = Stage::Ready;
                    return true;
                }
                if self.fault(|f| *f == Fault::ReadyWithStreams(tag)).is_some() {
                    self.faults.retain(|f| *f != Fault::ReadyWithStreams(tag));
                    let mut iu = std::vec![0x06, 0, 0, 0];
                    iu[2..4].copy_from_slice(&tag.to_be_bytes());
                    return self.status_iu(tag, &iu);
                }
                self.move_data(at)
            }
            (Stage::Ready, Some(_)) => self.move_data(at),
            _ => {
                let on = match self.fault(|f| *f == Fault::WrongStream(tag)) {
                    Some(_) => tag + 1,
                    None => tag,
                };
                let (status, sense) = match (
                    &job.answer,
                    self.fault(|f| matches!(f, Fault::Status(t, _) if *t == tag)),
                ) {
                    (_, Some(Fault::Status(_, status))) => (status, None),
                    (Ok(_), _) => (0, None),
                    (Err(sense), _) => (2, Some(*sense)),
                };
                if !self.status_iu(on, &Disk::sense_iu(tag, status, sense)) {
                    return false;
                }
                self.jobs.remove(at);
                true
            }
        }
    }

    /// Moves the data of the job at `at`.
    fn move_data(&mut self, at: usize) -> bool {
        let job = self.jobs[at].clone();
        let Some((into_host, length)) = job.data else { return false };
        let pipe = if into_host { Pipe::DataIn } else { Pipe::DataOut };
        // Without streams, the data moves in the order the Ready IUs went.
        let first = self.started.iter().find(|s| s.pipe == pipe).map(|s| s.n);
        if !self.streams && first != Some(job.tag) {
            return false;
        }
        let Some(transfer) = self.take(pipe, job.tag) else { return false };
        assert_eq!(transfer.n, job.tag, "the data of another command");
        if self.fault(|f| *f == Fault::StallData(job.tag)).is_some() {
            self.ended.push_back((pipe, transfer.n, Err(PipeError::Stall(0)), Vec::new()));
            self.jobs[at].stage = Stage::Moved;
            return true;
        }
        let Ok((bytes, write)) = &job.answer else { unreachable!() };
        let moved = if into_host {
            let n = bytes.len().min(length).min(transfer.length);
            self.came.insert(transfer.n, bytes[..n].to_vec());
            n
        } else {
            let n = transfer.out.len().min(length);
            if let Some(lba) = write {
                for (i, block) in transfer.out[..n].chunks(512).enumerate() {
                    self.blocks[lba + i][..block.len()].copy_from_slice(block);
                }
            }
            n
        };
        self.jobs[at].stage = Stage::Moved;
        self.jobs[at].moved = moved;
        self.ended.push_back((pipe, transfer.n, Ok(moved), Vec::new()));
        true
    }

    /// Reset: forgets every command and transfer.
    fn reset(&mut self) -> Result<(), PipeError> {
        if self.gone {
            return Err(PipeError::Gone);
        }
        self.resets += 1;
        self.jobs.clear();
        self.started.clear();
        self.ended.clear();
        self.faults.retain(|f| {
            !matches!(f, Fault::Silent(_) | Fault::WrongStream(_) | Fault::StallData(_))
        });
        Ok(())
    }

    fn pull(&mut self) {
        self.gone = true;
        for started in self.started.drain(..) {
            self.ended.push_back((started.pipe, started.n, Err(PipeError::Gone), Vec::new()));
        }
    }
}

/// The host side, with the engine and the disk: data of each command by
/// tag.
struct Host {
    uas: Uas,
    disk: Disk,
    data: HashMap<u16, Vec<u8>>,
    /// Each command's IU, as the engine gave it.
    ius: HashMap<u16, Vec<u8>>,
    done: Vec<(u16, Result<Reply, TransportError>)>,
}

impl Host {
    fn new(streams: bool, blocks: usize) -> Host {
        let uas = Uas::new(if streams { Some(16) } else { None });
        let disk = Disk::new(streams, blocks);
        Host { uas, disk, data: HashMap::new(), ius: HashMap::new(), done: Vec::new() }
    }

    /// Starts `cdb` with tag `tag`; data out of `out`, or `length` in.
    fn start(&mut self, tag: u16, cdb: &Cdb, into_host: bool, length: usize, out: Vec<u8>) -> bool {
        let data = (length > 0).then_some((into_host, length as u32));
        self.data.insert(tag, out);
        let Some(iu) = self.uas.start(tag, 0, cdb, data) else { return false };
        self.ius.insert(tag, iu.to_vec());
        true
    }

    /// Carries the engine's actions to the disk, and the disk's answers
    /// to the engine, until nothing moves; resets the disk if the engine
    /// asks.
    fn run(&mut self) {
        loop {
            while let Some(action) = self.uas.next_action() {
                match action {
                    Action::Start(Pipe::Command, n) => {
                        let iu = self.ius[&n].clone();
                        self.disk.start(Pipe::Command, n, iu, 0);
                    }
                    Action::Start(Pipe::Status, n) => {
                        self.disk.start(Pipe::Status, n, Vec::new(), STATUS_LENGTH);
                    }
                    Action::Start(pipe @ Pipe::DataIn, n) => {
                        self.disk.start(pipe, n, Vec::new(), 1 << 20);
                    }
                    Action::Start(pipe @ Pipe::DataOut, n) => {
                        let out = self.data[&n].clone();
                        self.disk.start(pipe, n, out, 0);
                    }
                    Action::Cancel(pipe, n) => self.disk.cancel(pipe, n),
                }
            }
            if self.uas.broken().is_some() {
                let result = self.disk.reset();
                self.uas.reset(result);
                continue;
            }
            while let Some(done) = self.uas.next_done() {
                self.done.push(done);
            }
            if let Some((pipe, n, result, bytes)) = self.disk.ended.pop_front() {
                if pipe == Pipe::DataIn
                    && let Some(came) = self.disk.came.remove(&n)
                {
                    self.data.insert(n, came);
                }
                self.uas.ended(pipe, n, result, &bytes);
                continue;
            }
            if !self.disk.step() {
                break;
            }
        }
    }

    fn reply(&self, tag: u16) -> Option<Result<Reply, TransportError>> {
        self.done.iter().find(|(t, _)| *t == tag).map(|(_, reply)| *reply)
    }
}

fn read(lba: u32, blocks: u32) -> Cdb {
    Cdb::read(u64::from(lba), blocks, false).unwrap()
}

fn write(lba: u32, blocks: u32) -> Cdb {
    Cdb::write(u64::from(lba), blocks, false).unwrap()
}

#[test]
fn a_command_iu_holds_its_tag_lun_and_cdb() {
    let iu = command_iu(0x1234, 3, &read(0x0102_0304, 8));
    let mut expected = [0u8; COMMAND_LENGTH];
    expected[0] = 1;
    expected[2..4].copy_from_slice(&[0x12, 0x34]);
    expected[9] = 3;
    expected[16..26].copy_from_slice(&[op::READ_10, 0, 1, 2, 3, 4, 0, 0, 8, 0]);
    assert_eq!(iu, expected);
}

#[test]
fn the_engine_fits_a_kernel_stack_frame() {
    // Built where the kernel's frames are held to 2 KiB.
    assert!(core::mem::size_of::<Uas>() <= 1536, "{}", core::mem::size_of::<Uas>());
}

#[test]
fn the_depth_is_two_less_than_the_tags() {
    assert_eq!(Uas::new(None).depth(), 30);
    assert_eq!(Uas::new(Some(16)).depth(), 14);
    assert_eq!(Uas::new(Some(256)).depth(), MAX_TAGS - 2);
    assert_eq!(Uas::new(Some(2)).depth(), 1);
    let mut uas = Uas::new(Some(4));
    assert!(uas.streams());
    let tur = Cdb::test_unit_ready();
    assert!(uas.start(0, 0, &tur, None).is_none());
    assert!(uas.start(3, 0, &tur, None).is_none());
    assert!(uas.start(2, 0, &tur, None).is_some());
    assert!(uas.start(2, 0, &tur, None).is_none());
    assert_eq!(uas.free_tag(), Some(1));
    assert!(uas.start(1, 0, &tur, None).is_some());
    assert_eq!((uas.free_tag(), uas.in_flight()), (None, 2));
}

#[test]
fn with_streams_the_status_and_data_wait_before_the_command() {
    let mut uas = Uas::new(Some(16));
    assert!(uas.start(5, 0, &read(0, 1), Some((true, 512))).is_some());
    let actions: Vec<Action> = core::iter::from_fn(|| uas.next_action()).collect();
    assert_eq!(
        actions,
        [
            Action::Start(Pipe::Status, 5),
            Action::Start(Pipe::DataIn, 5),
            Action::Start(Pipe::Command, 5)
        ]
    );
    // Without, the data waits for the disk to say it is ready.
    let mut uas = Uas::new(None);
    assert!(uas.start(5, 0, &write(0, 1), Some((false, 512))).is_some());
    let actions: Vec<Action> = core::iter::from_fn(|| uas.next_action()).collect();
    assert_eq!(actions, [Action::Start(Pipe::Status, 1), Action::Start(Pipe::Command, 5)]);
}

/// Reads and writes many blocks at once, all in flight together, the disk
/// answering them first to last or last to first.
fn many_at_once(streams: bool, order: Order) {
    let mut host = Host::new(streams, 64);
    host.disk.order = order;
    let depth = host.uas.depth() as u16;
    for tag in 1..=depth {
        let lba = u32::from(tag);
        if tag % 2 == 0 {
            host.start(tag, &read(lba, 2), true, 1024, Vec::new());
        } else {
            host.start(tag, &write(lba + 32, 1), false, 512, std::vec![tag as u8 + 100; 512]);
        }
    }
    assert_eq!(host.uas.free_tag(), None);
    host.run();
    assert_eq!(host.disk.most, usize::from(depth));
    assert_eq!(host.done.len(), usize::from(depth));
    assert_eq!(host.uas.in_flight(), 0);
    for tag in 1..=depth {
        assert_eq!(host.reply(tag), Some(Ok(Reply::Good { residue: 0 })), "tag {tag}");
        if tag % 2 == 0 {
            let mut expected = std::vec![tag as u8; 512];
            expected.extend([tag as u8 + 1; 512]);
            assert_eq!(host.data[&tag], expected, "tag {tag}");
        } else {
            assert_eq!(host.disk.blocks[usize::from(tag) + 32], [tag as u8 + 100; 512]);
        }
    }
    // The order they finished in is the disk's.
    let order_done: Vec<u16> = host.done.iter().map(|&(tag, _)| tag).collect();
    let mut sorted = order_done.clone();
    sorted.sort_unstable();
    if order == Order::Last {
        assert_ne!(order_done, sorted);
    }
    assert_eq!(host.disk.resets, 0);
}

#[test]
fn many_commands_are_in_flight_at_once_with_streams() {
    many_at_once(true, Order::First);
    many_at_once(true, Order::Last);
}

#[test]
fn many_commands_are_in_flight_at_once_without_streams() {
    many_at_once(false, Order::First);
    many_at_once(false, Order::Last);
}

#[test]
fn a_failed_command_brings_its_sense_and_its_data_is_stopped() {
    for streams in [true, false] {
        let mut host = Host::new(streams, 16);
        // Past the end: the disk fails it at once, without moving data.
        host.start(1, &read(100, 1), true, 512, Vec::new());
        host.start(2, &read(3, 1), true, 512, Vec::new());
        // An opcode it does not know.
        host.start(3, &Cdb::new(&[0xee, 0, 0, 0, 0, 0]), false, 0, Vec::new());
        host.run();
        let past = Sense { key: key::ILLEGAL_REQUEST, asc: 0x21, ascq: 0 };
        assert_eq!(host.reply(1), Some(Ok(Reply::Failed(Some(past)))), "streams {streams}");
        assert_eq!(host.reply(2), Some(Ok(Reply::Good { residue: 0 })));
        let unknown = Sense { key: key::ILLEGAL_REQUEST, asc: 0x20, ascq: 0 };
        assert_eq!(host.reply(3), Some(Ok(Reply::Failed(Some(unknown)))));
        assert_eq!(host.data[&2], [3; 512]);
        // With streams, the data that waited for the failed one was
        // stopped; without, it never started.
        assert!(host.disk.started.is_empty());
        assert_eq!(host.disk.resets, 0);
    }
}

#[test]
fn a_status_other_than_good_or_check_condition_is_a_failure_without_sense() {
    for streams in [true, false] {
        let mut host = Host::new(streams, 16);
        // Busy, then task set full.
        host.disk.faults =
            std::vec![Fault::Status(1, 0x08), Fault::Status(2, 0x28), Fault::Status(3, 0x04)];
        host.start(1, &Cdb::test_unit_ready(), false, 0, Vec::new());
        host.start(2, &Cdb::test_unit_ready(), false, 0, Vec::new());
        host.start(3, &Cdb::test_unit_ready(), false, 0, Vec::new());
        host.run();
        assert_eq!(host.reply(1), Some(Ok(Reply::Failed(None))));
        assert_eq!(host.reply(2), Some(Ok(Reply::Failed(None))));
        // Condition met is good.
        assert_eq!(host.reply(3), Some(Ok(Reply::Good { residue: 0 })));
    }
}

#[test]
fn what_did_not_move_is_the_residue() {
    for streams in [true, false] {
        let mut host = Host::new(streams, 16);
        // 36 bytes of INQUIRY data, of 96 asked for.
        host.start(1, &Cdb::inquiry(96), true, 96, Vec::new());
        host.run();
        assert_eq!(host.reply(1), Some(Ok(Reply::Good { residue: 60 })));
        assert_eq!(host.data[&1].len(), 36);
    }
}

#[test]
fn a_disk_that_breaks_the_protocol_is_reset_and_its_commands_go_again() {
    let faults = [
        (true, Fault::WrongStream(2), "it answered a command on another's stream"),
        (true, Fault::StallData(2), "its data pipe failed"),
        (false, Fault::StallData(2), "its data pipe failed"),
        (true, Fault::Respond(2, 0x02), "it refused a command's IU"),
        (false, Fault::Respond(2, 0x0a), "it refused a command's IU"),
    ];
    for (streams, fault, why) in faults {
        let mut host = Host::new(streams, 16);
        host.disk.faults.push(fault);
        // The broken one last, so that the first is answered before it.
        host.disk.order = Order::First;
        host.start(1, &read(1, 1), true, 512, Vec::new());
        host.start(2, &read(2, 1), true, 512, Vec::new());
        host.start(3, &read(3, 1), true, 512, Vec::new());
        host.run();
        assert_eq!(host.disk.resets, 1, "{fault:?}");
        assert_eq!(host.reply(1), Some(Ok(Reply::Good { residue: 0 })), "{fault:?}");
        // Those after it end with the reset, to go again.
        assert_eq!(host.reply(2), Some(Err(TransportError::Failed(why))), "{fault:?}");
        assert_eq!(host.reply(3), Some(Err(TransportError::Failed(why))), "{fault:?}");
        assert_eq!((host.uas.broken(), host.uas.in_flight()), (None, 0));
        // And they do, with the same tags.
        host.done.clear();
        host.disk.faults.clear();
        host.start(2, &read(2, 1), true, 512, Vec::new());
        host.start(3, &read(3, 1), true, 512, Vec::new());
        host.run();
        assert_eq!(host.reply(2), Some(Ok(Reply::Good { residue: 0 })), "{fault:?}");
        assert_eq!(host.data[&3], [3; 512]);
    }
}

#[test]
fn a_wrong_logical_unit_fails_without_a_reset() {
    for streams in [true, false] {
        let mut host = Host::new(streams, 16);
        host.disk.faults.push(Fault::Respond(1, 0x09));
        host.start(1, &read(1, 1), true, 512, Vec::new());
        host.run();
        assert_eq!(host.reply(1), Some(Ok(Reply::Failed(None))));
        assert_eq!(host.disk.resets, 0);
        // With streams its data waited, and was stopped.
        assert!(host.disk.started.iter().all(|s| s.pipe != Pipe::DataIn));
    }
}

#[test]
fn ius_for_no_command_and_ready_ius_with_streams_are_left_alone() {
    // Without streams, an IU for no command takes a status buffer; the
    // buffer goes again so that the command waiting still gets its own.
    let mut host = Host::new(false, 16);
    host.start(1, &read(1, 1), true, 512, Vec::new());
    host.disk.faults.push(Fault::Stray(9));
    host.run();
    assert_eq!(host.reply(1), Some(Ok(Reply::Good { residue: 0 })));
    // With streams, a Ready IU means nothing: the data waits already.
    let mut host = Host::new(true, 16);
    host.disk.faults.push(Fault::ReadyWithStreams(1));
    host.start(1, &read(1, 1), true, 512, Vec::new());
    host.run();
    assert_eq!(host.reply(1), Some(Ok(Reply::Good { residue: 0 })));
    assert_eq!(host.disk.resets, 0);
}

#[test]
fn an_unplugged_disk_fails_everything_in_flight_and_after() {
    for streams in [true, false] {
        let mut host = Host::new(streams, 16);
        host.disk.faults.push(Fault::Silent(2));
        host.start(1, &read(1, 1), true, 512, Vec::new());
        host.start(2, &read(2, 1), true, 512, Vec::new());
        host.run();
        assert_eq!(host.reply(1), Some(Ok(Reply::Good { residue: 0 })));
        host.disk.pull();
        host.run();
        assert_eq!(host.reply(2), Some(Err(TransportError::Gone)));
        assert!(host.uas.is_gone());
        assert_eq!(host.uas.free_tag(), None);
        assert!(!host.start(3, &read(1, 1), true, 512, Vec::new()));
    }
}

#[test]
fn a_command_that_times_out_has_the_disk_reset() {
    let mut host = Host::new(true, 16);
    host.disk.faults.push(Fault::Silent(1));
    host.start(1, &read(1, 1), true, 512, Vec::new());
    host.run();
    assert_eq!(host.reply(1), None);
    host.uas.timed_out();
    host.run();
    assert_eq!(host.reply(1), Some(Err(TransportError::Failed("it did not answer in time"))));
    assert_eq!(host.disk.resets, 1);
    // Nothing in flight, nothing to time out.
    host.uas.timed_out();
    assert_eq!(host.uas.broken(), None);
}

/// The disk as [`OneAtATime`] drives it.
impl Pipes for Disk {
    fn start(
        &mut self,
        pipe: Pipe,
        n: u16,
        iu: &[u8],
        data: &mut Data<'_>,
    ) -> Result<(), PipeError> {
        match pipe {
            Pipe::Command => Disk::start(self, pipe, n, iu.to_vec(), 0),
            Pipe::Status => Disk::start(self, pipe, n, Vec::new(), STATUS_LENGTH),
            Pipe::DataIn => Disk::start(self, pipe, n, Vec::new(), data.len()),
            Pipe::DataOut => {
                let Data::Out(bytes) = data else { unreachable!() };
                Disk::start(self, pipe, n, bytes.to_vec(), 0);
            }
        }
        Ok(())
    }

    fn cancel(&mut self, pipe: Pipe, n: u16) {
        Disk::cancel(self, pipe, n);
    }

    fn wait(
        &mut self,
        ms: u32,
        status: &mut [u8; STATUS_LENGTH],
        data: &mut Data<'_>,
    ) -> Option<(Pipe, u16, Result<usize, PipeError>)> {
        if self.ended.is_empty() && !self.step() {
            self.now += u64::from(ms);
            return None;
        }
        let (pipe, n, result, bytes) = self.ended.pop_front()?;
        status[..bytes.len()].copy_from_slice(&bytes);
        if pipe == Pipe::DataIn
            && let (Some(came), Data::In(buffer)) = (self.came.remove(&n), data)
        {
            buffer[..came.len()].copy_from_slice(&came);
        }
        Some((pipe, n, result))
    }

    fn reset(&mut self) -> Result<(), PipeError> {
        Disk::reset(self)
    }

    fn now_ms(&mut self) -> u64 {
        self.now
    }
}

#[test]
fn one_command_at_a_time_finds_the_disk() {
    for streams in [true, false] {
        let mut uas = Uas::new(streams.then_some(4));
        let mut disk = Disk::new(streams, 2048);
        let mut transport = OneAtATime { uas: &mut uas, pipes: &mut disk };
        let found = probe(&mut transport, 0, &mut |_| {}).unwrap();
        assert_eq!(
            found.capacity,
            Capacity { blocks: 2048, block_length: 512, physical_exponent: 0 }
        );
        assert_eq!(found.inquiry.vendor(), "QEMU");
        assert!(!found.read_only && !found.write_cache);
        // A block written, and read back.
        let reply = transport.command(0, &write(7, 1), Data::Out(&[0x5a; 512]), 1000);
        assert_eq!(reply, Ok(Reply::Good { residue: 0 }));
        let mut block = [0u8; 512];
        let reply = transport.command(0, &read(7, 1), Data::In(&mut block), 1000);
        assert_eq!((reply, block), (Ok(Reply::Good { residue: 0 }), [0x5a; 512]));
        // A logical unit it does not have.
        let reply = transport.command(1, &Cdb::test_unit_ready(), Data::None, 1000);
        let unsupported = Sense { key: key::ILLEGAL_REQUEST, asc: 0x25, ascq: 0 };
        assert_eq!(reply, Ok(Reply::Failed(Some(unsupported))));
        assert!(disk.ius.iter().all(|iu| u16::from_be_bytes([iu[2], iu[3]]) == 1));
    }
}

#[test]
fn one_command_at_a_time_resets_a_disk_that_goes_silent() {
    let mut uas = Uas::new(Some(4));
    let mut disk = Disk::new(true, 16);
    disk.faults.push(Fault::Silent(1));
    let mut transport = OneAtATime { uas: &mut uas, pipes: &mut disk };
    let reply = transport.command(0, &Cdb::test_unit_ready(), Data::None, 5000);
    assert_eq!(reply, Err(TransportError::Failed("it did not answer in time")));
    // The next goes, after the reset.
    let reply = transport.command(0, &Cdb::test_unit_ready(), Data::None, 5000);
    assert_eq!(reply, Ok(Reply::Good { residue: 0 }));
    assert_eq!((disk.resets, disk.now), (1, 5000));
    disk.pull();
    let mut transport = OneAtATime { uas: &mut uas, pipes: &mut disk };
    let reply = transport.command(0, &Cdb::test_unit_ready(), Data::None, 5000);
    assert_eq!(reply, Err(TransportError::Gone));
}

/// A configuration with a Bulk-Only setting 0 and a UAS setting 1, as UAS
/// disks have, with pipe usage descriptors `ids` after setting 1's
/// endpoints, and their addresses `addresses`.
fn configuration(ids: [u8; 4], addresses: [u8; 4]) -> Vec<u8> {
    let mut bytes = std::vec![9, 2, 0, 0, 1, 1, 0, 0x80, 50];
    bytes.extend([9, 4, 0, 0, 2, 8, 6, 0x50, 0]);
    bytes.extend([7, 5, 0x81, 2, 0, 4, 0, 6, 0x30, 15, 0, 0, 0]);
    bytes.extend([7, 5, 0x02, 2, 0, 4, 0, 6, 0x30, 15, 0, 0, 0]);
    bytes.extend([9, 4, 0, 1, 4, 8, 6, 0x62, 0]);
    for (id, address) in ids.into_iter().zip(addresses) {
        let streams = if id == 1 { 0 } else { 5 };
        bytes.extend([7, 5, address, 2, 0, 4, 0]);
        bytes.extend([6, 0x30, 15, streams, 0, 0]);
        bytes.extend([4, PIPE_USAGE, id, 0]);
    }
    let total = bytes.len() as u16;
    bytes[2..4].copy_from_slice(&total.to_le_bytes());
    bytes
}

#[test]
fn the_pipes_are_found_by_their_usage() {
    let bytes = configuration([1, 2, 3, 4], [0x04, 0x83, 0x81, 0x02]);
    let config = Configuration::parse(&bytes).unwrap();
    let mut settings = config.interfaces();
    let bot = settings.next().unwrap();
    assert_eq!(find_pipes(&bot), None);
    let uas = settings.next().unwrap();
    assert_eq!(uas.descriptor.protocol, super::super::PROTOCOL_UAS);
    let pipes = find_pipes(&uas).unwrap();
    assert_eq!(pipes.map(|e| e.address), [0x04, 0x83, 0x81, 0x02]);
    assert_eq!(pipes.map(|e| e.max_streams()), [0, 32, 32, 32]);
    // In any order the endpoints come in.
    let bytes = configuration([3, 4, 2, 1], [0x81, 0x02, 0x83, 0x04]);
    let config = Configuration::parse(&bytes).unwrap();
    let pipes = find_pipes(&config.interfaces().nth(1).unwrap()).unwrap();
    assert_eq!(pipes.map(|e| e.address), [0x04, 0x83, 0x81, 0x02]);
    // Not with one missing, or one the wrong way.
    for (ids, addresses) in
        [([1, 2, 3, 3], [0x04, 0x83, 0x81, 0x82]), ([1, 2, 3, 4], [0x04, 0x83, 0x81, 0x82])]
    {
        let bytes = configuration(ids, addresses);
        let config = Configuration::parse(&bytes).unwrap();
        assert_eq!(find_pipes(&config.interfaces().nth(1).unwrap()), None, "{ids:?} {addresses:?}");
    }
}

proptest! {
    /// Whatever order the disk answers in, and whatever it is asked, each
    /// command ends once, as the disk answered it.
    #[test]
    fn every_command_ends_once(
        streams in any::<bool>(),
        last_first in any::<bool>(),
        asks in proptest::collection::vec((0u32..20, 1u32..4, any::<bool>()), 1..40),
    ) {
        let mut host = Host::new(streams, 16);
        host.disk.order = if last_first { Order::Last } else { Order::First };
        let mut expected = HashMap::new();
        let mut asks = asks.into_iter();
        let mut pending = true;
        while pending {
            pending = false;
            while let Some(tag) = host.uas.free_tag() {
                let Some((lba, blocks, reads)) = asks.next() else { break };
                pending = true;
                let fits = lba + blocks <= 16;
                let length = blocks as usize * 512;
                if reads {
                    host.start(tag, &read(lba, blocks), true, length, Vec::new());
                } else {
                    host.start(tag, &write(lba, blocks), false, length, std::vec![7; length]);
                }
                expected.insert(tag, fits);
            }
            host.run();
            for (tag, reply) in host.done.drain(..) {
                let fits = expected.remove(&tag).unwrap();
                match reply {
                    Ok(Reply::Good { residue: 0 }) => prop_assert!(fits),
                    Ok(Reply::Failed(Some(sense))) => prop_assert!(!fits && sense.asc == 0x21),
                    other => prop_assert!(false, "tag {}: {:?}", tag, other),
                }
            }
            prop_assert_eq!(host.uas.in_flight(), 0);
        }
        prop_assert!(expected.is_empty());
        prop_assert_eq!(host.disk.resets, 0);
    }
}
