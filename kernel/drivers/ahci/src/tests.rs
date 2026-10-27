// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated AHCI controller: its registers, ports
//! whose links come up when reset, and disks that carry out the commands
//! in their slots, at once or when a test says, failing on sectors marked
//! bad.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Dma, Mmio, Registers, fake_memory, fake_memory_write};

use crate::ata::{self, Taskfile, command};
use crate::{Absent, Error, Hba, Op, Outcome, Port};

const PORTS: usize = 4;
const ABAR: usize = 0x100 + PORTS * 0x80;
const SECTORS: u64 = 1 << 24;

const CAP: u32 = (1 << 31) | (1 << 30) | (1 << 24) | (31 << 8) | (PORTS as u32 - 1);

/// A disk on a simulated port.
struct Disk {
    /// The sectors written; the others hold their number's pattern.
    written: std::collections::BTreeMap<u64, Vec<u8>>,
    identify: [u16; 256],
    /// Sectors that fail to read or write.
    bad: Vec<u64>,
    /// Whether commands wait for [`Sim::finish`] instead of finishing as
    /// they are issued.
    hold: bool,
    /// Commands accepted and not finished: slot, and whether it failed.
    held: Vec<(usize, bool)>,
    /// After a queued command's error: refuses everything but its error
    /// log, which names `failed`.
    ncq_error: Option<usize>,
    /// Whether its error log can be read.
    log_works: bool,
    /// The error register of a failed command: UNC (uncorrectable data).
    error: u8,
    signature: u32,
    flushes: u32,
    trims: Vec<(u64, u64)>,
    /// The commands it was sent: code, device register, count register.
    commands: Vec<(u8, u8, u16)>,
}

/// What sector `sector` holds before it is written.
fn pattern(sector: u64) -> Vec<u8> {
    (sector * 512..sector * 512 + 512).map(|i| (i % 251) as u8).collect()
}

impl Disk {
    fn sector(&self, sector: u64) -> Vec<u8> {
        self.written.get(&sector).cloned().unwrap_or_else(|| pattern(sector))
    }

    fn new() -> Disk {
        Disk {
            written: std::collections::BTreeMap::new(),
            identify: identify_words(SECTORS, 32, true),
            bad: Vec::new(),
            hold: false,
            held: Vec::new(),
            ncq_error: None,
            log_works: true,
            error: 0x40,
            signature: 0x101,
            flushes: 0,
            trims: Vec::new(),
            commands: Vec::new(),
        }
    }
}

/// IDENTIFY DEVICE data of a solid-state disk of `sectors` sectors that
/// queues `depth` commands, with a write cache, FUA and, if `trim`, TRIM.
fn identify_words(sectors: u64, depth: u16, trim: bool) -> [u16; 256] {
    let mut id = [0u16; 256];
    let text = |id: &mut [u16; 256], from: usize, s: &str, words: usize| {
        let mut bytes = std::vec![b' '; words * 2];
        bytes[..s.len()].copy_from_slice(s.as_bytes());
        for i in 0..words {
            id[from + i] = u16::from_be_bytes([bytes[2 * i], bytes[2 * i + 1]]);
        }
    };
    text(&mut id, 10, "SIM0001", 10);
    text(&mut id, 23, "1.0", 4);
    text(&mut id, 27, "Simulated SSD", 20);
    id[49] = 1 << 9;
    if depth > 0 {
        id[75] = depth - 1;
        id[76] = (1 << 8) | (1 << 2);
    } else {
        id[76] = 1 << 2;
    }
    id[82] = 1 << 5;
    id[83] = 0x4000 | (1 << 10) | (1 << 13);
    id[84] = 0x4000 | (1 << 6);
    id[85] = 1 << 5;
    id[86] = (1 << 10) | (1 << 13);
    id[87] = 0x4000;
    id[100] = sectors as u16;
    id[101] = (sectors >> 16) as u16;
    id[102] = (sectors >> 32) as u16;
    id[103] = (sectors >> 48) as u16;
    id[105] = 1;
    id[106] = 0x4000 | (1 << 13) | 3;
    id[169] = u16::from(trim);
    id[217] = 1;
    checksum(&mut id);
    id
}

fn checksum(id: &mut [u16; 256]) {
    id[255] = 0xa5;
    let sum = id.iter().fold(0u8, |sum, w| {
        let [low, high] = w.to_le_bytes();
        sum.wrapping_add(low).wrapping_add(high)
    });
    id[255] |= u16::from(0u8.wrapping_sub(sum)) << 8;
}

#[derive(Default)]
struct PortRegs {
    clb: u64,
    fb: u64,
    is: u32,
    ie: u32,
    cmd: u32,
    tfd: u32,
    sig: u32,
    ssts: u32,
    sctl: u32,
    serr: u32,
    sact: u32,
    ci: u32,
    /// The commands whose frames came in: the slots set in CI that the
    /// disk took.
    taken: u32,
    disk: Option<Disk>,
}

