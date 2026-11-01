// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The channel against a simulated one: its command block, control port
//! and bus-master registers, and up to two devices, disks whose sectors
//! are a vector and optical drives that answer as packet devices do.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Dma, IoPorts, fake_memory, fake_memory_write};

use super::{Channel, Device, Error, Op};

thread_local! {
    /// The high half of the test's memory's host addresses: the PRDs hold
    /// the low half, as below 4 GiB.
    static HIGH: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

fn full(phys: u32) -> u64 {
    HIGH.with(|h| h.get().expect("memory was allocated")) | u64::from(phys)
}

/// `pages` of the test's memory, in the same 4 GiB as the rest of it. The
/// host's allocator may put a block anywhere, so one that lands in other
/// 4 GiB, or across a 4 GiB line, is kept aside and another asked for.
fn alloc(pages: usize, below: bool) -> Option<Dma> {
    assert!(below, "the PRD table is below 4 GiB");
    let mut aside = Vec::new();
    for _ in 0..64 {
        let dma = Dma::fake(pages * 4096);
        let high = dma.phys() & !0xffff_ffff;
        let last = (dma.phys() + (pages * 4096) as u64 - 1) & !0xffff_ffff;
        let fits = HIGH.with(|h| last == high && *h.get().get_or_insert(high) == high);
        if fits {
            HIGH.with(|h| h.set(Some(high)));
            return Some(dma);
        }
        aside.push(dma);
    }
    panic!("the host gave no memory in the test's 4 GiB")
}

fn low(dma: &Dma) -> u32 {
    dma.phys() as u32
}

#[derive(Clone)]
struct SimDisk {
    sectors: Vec<u8>,
    atapi: bool,
    /// It says it is Serial ATA, behind a SATA controller in IDE mode.
    serial: bool,
    /// Commands it fails, as a bad sector does.
    fails: bool,
    flushes: u32,
}

impl SimDisk {
    fn new(sectors: usize) -> SimDisk {
        SimDisk {
            sectors: vec![0; sectors * 512],
            atapi: false,
            serial: false,
            fails: false,
            flushes: 0,
        }
    }

    fn optical() -> SimDisk {
        SimDisk { atapi: true, ..SimDisk::new(0) }
    }

    fn identify(&self) -> [u16; 256] {
        let mut id = [0u16; 256];
        let count = (self.sectors.len() / 512) as u64;
        id[82] = 1 << 5;
        id[83] = 0x4000 | 1 << 10 | 1 << 13;
        id[85] = 1 << 5;
        id[86] = 1 << 10 | 1 << 13;
        id[87] = 0x4000;
        for (i, word) in id[100..104].iter_mut().enumerate() {
            *word = (count >> (16 * i)) as u16;
        }
        if self.serial {
            // Serial ATA Gen 1 and 2.
            id[76] = 0b110;
        }
        let model = b"QEMU HARDDISK                           ";
        for (i, pair) in model.chunks(2).enumerate() {
            id[27 + i] = u16::from_be_bytes([pair[0], pair[1]]);
        }
        id
    }
}

/// A register that keeps what was written before, as 48-bit commands
/// read it: the high byte first, then the low.
#[derive(Clone, Copy, Default)]
struct Pair {
    now: u8,
    before: u8,
}

impl Pair {
    fn write(&mut self, value: u8) {
        self.before = self.now;
        self.now = value;
    }
}

#[derive(Default)]
struct State {
    disks: [Option<SimDisk>; 2],
    selected: usize,
    status: u8,
    error: u8,
    count: Pair,
    lba: [Pair; 3],
    data: VecDeque<u16>,
    control: u8,
    bm_command: u8,
    bm_status: u8,
    prd: u32,
    /// A DMA command waiting for the bus master: read, LBA, count.
    pending: Option<(bool, u64, u32)>,
    /// While set, a started transfer waits for the test.
    hold: bool,
    /// What each transfer's PRDs were.
    prds: Vec<Vec<(u32, u32)>>,
    /// The LBAs and counts of the commands.
    commands: Vec<(u8, u64, u32)>,
}

const READY: u8 = 1 << 6;
const BSY: u8 = 1 << 7;
const DRQ: u8 = 1 << 3;
const ERR: u8 = 1 << 0;

impl State {
    fn status(&self) -> u8 {
        if self.disks.iter().all(Option::is_none) {
            return 0xff;
        }
        if self.disks[self.selected].is_none() { 0 } else { self.status }
    }

    fn lba48(&self) -> u64 {
        let mut lba = 0u64;
        for (i, pair) in self.lba.iter().enumerate() {
            lba |= u64::from(pair.now) << (8 * i) | u64::from(pair.before) << (8 * (i + 3));
        }
        lba
    }

    fn command(&mut self, code: u8) {
        let Some(disk) = self.disks[self.selected].clone() else { return };
        match code {
            0xec if disk.atapi => {
                self.status = READY | ERR;
                self.error = 1 << 2;
                self.lba[1].now = 0x14;
                self.lba[2].now = 0xeb;
            }
            0xec => {
                self.data = disk.identify().into_iter().collect();
                self.status = READY | DRQ;
            }
            0x25 | 0x35 => {
                let count = u32::from(u16::from_le_bytes([self.count.now, self.count.before]));
                let count = if count == 0 { 65_536 } else { count };
                let lba = self.lba48();
                self.commands.push((code, lba, count));
                self.pending = Some((code == 0x25, lba, count));
                self.status = READY | BSY;
            }
            0xea => {
                self.disks[self.selected].as_mut().unwrap().flushes += 1;
                self.status = READY;
            }
            _ => {
                self.status = READY | ERR;
                self.error = 1 << 2;
            }
        }
    }

    /// Runs the pending transfer through the PRD table.
    fn transfer(&mut self) {
        let Some((read, lba, count)) = self.pending.take() else { return };
        let mut prds = Vec::new();
        let mut at = self.prd;
        loop {
            let mut entry = [0u8; 8];
            fake_memory(full(at), &mut entry);
            let address = u32::from_le_bytes(entry[0..4].try_into().unwrap());
            let flags = u32::from_le_bytes(entry[4..8].try_into().unwrap());
            let size = if flags & 0xffff == 0 { 0x1_0000 } else { flags & 0xffff };
            prds.push((address, size));
            if flags & 1 << 31 != 0 {
                break;
            }
            at += 8;
        }
        self.prds.push(prds.clone());
        let disk = self.disks[self.selected].as_mut().unwrap();
        let start = lba as usize * 512;
        let end = start + count as usize * 512;
        if disk.fails || end > disk.sectors.len() {
            self.status = READY | ERR;
            self.error = 1 << 6;
        } else {
            let mut offset = start;
            for (address, size) in prds {
                let size = size as usize;
                if read {
                    fake_memory_write(full(address), &disk.sectors[offset..offset + size]);
                } else {
                    fake_memory(full(address), &mut disk.sectors[offset..offset + size]);
                }
                offset += size;
            }
            self.status = READY;
        }
        self.bm_status &= !1;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Command,
    Control,
    BusMaster,
}

#[derive(Clone)]
struct Port {
    sim: Arc<Mutex<State>>,
    block: Block,
}

impl IoPorts for Port {
    fn len(&self) -> u16 {
        if self.block == Block::Control { 1 } else { 8 }
    }

    fn read8(&self, offset: u16) -> u8 {
        let s = self.sim.lock().unwrap();
        match (self.block, offset) {
            (Block::Command, 1) => s.error,
            (Block::Command, 2) => s.count.now,
            (Block::Command, o @ 3..=5) => s.lba[usize::from(o - 3)].now,
            (Block::Command, 7) | (Block::Control, 0) => s.status(),
            (Block::BusMaster, 0) => s.bm_command,
            (Block::BusMaster, 2) => s.bm_status,
            _ => 0,
        }
    }

    fn write8(&self, offset: u16, value: u8) {
        let mut s = self.sim.lock().unwrap();
        match (self.block, offset) {
            (Block::Command, 2) => s.count.write(value),
            (Block::Command, o @ 3..=5) => s.lba[usize::from(o - 3)].write(value),
            (Block::Command, 6) => s.selected = usize::from(value >> 4 & 1),
            (Block::Command, 7) => s.command(value),
            (Block::Control, 0) => {
                if s.control & 4 != 0 && value & 4 == 0 {
                    // The end of a software reset: each device shows its
                    // signature, and the master is picked.
                    s.selected = 0;
                    s.status = READY;
                    s.pending = None;
                    let atapi = s.disks[0].as_ref().is_some_and(|d| d.atapi);
                    let signature = if atapi { (0x14, 0xeb) } else { (0, 0) };
                    s.lba[1].now = signature.0;
                    s.lba[2].now = signature.1;
                }
                s.control = value;
            }
            (Block::BusMaster, 0) => {
                s.bm_command = value;
                if value & 1 != 0 {
                    s.bm_status |= 1;
                    if !s.hold {
                        s.transfer();
                    }
                } else {
                    s.bm_status &= !1;
                }
            }
            (Block::BusMaster, 2) => {
                s.bm_status = s.bm_status & !(value & 6) & !0x60 | value & 0x60;
            }
            _ => {}
        }
    }

    fn read16(&self, offset: u16) -> u16 {
        let mut s = self.sim.lock().unwrap();
        if self.block != Block::Command || offset != 0 {
            return 0;
        }
        let word = s.data.pop_front().unwrap_or(0);
        if s.data.is_empty() {
            s.status &= !DRQ;
        }
        word
    }

    fn write16(&self, _offset: u16, _value: u16) {}

    fn read32(&self, offset: u16) -> u32 {
        let s = self.sim.lock().unwrap();
        if self.block == Block::BusMaster && offset == 4 { s.prd } else { 0 }
    }

    fn write32(&self, offset: u16, value: u32) {
        if self.block == Block::BusMaster && offset == 4 {
            self.sim.lock().unwrap().prd = value;
        }
    }
}

struct Rig {
    sim: Arc<Mutex<State>>,
    channel: Channel<Port>,
}

fn rig(disks: [Option<SimDisk>; 2]) -> Rig {
    let sim = Arc::new(Mutex::new(State { disks, status: READY, ..State::default() }));
    let port = |block| Port { sim: sim.clone(), block };
    let channel = Channel::new(
        port(Block::Command),
        port(Block::Control),
        port(Block::BusMaster),
        &mut alloc,
    )
    .unwrap();
    Rig { sim, channel }
}

fn delay(_us: u32) {}

#[test]
fn finds_a_disk_and_leaves_an_optical_drive() {
    let mut rig = rig([Some(SimDisk::new(2048)), Some(SimDisk::optical())]);
    assert!(rig.channel.present());
    assert!(rig.channel.reset(&mut delay));
    let Device::Ata { identity: id, serial } = rig.channel.identify(0, &mut delay) else {
        panic!("no disk")
    };
    assert!(!serial, "a parallel ATA disk");
    assert_eq!(id.sectors, 2048);
    assert_eq!(radix_ahci::ata::trimmed(&id.model), "QEMU HARDDISK");
    assert!(id.write_cache && id.flush);
    assert_eq!(rig.channel.identify(1, &mut delay), Device::Atapi);
    assert_eq!(rig.sim.lock().unwrap().control & 2, 2, "its interrupt is off");
}

#[test]
fn a_serial_ata_disk_says_so() {
    let mut disk = SimDisk::new(64);
    disk.serial = true;
    let mut rig = rig([Some(disk), None]);
    assert!(matches!(rig.channel.identify(0, &mut delay), Device::Ata { serial: true, .. }));
}

#[test]
fn an_empty_end_or_channel_is_absent() {
    let mut rig = self::rig([Some(SimDisk::new(64)), None]);
    assert_eq!(rig.channel.identify(1, &mut delay), Device::Absent);
    let rig = self::rig([None, None]);
    assert!(!rig.channel.present(), "a floating bus reads all ones");
}

fn run(
    rig: &mut Rig,
    drive: u8,
    op: Op,
    lba: u64,
    count: u32,
    buffers: &[(u32, u32)],
) -> Result<(), Error> {
    rig.channel.start(drive, op, lba, count, buffers)?;
    rig.channel.poll().expect("done")
}

#[test]
fn what_is_written_reads_back() {
    let mut rig = rig([Some(SimDisk::new(64)), Some(SimDisk::new(64))]);
    let out = alloc(1, true).unwrap();
    let back = alloc(1, true).unwrap();
    let data: Vec<u8> = (0..4096).map(|i| (i * 7) as u8).collect();
    out.write(0, &data);
    assert_eq!(run(&mut rig, 1, Op::Write, 8, 8, &[(low(&out), 4096)]), Ok(()));
    assert_eq!(&rig.sim.lock().unwrap().disks[1].as_ref().unwrap().sectors[4096..8192], &data[..]);
    assert_eq!(
        run(&mut rig, 1, Op::Read, 8, 8, &[(low(&back), 2048), (low(&back) + 2048, 2048)]),
        Ok(())
    );
    let mut read = vec![0u8; 4096];
    back.read(0, &mut read);
    assert_eq!(read, data);
    assert!(rig.sim.lock().unwrap().disks[0].as_ref().unwrap().sectors.iter().all(|&b| b == 0));
    assert_eq!(rig.sim.lock().unwrap().bm_command & 1, 0, "the bus master is stopped");
}

#[test]
fn a_buffer_across_64_kib_takes_two_prds() {
    let mut rig = rig([Some(SimDisk::new(64)), None]);
    // 68 KiB always holds a 64 KiB line with 2 KiB either side, and stays
    // under the host allocator's threshold for blocks of their own, which
    // land anywhere.
    let buffer = alloc(17, true).unwrap();
    let start = low(&buffer);
    // From 2 KiB before the next 64 KiB line, 4 KiB.
    let at = (start | 0xffff).wrapping_add(1).wrapping_sub(2048);
    let at = if at >= start && u64::from(at) + 4096 <= u64::from(start) + 17 * 4096 {
        at
    } else {
        start
    };
    assert_eq!(run(&mut rig, 0, Op::Read, 0, 8, &[(at, 4096)]), Ok(()));
    let prds = rig.sim.lock().unwrap().prds.last().unwrap().clone();
    if at != start {
        assert_eq!(prds, [(at, 2048), (at + 2048, 2048)]);
    }
    assert_eq!(prds.iter().map(|p| p.1).sum::<u32>(), 4096);
}

#[test]
fn addresses_take_48_bits_high_byte_first() {
    let mut rig = rig([Some(SimDisk::new(64)), None]);
    let buffer = alloc(1, true).unwrap();
    let lba = 0x0000_1234_5678_9abc;
    // Past the disk's end: it fails, but the address it saw is whole.
    let result = run(&mut rig, 0, Op::Read, lba, 1, &[(low(&buffer), 512)]);
    assert_eq!(result, Err(Error::Device { status: READY | ERR, error: 1 << 6 }));
    assert_eq!(rig.sim.lock().unwrap().commands.last(), Some(&(0x25, lba, 1)));
    let result = run(&mut rig, 0, Op::Read, 0, 65_536, &[(low(&buffer), 512)]);
    assert_eq!(result, Err(Error::BadRequest), "the buffers hold one sector, not 65,536");
}

#[test]
fn a_flush_reaches_the_disk() {
    let mut rig = rig([Some(SimDisk::new(64)), None]);
    assert_eq!(run(&mut rig, 0, Op::Flush, 0, 0, &[]), Ok(()));
    assert_eq!(rig.sim.lock().unwrap().disks[0].as_ref().unwrap().flushes, 1);
}

#[test]
fn a_failing_disk_says_why() {
    let mut disk = SimDisk::new(64);
    disk.fails = true;
    let mut rig = rig([Some(disk), None]);
    let buffer = alloc(1, true).unwrap();
    let result = run(&mut rig, 0, Op::Read, 0, 1, &[(low(&buffer), 512)]);
    assert_eq!(result, Err(Error::Device { status: READY | ERR, error: 1 << 6 }));
    assert!(!rig.channel.busy());
}

#[test]
fn one_command_at_a_time() {
    let mut rig = rig([Some(SimDisk::new(64)), Some(SimDisk::new(64))]);
    let buffer = alloc(1, true).unwrap();
    rig.sim.lock().unwrap().hold = true;
    rig.channel.start(0, Op::Read, 0, 1, &[(low(&buffer), 512)]).unwrap();
    assert_eq!(rig.channel.poll(), None, "still moving");
    assert_eq!(rig.channel.in_flight(), Some(0));
    assert_eq!(rig.channel.start(1, Op::Read, 0, 1, &[(low(&buffer), 512)]), Err(Error::Busy));
    {
        let mut s = rig.sim.lock().unwrap();
        s.hold = false;
        s.transfer();
    }
    assert_eq!(rig.channel.poll(), Some(Ok(())));
    assert_eq!(rig.channel.poll(), None, "nothing in flight");
    assert_eq!(run(&mut rig, 1, Op::Read, 0, 1, &[(low(&buffer), 512)]), Ok(()));
}

#[test]
fn a_reset_drops_what_was_in_flight() {
    let mut rig = rig([Some(SimDisk::new(64)), None]);
    let buffer = alloc(1, true).unwrap();
    rig.sim.lock().unwrap().hold = true;
    rig.channel.start(0, Op::Write, 0, 1, &[(low(&buffer), 512)]).unwrap();
    assert!(rig.channel.reset(&mut delay));
    assert!(!rig.channel.busy());
    rig.sim.lock().unwrap().hold = false;
    assert_eq!(run(&mut rig, 0, Op::Write, 0, 1, &[(low(&buffer), 512)]), Ok(()));
}

#[test]
fn bad_requests_are_refused() {
    let mut rig = rig([Some(SimDisk::new(64)), None]);
    let buffer = alloc(1, true).unwrap();
    let at = low(&buffer);
    assert_eq!(rig.channel.start(2, Op::Read, 0, 1, &[(at, 512)]), Err(Error::BadRequest));
    assert_eq!(rig.channel.start(0, Op::Read, 0, 0, &[(at, 512)]), Err(Error::BadRequest));
    assert_eq!(rig.channel.start(0, Op::Read, 0, 1, &[(at, 100)]), Err(Error::BadRequest));
    assert_eq!(rig.channel.start(0, Op::Read, 0, 1, &[]), Err(Error::BadRequest));
}