struct State {
    ghc: u32,
    is: u32,
    cap: u32,
    cap2: u32,
    bohc: u32,
    ports: Vec<PortRegs>,
    resets: u32,
}

/// The controller.
struct Sim(Mutex<State>);

fn read_mem(phys: u64, len: usize) -> Vec<u8> {
    let mut out = std::vec![0u8; len];
    fake_memory(phys, &mut out);
    out
}

fn u32_at(phys: u64) -> u32 {
    u32::from_le_bytes(read_mem(phys, 4).try_into().unwrap())
}

fn u64_at(phys: u64) -> u64 {
    u64::from_le_bytes(read_mem(phys, 8).try_into().unwrap())
}

impl PortRegs {
    fn link_down(&mut self) {
        self.ssts = 0;
        self.tfd = 0x7f;
        self.sig = u32::MAX;
    }

    fn link_up(&mut self) {
        match &mut self.disk {
            Some(disk) => {
                // A link reset clears what the disk held, and its error.
                disk.ncq_error = None;
                disk.held.clear();
                self.ssts = 0x113;
                self.tfd = 0x50;
                self.sig = disk.signature;
                self.serr |= 1 << 26;
            }
            None => self.link_down(),
        }
    }

    /// Takes the slots newly set in CI.
    fn issue(&mut self) {
        if self.cmd & 1 == 0 {
            return;
        }
        let new = self.ci & !self.taken;
        for slot in 0..32 {
            if new & (1 << slot) != 0 {
                self.taken |= 1 << slot;
                self.run(slot);
            }
        }
    }

    /// Carries out the command in `slot`.
    fn run(&mut self, slot: usize) {
        let header = self.clb + 32 * slot as u64;
        let flags = u32_at(header);
        let table = u64_at(header + 8);
        let prdtl = (flags >> 16) as usize;
        let fis = read_mem(table, 20);
        assert_eq!(fis[0], ata::FIS_H2D);
        assert_eq!(flags & 0x1f, 5);
        let code = fis[2];
        let features = u16::from_le_bytes([fis[3], fis[11]]);
        let lba = u64::from_le_bytes([fis[4], fis[5], fis[6], fis[8], fis[9], fis[10], 0, 0]);
        let device = fis[7];
        let count = u16::from_le_bytes([fis[12], fis[13]]);
        let buffers: Vec<(u64, usize)> = (0..prdtl)
            .map(|i| {
                let entry = table + 0x80 + 16 * i as u64;
                (u64_at(entry), (u32_at(entry + 12) & 0x3f_ffff) as usize + 1)
            })
            .collect();
        let disk = self.disk.as_mut().expect("a command to a port with no disk");
        disk.commands.push((code, device, count));
        let queued = matches!(code, command::READ_FPDMA_QUEUED | command::WRITE_FPDMA_QUEUED);
        if queued {
            assert_eq!(usize::from(count >> 3), slot, "an NCQ tag is its slot");
            assert!(self.sact & (1 << slot) != 0, "SActive set before Command Issue");
        } else {
            assert!(self.ci & !(1 << slot) == 0 && self.sact == 0, "a lone command alone");
        }
        if disk.ncq_error.is_some() && !(code == command::READ_LOG_EXT && lba == 0x10) {
            self.fail(slot);
            return;
        }
        let mut failed = false;
        match code {
            command::IDENTIFY_DEVICE => {
                let mut bytes = Vec::new();
                for w in disk.identify {
                    bytes.extend_from_slice(&w.to_le_bytes());
                }
                fake_memory_write(buffers[0].0, &bytes);
            }
            command::READ_LOG_EXT => {
                if !disk.log_works {
                    failed = true;
                } else {
                    let mut page = [0u8; 512];
                    page[0] = match disk.ncq_error.take() {
                        Some(tag) => tag as u8,
                        None => 0x80,
                    };
                    (page[2], page[3]) = (0x51, disk.error);
                    fake_memory_write(buffers[0].0, &page);
                }
            }
            command::READ_DMA_EXT
            | command::WRITE_DMA_EXT
            | command::WRITE_DMA_FUA_EXT
            | command::READ_FPDMA_QUEUED
            | command::WRITE_FPDMA_QUEUED => {
                let sectors = u64::from(if queued { features } else { count });
                let write = matches!(
                    code,
                    command::WRITE_DMA_EXT
                        | command::WRITE_DMA_FUA_EXT
                        | command::WRITE_FPDMA_QUEUED
                );
                assert_eq!(write, flags & (1 << 6) != 0, "the header's write bit");
                let total: usize = buffers.iter().map(|b| b.1).sum();
                assert_eq!(total as u64, sectors * 512);
                if (lba..lba + sectors).any(|s| disk.bad.contains(&s)) {
                    failed = true;
                } else {
                    let mut sector = lba;
                    for &(phys, len) in &buffers {
                        assert_eq!(len % 512, 0);
                        for i in 0..len / 512 {
                            let at = phys + 512 * i as u64;
                            if write {
                                disk.written.insert(sector, read_mem(at, 512));
                            } else {
                                fake_memory_write(at, &disk.sector(sector));
                            }
                            sector += 1;
                        }
                    }
                }
            }
            command::FLUSH_CACHE_EXT => disk.flushes += 1,
            command::DATA_SET_MANAGEMENT => {
                assert_eq!(features, ata::DSM_TRIM);
                let page = read_mem(buffers[0].0, 512 * usize::from(count));
                for entry in page.as_chunks::<8>().0 {
                    let value = u64::from_le_bytes(*entry);
                    let length = value >> 48;
                    if length != 0 {
                        disk.trims.push((value & ((1 << 48) - 1), length));
                    }
                }
            }
            other => panic!("unexpected command {other:#x}"),
        }
        if disk.hold && !matches!(code, command::IDENTIFY_DEVICE | command::READ_LOG_EXT) {
            if queued {
                // The disk takes the command: Command Issue clears, and
                // SActive stays until it finishes.
                self.ci &= !(1 << slot);
                self.taken &= !(1 << slot);
                self.is |= 1;
            }
            disk.held.push((slot, failed));
            return;
        }
        if failed {
            self.fail(slot);
        } else {
            self.finish(slot, queued);
        }
    }

    fn finish(&mut self, slot: usize, queued: bool) {
        self.ci &= !(1 << slot);
        self.taken &= !(1 << slot);
        if queued {
            self.sact &= !(1 << slot);
            self.is |= 1 << 3;
        } else {
            self.is |= 1;
        }
        self.tfd = 0x50;
    }

    /// The disk reports an error: a queued command's aborts all the queued
    /// ones, and the controller stops on it.
    fn fail(&mut self, slot: usize) {
        let disk = self.disk.as_mut().unwrap();
        if self.sact & (1 << slot) != 0 && disk.ncq_error.is_none() {
            disk.ncq_error = Some(slot);
        }
        self.tfd = u32::from(disk.error) << 8 | 0x51;
        self.is |= 1 << 30;
        self.cmd = (self.cmd & !(0x1f << 8)) | (slot as u32) << 8;
    }
}

impl State {
    fn reset(&mut self) {
        self.ghc = 0;
        self.is = 0;
        self.resets += 1;
        for port in &mut self.ports {
            let disk = port.disk.take();
            *port = PortRegs { disk, ..PortRegs::default() };
            port.link_up();
        }
    }

    fn update_is(&mut self) {
        for (i, port) in self.ports.iter().enumerate() {
            if (port.is | (port.serr >> 26 & 1) << 6) & port.ie != 0 {
                self.is |= 1 << i;
            }
        }
    }
}

impl Registers for Sim {
    fn read(&self, offset: usize, size: usize) -> u64 {
        assert_eq!(size, 4, "AHCI registers are read as words");
        let mut s = self.0.lock().unwrap();
        s.update_is();
        let value = match offset {
            0x00 => s.cap,
            0x04 => s.ghc,
            0x08 => s.is,
            0x0c => (1 << PORTS) - 1,
            0x10 => 0x0001_0301,
            0x24 => s.cap2,
            0x28 => s.bohc,
            o if o >= 0x100 => {
                let port = &s.ports[(o - 0x100) / 0x80];
                match (o - 0x100) % 0x80 {
                    0x00 => port.clb as u32,
                    0x04 => (port.clb >> 32) as u32,
                    0x08 => port.fb as u32,
                    0x0c => (port.fb >> 32) as u32,
                    // PCS shows PxSERR.DIAG.X.
                    0x10 => port.is | (port.serr >> 26 & 1) << 6,
                    0x14 => port.ie,
                    0x18 => port.cmd,
                    0x20 => port.tfd,
                    0x24 => port.sig,
                    0x28 => port.ssts,
                    0x2c => port.sctl,
                    0x30 => port.serr,
                    0x34 => port.sact,
                    0x38 => port.ci,
                    _ => 0,
                }
            }
            _ => 0,
        };
        u64::from(value)
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 4, "AHCI registers are written as words");
        let value = value as u32;
        let mut s = self.0.lock().unwrap();
        match offset {
            0x04 => {
                if value & 1 != 0 {
                    s.reset();
                } else {
                    s.ghc = value;
                }
            }
            0x08 => s.is &= !value,
            // The firmware lets go at once.
            0x28 => s.bohc = if value & 2 != 0 { 2 } else { value },
            o if o >= 0x100 => {
                let port = &mut s.ports[(o - 0x100) / 0x80];
                match (o - 0x100) % 0x80 {
                    0x00 => port.clb = (port.clb & !0xffff_ffff) | u64::from(value),
                    0x04 => port.clb = (port.clb & 0xffff_ffff) | u64::from(value) << 32,
                    0x08 => port.fb = (port.fb & !0xffff_ffff) | u64::from(value),
                    0x0c => port.fb = (port.fb & 0xffff_ffff) | u64::from(value) << 32,
                    0x10 => port.is &= !value,
                    0x14 => port.ie = value,
                    0x18 => {
                        let mut cmd = value & !((1 << 14) | (1 << 15) | (0x1f << 8));
                        cmd |= port.cmd & (0x1f << 8);
                        if value & 1 != 0 {
                            cmd |= 1 << 15;
                        } else {
                            port.ci = 0;
                            port.sact = 0;
                            port.taken = 0;
                            if let Some(disk) = &mut port.disk {
                                disk.held.clear();
                            }
                        }
                        if value & (1 << 4) != 0 {
                            cmd |= 1 << 14;
                        }
                        if value & (1 << 3) != 0 {
                            port.tfd &= !0x88;
                            cmd &= !(1 << 3);
                        }
                        port.cmd = cmd;
                    }
                    0x2c => {
                        port.sctl = value;
                        if value & 0xf == 1 {
                            port.link_down();
                        } else {
                            port.link_up();
                        }
                    }
                    0x30 => port.serr &= !value,
                    0x34 => port.sact |= value,
                    0x38 => {
                        port.ci |= value;
                        port.issue();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Sim {
    fn new(disks: Vec<(usize, Disk)>) -> Arc<Sim> {
        let mut ports: Vec<PortRegs> = (0..PORTS).map(|_| PortRegs::default()).collect();
        for (index, disk) in disks {
            ports[index].disk = Some(disk);
        }
        let state = State { ghc: 0, is: 0, cap: CAP, cap2: 0, bohc: 0, ports, resets: 0 };
        Arc::new(Sim(Mutex::new(state)))
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }

    fn disk<R>(&self, port: usize, f: impl FnOnce(&mut Disk) -> R) -> R {
        self.with(|s| f(s.ports[port].disk.as_mut().unwrap()))
    }

    /// Finishes the commands port `port`'s disk holds, in the order given
    /// by `order` (slots), or all in the order taken.
    fn finish(&self, port: usize, order: Option<&[usize]>) {
        self.with(|s| {
            let regs = &mut s.ports[port];
            let disk = regs.disk.as_mut().unwrap();
            let slots: Vec<(usize, bool)> = match order {
                Some(order) => order
                    .iter()
                    .map(|slot| {
                        let at = disk.held.iter().position(|(s, _)| s == slot).unwrap();
                        disk.held.remove(at)
                    })
                    .collect(),
                None => std::mem::take(&mut disk.held),
            };
            for (slot, failed) in slots {
                if regs.is & (1 << 30) != 0 {
                    // Stopped: the controller sees nothing more.
                    break;
                }
                if failed {
                    regs.fail(slot);
                } else {
                    let queued = regs.sact & (1 << slot) != 0;
                    regs.finish(slot, queued);
                }
            }
        });
    }
}

fn alloc(pages: usize) -> Option<Dma> {
    Some(Dma::fake(pages * 4096))
}

fn no_delay(_: u32) {}

/// What probing found on each port.
type Found = Vec<(usize, Result<Port, Absent>)>;

/// The controller and the disks it found, by port.
fn probe(sim: &Arc<Sim>) -> (Hba, Found) {
    let regs = Mmio::fake(sim.clone(), ABAR);
    let hba = Hba::new(regs, &mut no_delay).unwrap();
    let mut found = Vec::new();
    let mut ports: Vec<Option<Port>> = (0..32).map(|_| None).collect();
    hba.probe(&mut ports, &mut alloc, &mut no_delay, &mut |index, port| found.push((index, port)));
    assert!(ports.iter().all(Option::is_none));
    (hba, found)
}

fn one_port(sim: &Arc<Sim>) -> (Hba, Port) {
    let (hba, mut found) = probe(sim);
    assert_eq!(found.len(), 1);
    let (_, port) = found.remove(0);
    (hba, port.unwrap())
}

/// A buffer of `pages` pages of DMA memory, as (memory, its buffers).
fn buffers(pages: usize) -> (Dma, Vec<(u64, u32)>) {
    let memory = Dma::fake(pages * 4096);
    let list = (0..pages).map(|i| (memory.phys() + 4096 * i as u64, 4096)).collect();
    (memory, list)
}

fn wait_all(port: &mut Port) -> Vec<(u8, Outcome)> {
    let mut out = Vec::new();
    while let Some(done) = port.complete() {
        out.push(done);
    }
    out
}

#[test]
fn identify_data_say_what_the_disk_is() {
    let id = identify_words(1 << 30, 32, true);
    let identity = ata::identify(&id).unwrap();
    assert_eq!(identity.sectors, 1 << 30);
    assert_eq!(identity.physical_sector, 4096);
    assert_eq!(identity.queue_depth, 32);
    assert!(identity.write_cache && identity.flush && identity.fua);
    assert_eq!(identity.trim_blocks, 1);
    assert_eq!(identity.rotational, Some(false));
    assert_eq!(ata::trimmed(&identity.model), "Simulated SSD");
    assert_eq!(ata::trimmed(&identity.serial), "SIM0001");
    assert_eq!(ata::trimmed(&identity.firmware), "1.0");
}

#[test]
fn disks_with_4k_sectors_count_in_them() {
    // 4,096-byte logical sectors: 2,048 words each, eight to a physical
    // one as the words say, or one when it is too big to take.
    let mut id = identify_words(1 << 20, 32, false);
    id[106] |= 1 << 12;
    id[117] = 2048;
    checksum(&mut id);
    let identity = ata::identify(&id).unwrap();
    assert_eq!((identity.sectors, identity.logical_sector), (1 << 20, 4096));
    assert_eq!(identity.physical_sector, 32_768);
    id[106] = (id[106] & !0xf) | 5;
    checksum(&mut id);
    assert_eq!(ata::identify(&id).unwrap().physical_sector, 4096);
    // A disk that does not say otherwise has 512-byte sectors.
    let identity = ata::identify(&identify_words(64, 32, false)).unwrap();
    assert_eq!(identity.logical_sector, 512);
}

#[test]
fn identify_data_that_do_not_fit_are_refused() {
    let mut id = identify_words(64, 32, false);
    id[3] ^= 1;
    assert_eq!(ata::identify(&id), Err(ata::Unsupported::Checksum));

    let mut id = identify_words(64, 32, false);
    id[86] &= !(1 << 10);
    checksum(&mut id);
    assert_eq!(ata::identify(&id), Err(ata::Unsupported::No48Bit));

    // 1,024-byte logical sectors: 512 words each.
    let mut id = identify_words(64, 32, false);
    id[106] |= 1 << 12;
    id[117] = 512;
    checksum(&mut id);
    assert_eq!(ata::identify(&id), Err(ata::Unsupported::SectorSize(1024)));

    let mut id = identify_words(64, 32, false);
    id[0] = 1 << 15;
    checksum(&mut id);
    assert_eq!(ata::identify(&id), Err(ata::Unsupported::NotAta));

    // No signature: no checksum to check.
    let mut id = identify_words(64, 0, false);
    id[255] = 0;
    id[3] = 7;
    let identity = ata::identify(&id).unwrap();
    assert_eq!(identity.queue_depth, 0);
    assert_eq!(identity.trim_blocks, 0);
}

#[test]
fn register_frames_and_trim_ranges_are_laid_out_as_ata_has_them() {
    let fis = Taskfile::queued(true, true, 0x0123_4567_89ab, 8, 5).fis();
    assert_eq!(fis[0], 0x27);
    assert_eq!(fis[1], 0x80);
    assert_eq!(fis[2], command::WRITE_FPDMA_QUEUED);
    assert_eq!((fis[3], fis[11]), (8, 0));
    assert_eq!(&fis[4..7], &[0xab, 0x89, 0x67]);
    assert_eq!(fis[7], ata::DEVICE_LBA | ata::DEVICE_FUA);
    assert_eq!(&fis[8..11], &[0x45, 0x23, 0x01]);
    assert_eq!((fis[12], fis[13]), (5 << 3, 0));
    let fis = Taskfile::queued(false, true, 0, 1, 0).fis();
    assert_eq!(fis[7], ata::DEVICE_LBA, "a read has no FUA");
    let entry = u64::from_le_bytes(ata::trim_entry(0x1234, 0x20000));
    assert_eq!(entry, 0x1234 | (0xffff << 48));
    let mut page = [0u8; 512];
    (page[0], page[3]) = (7, 0x40);
    assert_eq!(ata::ncq_error(&page), Some((7, 0x40)));
    page[0] = 0x80 | 7;
    assert_eq!(ata::ncq_error(&page), None);
}

#[test]
fn probing_finds_the_disks_and_says_why_a_device_is_not_used() {
    let mut atapi = Disk::new();
    atapi.signature = 0xeb14_0101;
    let sim = Sim::new(std::vec![(0, Disk::new()), (2, atapi)]);
    let (hba, found) = probe(&sim);
    assert_eq!(hba.slots(), 32);
    assert!(hba.ncq() && hba.wide());
    assert_eq!(hba.version(), (1, 0x301));
    assert_eq!(sim.with(|s| s.resets), 1);
    // Empty ports are left out.
    assert_eq!(found.len(), 2);
    let port = found[0].1.as_ref().unwrap();
    assert_eq!(found[0].0, 0);
    let info = port.info();
    assert_eq!(info.sectors, SECTORS);
    assert_eq!(info.block_size, 4096);
    assert_eq!(info.depth, 32);
    assert!(info.queued && info.write_cache && info.fua);
    assert_eq!(info.trim_sectors, 64 * 0xffff);
    assert!(matches!(found[1], (2, Err(Absent::Atapi))));
    // The port is running and takes interrupts.
    sim.with(|s| {
        assert_eq!(s.ports[0].cmd & 0x11, 0x11);
        assert_ne!(s.ports[0].ie, 0);
        assert_eq!(s.ports[0].sctl & 0xf00, 0x300, "no link power saving");
    });
}

#[test]
fn the_firmware_lets_the_controller_go_first() {
    let sim = Sim::new(std::vec![(0, Disk::new())]);
    sim.with(|s| {
        s.cap2 = 1;
        s.bohc = 1;
    });
    let (_, port) = one_port(&sim);
    assert_eq!(port.info().sectors, SECTORS);
    assert_eq!(sim.with(|s| s.bohc), 2);
}

#[test]
fn queued_reads_and_writes_move_the_data() {
    let sim = Sim::new(std::vec![(1, Disk::new())]);
    let (_, mut port) = one_port(&sim);
    let (memory, list) = buffers(2);
    memory.write(0, &[0x5a; 8192]);
    let slot = port.submit(Op::Write, 100, &list).unwrap();
    assert_eq!(wait_all(&mut port), [(slot, Outcome::Done)]);
    sim.disk(1, |d| {
        assert!((100..116).all(|s| d.sector(s).iter().all(|&b| b == 0x5a)));
        assert_eq!(d.commands.last().unwrap().0, command::WRITE_FPDMA_QUEUED);
    });
    let (into, list) = buffers(1);
    let slot = port.submit(Op::Read, 3, &list).unwrap();
    assert_eq!(wait_all(&mut port), [(slot, Outcome::Done)]);
    let mut bytes = std::vec![0u8; 4096];
    into.read(0, &mut bytes);
    let expected: Vec<u8> = (3..11).flat_map(pattern).collect();
    assert_eq!(bytes, expected);
}

#[test]
fn queued_commands_finish_in_any_order() {
    let mut disk = Disk::new();
    disk.hold = true;
    let sim = Sim::new(std::vec![(0, disk)]);
    let (_, mut port) = one_port(&sim);
    let (_memory, list) = buffers(4);
    let slots: Vec<u8> =
        (0..4).map(|i| port.submit(Op::Read, 8 * i as u64, &list[i..=i]).unwrap()).collect();
    assert_eq!(slots, [0, 1, 2, 3]);
    assert_eq!(port.complete(), None);
    sim.finish(0, Some(&[2, 0]));
    assert_eq!(wait_all(&mut port), [(0, Outcome::Done), (2, Outcome::Done)]);
    // A finished slot is used again.
    assert_eq!(port.submit(Op::Read, 64, &list[..1]).unwrap(), 0);
    sim.finish(0, None);
    let mut done = wait_all(&mut port);
    done.sort_by_key(|&(slot, _)| slot);
    assert_eq!(done, [(0, Outcome::Done), (1, Outcome::Done), (3, Outcome::Done)]);
}

#[test]
fn the_queue_holds_as_many_as_the_disk_takes() {
    let mut disk = Disk::new();
    disk.hold = true;
    disk.identify = identify_words(SECTORS, 4, true);
    let sim = Sim::new(std::vec![(0, disk)]);
    let (_, mut port) = one_port(&sim);
    assert_eq!(port.info().depth, 4);
    let (_memory, list) = buffers(1);
    for _ in 0..4 {
        assert!(port.room(Op::Read));
        port.submit(Op::Read, 0, &list).unwrap();
    }
    assert!(!port.room(Op::Read));
    assert_eq!(port.submit(Op::Read, 0, &list), Err(Error::Full));
}

#[test]
fn flushes_and_trims_go_out_alone() {
    let mut disk = Disk::new();
    disk.hold = true;
    let sim = Sim::new(std::vec![(0, disk)]);
    let (_, mut port) = one_port(&sim);
    let (_memory, list) = buffers(1);
    port.submit(Op::Read, 0, &list).unwrap();
    assert!(!port.room(Op::Flush));
    assert_eq!(port.submit(Op::Flush, 0, &[]), Err(Error::Full));
    sim.finish(0, None);
    wait_all(&mut port);
    let flush = port.submit(Op::Flush, 0, &[]).unwrap();
    // Nothing else while it is out.
    assert!(!port.room(Op::Read));
    assert!(!port.room(Op::Trim(8)));
    sim.finish(0, None);
    assert_eq!(wait_all(&mut port), [(flush, Outcome::Done)]);
    assert!(port.room(Op::Read));
    let trim = port.submit(Op::Trim(0x1_0010), 16, &[]).unwrap();
    sim.finish(0, None);
    assert_eq!(wait_all(&mut port), [(trim, Outcome::Done)]);
    sim.disk(0, |d| {
        assert_eq!(d.flushes, 1);
        assert_eq!(d.trims, [(16, 0xffff), (16 + 0xffff, 0x11)]);
    });
}

#[test]
fn writes_through_the_cache_set_fua() {
    let sim = Sim::new(std::vec![(0, Disk::new())]);
    let (_, mut port) = one_port(&sim);
    let (_memory, list) = buffers(1);
    port.submit(Op::WriteThrough, 8, &list).unwrap();
    wait_all(&mut port);
    port.submit(Op::Write, 8, &list).unwrap();
    wait_all(&mut port);
    sim.disk(0, |d| {
        let n = d.commands.len();
        assert_eq!(d.commands[n - 2].1, ata::DEVICE_LBA | ata::DEVICE_FUA);
        assert_eq!(d.commands[n - 1].1, ata::DEVICE_LBA);
    });
}

#[test]
fn a_disk_that_does_not_queue_takes_one_command_at_a_time() {
    let mut disk = Disk::new();
    disk.identify = identify_words(SECTORS, 0, false);
    disk.hold = true;
    let sim = Sim::new(std::vec![(0, disk)]);
    let (_, mut port) = one_port(&sim);
    let info = port.info();
    assert!(!info.queued);
    assert_eq!(info.depth, 1);
    let (_memory, list) = buffers(1);
    port.submit(Op::WriteThrough, 0, &list).unwrap();
    assert!(!port.room(Op::Read));
    sim.finish(0, None);
    assert_eq!(wait_all(&mut port), [(0, Outcome::Done)]);
    port.submit(Op::Read, 0, &list).unwrap();
    sim.finish(0, None);
    wait_all(&mut port);
    sim.disk(0, |d| {
        let n = d.commands.len();
        assert_eq!(d.commands[n - 2].0, command::WRITE_DMA_FUA_EXT);
        assert_eq!(d.commands[n - 1].0, command::READ_DMA_EXT);
    });
    assert_eq!(port.submit(Op::Trim(8), 0, &[]), Err(Error::BadRequest), "no TRIM");
}

#[test]
fn malformed_requests_are_refused() {
    let sim = Sim::new(std::vec![(0, Disk::new())]);
    let (_, mut port) = one_port(&sim);
    let (memory, list) = buffers(1);
    let phys = memory.phys();
    assert_eq!(port.submit(Op::Read, 0, &[]), Err(Error::BadRequest));
    assert_eq!(port.submit(Op::Read, 0, &[(phys, 100)]), Err(Error::BadRequest));
    assert_eq!(port.submit(Op::Read, 0, &[(phys + 1, 512)]), Err(Error::BadRequest));
    assert_eq!(port.submit(Op::Read, SECTORS - 4, &list), Err(Error::BadRequest));
    assert_eq!(port.submit(Op::Read, u64::MAX, &list), Err(Error::BadRequest));
    let many = std::vec![(phys, 512); crate::port::MAX_SEGMENTS + 1];
    assert_eq!(port.submit(Op::Read, 0, &many), Err(Error::BadRequest));
    assert_eq!(port.submit(Op::Flush, 0, &list), Err(Error::BadRequest));
    assert_eq!(port.submit(Op::Trim(0), 0, &[]), Err(Error::BadRequest));
    assert_eq!(port.submit(Op::Trim(9), SECTORS - 8, &[]), Err(Error::BadRequest));
    assert_eq!(port.complete(), None);
    // The last sectors are fine.
    port.submit(Op::Read, SECTORS - 8, &list).unwrap();
    assert_eq!(wait_all(&mut port), [(0, Outcome::Done)]);
}

#[test]
fn a_failed_command_alone_fails_and_the_port_goes_on() {
    let mut disk = Disk::new();
    disk.bad.push(40);
    let sim = Sim::new(std::vec![(0, disk)]);
    let (_, mut port) = one_port(&sim);
    let (_memory, list) = buffers(1);
    let (_list2_memory, list2) = buffers(1);
    // A queued read of the bad sector, alone in flight.
    port.submit(Op::Read, 40, &list).unwrap();
    assert_eq!(port.complete(), None);
    assert!(port.stopped());
    // Its interrupts are off until it recovers.
    assert_eq!(sim.with(|s| s.ports[0].ie), 0);
    assert_eq!(port.submit(Op::Read, 0, &list2), Err(Error::Stopped));
    port.recover(&mut no_delay);
    assert!(!port.stopped());
    assert_ne!(sim.with(|s| s.ports[0].ie), 0);
    assert_eq!(wait_all(&mut port), [(0, Outcome::Failed)]);
    assert_eq!(port.errors().0, 1);
    port.submit(Op::Read, 0, &list2).unwrap();
    assert_eq!(wait_all(&mut port), [(0, Outcome::Done)]);
}

#[test]
fn a_queued_error_fails_its_command_and_the_others_go_again() {
    let mut disk = Disk::new();
    disk.hold = true;
    disk.bad.push(17);
    let sim = Sim::new(std::vec![(0, disk)]);
    let (_, mut port) = one_port(&sim);
    let (_memory, list) = buffers(3);
    port.submit(Op::Read, 0, &list[0..1]).unwrap();
    port.submit(Op::Read, 16, &list[1..2]).unwrap();
    port.submit(Op::Read, 64, &list[2..3]).unwrap();
    // The bad read fails first; the others are aborted with it.
    sim.finish(0, Some(&[1, 0, 2]));
    assert_eq!(port.complete(), None);
    assert!(port.stopped());
    port.recover(&mut no_delay);
    let mut done = wait_all(&mut port);
    done.sort_by_key(|&(slot, _)| slot);
    assert_eq!(done, [(0, Outcome::Retry), (1, Outcome::Failed), (2, Outcome::Retry)]);
    // The disk takes commands again: its log was read.
    sim.disk(0, |d| {
        assert!(d.ncq_error.is_none());
        assert_eq!(d.commands.last().unwrap().0, command::READ_LOG_EXT);
        d.hold = false;
    });
    port.submit(Op::Read, 0, &list[0..1]).unwrap();
    assert_eq!(wait_all(&mut port), [(0, Outcome::Done)]);
}

#[test]
fn a_command_the_disk_aborted_goes_again() {
    for queued in [true, false] {
        let mut disk = Disk::new();
        disk.bad.push(17);
        // ABRT: the disk gave up on the command, but its sectors are not
        // known to be bad.
        disk.error = 0x04;
        if !queued {
            disk.identify = identify_words(SECTORS, 0, true);
        }
        let sim = Sim::new(std::vec![(0, disk)]);
        let (_, mut port) = one_port(&sim);
        let (_memory, list) = buffers(1);
        port.submit(Op::Read, 16, &list).unwrap();
        assert_eq!(port.complete(), None);
        assert!(port.stopped());
        port.recover(&mut no_delay);
        assert_eq!(wait_all(&mut port), [(0, Outcome::Retry)], "queued: {queued}");
        sim.disk(0, |d| assert!(d.ncq_error.is_none()));
    }
}

#[test]
fn without_the_error_log_the_link_is_reset_and_every_command_goes_again() {
    let mut disk = Disk::new();
    disk.hold = true;
    disk.bad.push(17);
    disk.log_works = false;
    let sim = Sim::new(std::vec![(0, disk)]);
    let (_, mut port) = one_port(&sim);
    let (_memory, list) = buffers(2);
    port.submit(Op::Read, 0, &list[0..1]).unwrap();
    port.submit(Op::Read, 16, &list[1..2]).unwrap();
    sim.finish(0, Some(&[1]));
    assert_eq!(port.complete(), None);
    // The log read fails; the link reset clears the disk's error.
    port.recover(&mut no_delay);
    sim.disk(0, |d| assert!(d.ncq_error.is_none()));
    let mut done = wait_all(&mut port);
    done.sort_by_key(|&(slot, _)| slot);
    assert_eq!(done, [(0, Outcome::Retry), (1, Outcome::Retry)]);
}

#[test]
fn a_reset_retries_what_was_in_flight_or_fails_it_when_the_disk_is_gone() {
    let mut disk = Disk::new();
    disk.hold = true;
    let sim = Sim::new(std::vec![(0, disk)]);
    let (_, mut port) = one_port(&sim);
    let (_memory, list) = buffers(1);
    port.submit(Op::Read, 0, &list).unwrap();
    port.reset(&mut no_delay);
    assert_eq!(wait_all(&mut port), [(0, Outcome::Retry)]);
    assert!(!port.dead());
    port.submit(Op::Read, 0, &list).unwrap();
    // A different disk now.
    sim.disk(0, |d| d.identify = identify_words(SECTORS * 2, 32, true));
    port.reset(&mut no_delay);
    assert_eq!(wait_all(&mut port), [(0, Outcome::Failed)]);
    assert!(port.dead());
    assert_eq!(sim.with(|s| s.ports[0].ie), 0);
    assert_eq!(port.submit(Op::Read, 0, &list), Err(Error::Dead));
}

#[test]
fn a_controller_that_reaches_only_4_gib_gets_no_memory_above() {
    let sim = Sim::new(std::vec![(0, Disk::new())]);
    sim.with(|s| s.cap &= !(1 << 31));
    let (hba, found) = probe(&sim);
    assert!(!hba.wide());
    // Host memory, where the tests' DMA memory is, is above 4 GiB.
    assert!(Dma::fake(4096).phys() > 1 << 32);
    // Every wired port, since none can be set up to look.
    assert_eq!(found.len(), PORTS);
    assert!(found.iter().all(|(_, port)| matches!(port, Err(Absent::NoMemory))));
}

#[test]
fn a_dropped_port_stops() {
    let sim = Sim::new(std::vec![(0, Disk::new())]);
    let (_, port) = one_port(&sim);
    drop(port);
    sim.with(|s| {
        assert_eq!(s.ports[0].cmd & ((1 << 0) | (1 << 4) | (1 << 14) | (1 << 15)), 0);
        assert_eq!(s.ports[0].ie, 0);
    });
}

#[test]
fn interrupts_are_raised_for_ports_with_completions() {
    let sim = Sim::new(std::vec![(0, Disk::new()), (3, Disk::new())]);
    let (hba, mut found) = probe(&sim);
    hba.enable_interrupts();
    let (_, port3) = found.pop().unwrap();
    let mut port3 = port3.unwrap();
    let (_memory, list) = buffers(1);
    sim.with(|s| s.is = 0);
    port3.submit(Op::Read, 0, &list).unwrap();
    assert_eq!(hba.pending(), 1 << 3);
    assert_eq!(wait_all(&mut port3), [(0, Outcome::Done)]);
    hba.acknowledge(1 << 3);
    assert_eq!(hba.pending(), 0);
    assert!(sim.with(|s| s.ghc & 2 != 0));
}
