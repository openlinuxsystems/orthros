// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated virtio-blk device: PCI configuration
//! space with the four capabilities, a BAR with the configuration
//! structures, and a disk that serves requests when its doorbell rings.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::pci::{self, Address, Function};
use radix_hw::{Dma, Mmio, Registers, fake_memory, fake_memory_write};

use crate::blk::{Blk, Op, Outcome, SECTOR, feature};
use crate::{Error, Transport, device_type, feature as common_feature, status};

pub(crate) const BAR_SIZE: usize = 0x4000;
const SECTORS: u64 = 64;

/// Configuration space bytes, with BAR 4 a 16 KiB 64-bit memory BAR.
pub(crate) struct Config(Mutex<Vec<u8>>);

impl Registers for Config {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let bytes = self.0.lock().unwrap();
        (0..size).fold(0, |v, i| v | u64::from(bytes[offset + i]) << (8 * i))
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut value = value;
        if size == 4 && offset == 0x20 {
            value = (value & !(BAR_SIZE as u64 - 1) & 0xffff_ffff) | 0x4;
        } else if size == 4 && (0x10..0x28).contains(&offset) && offset != 0x24 {
            value = 0;
        }
        let mut bytes = self.0.lock().unwrap();
        for i in 0..size {
            bytes[offset + i] = (value >> (8 * i)) as u8;
        }
    }
}

pub(crate) fn config_space(subsystem: u16, device: u16) -> Arc<Config> {
    let mut b = std::vec![0u8; pci::CONFIG_SIZE];
    let mut put = |offset: usize, data: &[u8]| b[offset..offset + data.len()].copy_from_slice(data);
    put(0x00, &0x1af4u16.to_le_bytes());
    put(0x02, &device.to_le_bytes());
    put(0x06, &(1u16 << 4).to_le_bytes());
    put(0x0b, &[0x01]);
    put(0x20, &0xfe00_0004u32.to_le_bytes());
    put(0x24, &0u32.to_le_bytes());
    put(0x2e, &subsystem.to_le_bytes());
    put(0x34, &[0x40]);
    let cap = |next: u8, kind: u8, offset: u32, length: u32| {
        let mut c = std::vec![0x09, next, 16, kind, 4, 0, 0, 0];
        c.extend_from_slice(&offset.to_le_bytes());
        c.extend_from_slice(&length.to_le_bytes());
        c
    };
    put(0x40, &cap(0x50, 1, 0x0000, 0x38));
    let mut notify = cap(0x64, 2, 0x3000, 0x1000);
    notify[2] = 20;
    notify.extend_from_slice(&4u32.to_le_bytes());
    put(0x50, &notify);
    put(0x64, &cap(0x74, 3, 0x1000, 1));
    put(0x74, &cap(0x00, 4, 0x2000, 0x100));
    Arc::new(Config(Mutex::new(b)))
}

#[derive(Default)]
struct State {
    device_select: u32,
    driver_select: u32,
    driver_features: u64,
    status: u8,
    queue_size: u16,
    queue_vector: u16,
    desc: u64,
    driver: u64,
    device: u64,
    enabled: bool,
    last_avail: u16,
    used_idx: u16,
    disk: Vec<u8>,
    flushes: u32,
    discarded: u64,
    requests: u64,
    /// Requests whose chain was in a table of its own.
    indirect: u64,
    /// How many data buffers each request had.
    chains: Vec<usize>,
    /// Doorbells rung, and interrupts the device would raise.
    doorbells: u64,
    interrupts: u64,
    /// Its `blk_size`, 512 if 0.
    blk_size: u32,
}

/// The device's BAR: the common configuration, the device configuration
/// and the doorbell, and the disk behind them.
struct Device {
    state: Mutex<State>,
    features: u64,
}

impl Device {
    fn new(features: u64) -> Arc<Device> {
        let state = State {
            disk: (0..SECTORS * SECTOR).map(|i| (i % 251) as u8).collect(),
            ..State::default()
        };
        Arc::new(Device { state: Mutex::new(state), features })
    }

    fn read_mem(phys: u64, len: usize) -> Vec<u8> {
        let mut out = std::vec![0u8; len];
        fake_memory(phys, &mut out);
        out
    }

    fn u16_at(phys: u64) -> u16 {
        u16::from_le_bytes(Device::read_mem(phys, 2).try_into().unwrap())
    }

    /// Serves every request the driver made available.
    fn serve(s: &mut State) {
        let size = u64::from(s.queue_size);
        let avail_idx = Device::u16_at(s.driver + 2);
        while s.last_avail != avail_idx {
            let head = Device::u16_at(s.driver + 4 + 2 * (u64::from(s.last_avail) % size));
            s.last_avail = s.last_avail.wrapping_add(1);
            let mut chain = Vec::new();
            let (mut table, mut entries, mut desc) = (s.desc, size, head);
            loop {
                assert!(u64::from(desc) < entries, "descriptor {desc} is past its table");
                let d = Device::read_mem(table + 16 * u64::from(desc), 16);
                let addr = u64::from_le_bytes(d[0..8].try_into().unwrap());
                let len = u32::from_le_bytes(d[8..12].try_into().unwrap());
                let flags = u16::from_le_bytes(d[12..14].try_into().unwrap());
                let next = u16::from_le_bytes(d[14..16].try_into().unwrap());
                if flags & 4 != 0 {
                    // The chain is in a table of its own (2.7.5.3), which
                    // holds no further tables.
                    assert!(s.driver_features & common_feature::INDIRECT_DESC != 0);
                    assert!(table == s.desc && flags & 1 == 0, "a table in a table or a chain");
                    assert!(len > 0 && len % 16 == 0 && u64::from(len / 16) <= size);
                    (table, entries, desc) = (addr, u64::from(len / 16), 0);
                    s.indirect += 1;
                    continue;
                }
                chain.push((addr, len, flags & 2 != 0));
                if flags & 1 == 0 {
                    break;
                }
                desc = next;
            }
            let header = Device::read_mem(chain[0].0, 16);
            let kind = u32::from_le_bytes(header[0..4].try_into().unwrap());
            let sector = u64::from_le_bytes(header[8..16].try_into().unwrap());
            let (status_addr, _, _) = *chain.last().unwrap();
            let mut offset = (sector * SECTOR) as usize;
            let mut written = 0u32;
            let mut status = 0u8;
            if kind == 11 {
                // A discard's ranges: what they cover reads as zeros.
                assert_eq!(sector, 0, "a discard's header names no sector");
                for &(addr, len, writes) in &chain[1..chain.len() - 1] {
                    let range = Device::read_mem(addr, len as usize);
                    let first = u64::from_le_bytes(range[0..8].try_into().unwrap());
                    let count = u32::from_le_bytes(range[8..12].try_into().unwrap());
                    let flags = u32::from_le_bytes(range[12..16].try_into().unwrap());
                    let (from, to) =
                        ((first * SECTOR) as usize, ((first + u64::from(count)) * SECTOR) as usize);
                    if writes || len != 16 || flags != 0 || to > s.disk.len() {
                        status = 1;
                        break;
                    }
                    s.disk[from..to].fill(0);
                    s.discarded += u64::from(count);
                }
                chain.clear();
                chain.push((0, 0, false));
                chain.push((status_addr, 1, true));
            }
            for &(addr, len, writes) in &chain[1..chain.len() - 1] {
                let len = len as usize;
                if offset + len > s.disk.len() {
                    status = 1;
                    break;
                }
                match kind {
                    0 if writes => {
                        fake_memory_write(addr, &s.disk[offset..offset + len]);
                        written += len as u32;
                    }
                    1 if !writes => {
                        let data = Device::read_mem(addr, len);
                        s.disk[offset..offset + len].copy_from_slice(&data);
                    }
                    _ => status = 1,
                }
                offset += len;
            }
            if kind == 4 {
                s.flushes += 1;
            } else if kind != 0 && kind != 1 && kind != 11 {
                status = 2;
            }
            fake_memory_write(status_addr, &[status]);
            s.chains.push(chain.len() - 2);
            let slot = s.device + 4 + 8 * (u64::from(s.used_idx) % size);
            fake_memory_write(slot, &u32::from(head).to_le_bytes());
            fake_memory_write(slot + 4, &(written + 1).to_le_bytes());
            s.used_idx = s.used_idx.wrapping_add(1);
            fake_memory_write(s.device + 2, &s.used_idx.to_le_bytes());
            s.requests += 1;
            // An interrupt, as QEMU decides: with event indexes, only if
            // this entry is the one the driver asked about.
            let wanted = if s.driver_features & common_feature::EVENT_IDX != 0 {
                let event = Device::u16_at(s.driver + 4 + 2 * size);
                s.used_idx.wrapping_sub(event).wrapping_sub(1) < 1
            } else {
                Device::u16_at(s.driver) & 1 == 0
            };
            s.interrupts += u64::from(wanted);
        }
        // Done with the ring: told of what comes next, with event indexes.
        fake_memory_write(s.device + 4 + 8 * size, &s.last_avail.to_le_bytes());
    }
}

impl Registers for Device {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let s = self.state.lock().unwrap();
        match (offset, size) {
            (0x04, 4) => (self.features >> (32 * s.device_select)) & 0xffff_ffff,
            (0x12, 2) => 1,
            (0x14, 1) => u64::from(s.status),
            (0x15, 1) => 0,
            (0x18, 2) => 256,
            (0x1a, 2) => u64::from(s.queue_vector),
            (0x1e, 2) => 0,
            (0x2000, 4) => SECTORS & 0xffff_ffff,
            (0x2004, 4) => SECTORS >> 32,
            // size_max, if offered.
            (0x2008, 4) => 2048,
            (0x200c, 4) => 8,
            (0x2014, 4) => u64::from(if s.blk_size == 0 { 512 } else { s.blk_size }),
            // Eight logical blocks to a physical one.
            (0x2018, 4) => 3,
            (0x2024, 4) => 32,
            (0x2028, 4) => 1,
            _ => 0,
        }
    }

    fn write(&self, offset: usize, _size: usize, value: u64) {
        let mut s = self.state.lock().unwrap();
        match offset {
            0x00 => s.device_select = value as u32,
            0x08 => s.driver_select = value as u32,
            0x0c => {
                let shift = 32 * s.driver_select;
                s.driver_features =
                    (s.driver_features & !(0xffff_ffff << shift)) | (value << shift);
            }
            0x14 => {
                let value = value as u8;
                // Accepts the features only if they are among those
                // offered.
                if value & status::FEATURES_OK != 0 && s.driver_features & !self.features != 0 {
                    s.status = value & !status::FEATURES_OK;
                } else {
                    s.status = value;
                }
                if value == 0 {
                    s.enabled = false;
                }
            }
            0x18 => s.queue_size = value as u16,
            0x1a => s.queue_vector = value as u16,
            0x1c => s.enabled = value == 1,
            0x20 => s.desc = value,
            0x28 => s.driver = value,
            0x30 => s.device = value,
            0x3000 => {
                assert!(s.enabled && s.status & status::DRIVER_OK != 0, "notified before ready");
                s.doorbells += 1;
                Device::serve(&mut s);
            }
            _ => {}
        }
    }
}

const FEATURES: u64 = common_feature::VERSION_1
    | common_feature::INDIRECT_DESC
    | common_feature::EVENT_IDX
    | feature::SEG_MAX
    | feature::BLK_SIZE
    | feature::FLUSH
    | feature::TOPOLOGY
    | (1 << 34);

/// The most requests the tests have in flight, a table each.
const REQUESTS: usize = 64;

pub(crate) fn function(subsystem: u16, device_id: u16) -> Function {
    let config = Mmio::fake(config_space(subsystem, device_id), pci::CONFIG_SIZE);
    Function::probe(Address { segment: 0, bus: 0, device: 3, function: 0 }, config).unwrap()
}

pub(crate) fn alloc(pages: usize) -> Option<Dma> {
    Some(Dma::fake(pages * 4096))
}

fn setup(features: u64) -> (Arc<Device>, Blk) {
    setup_with(features, 0)
}

/// [`setup`], with a device whose `blk_size` is `blk_size`.
fn setup_with(features: u64, blk_size: u32) -> (Arc<Device>, Blk) {
    let device = Device::new(features);
    device.state.lock().unwrap().blk_size = blk_size;
    let f = function(2, 0x1042);
    let regs = device.clone();
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(regs.clone(), BAR_SIZE))).unwrap();
    let blk = Blk::new(transport, 0, REQUESTS, &mut alloc).unwrap();
    (device, blk)
}

fn wait(blk: &mut Blk, token: u16) -> Outcome {
    let (done, outcome) = blk.complete().expect("the simulated device answers at once");
    assert_eq!(done, token);
    outcome
}

#[test]
fn finds_the_device_type() {
    assert_eq!(device_type(&function(2, 0x1042)), Some(2));
    assert_eq!(device_type(&function(2, 0x1001)), Some(2));
    assert_eq!(device_type(&function(1, 0x1000)), Some(1));
    assert_eq!(device_type(&function(0, 0x2000)), None);
}

#[test]
fn blk_size_is_the_logical_block() {
    // A disk with 4 KiB sectors, eight to a physical block or alone.
    let (_, blk) = setup_with(FEATURES, 4096);
    assert_eq!((blk.info().logical_block, blk.info().block_size), (4096, 32_768));
    let (_, blk) = setup_with(FEATURES & !feature::TOPOLOGY, 4096);
    assert_eq!((blk.info().logical_block, blk.info().block_size), (4096, 4096));
    // One it cannot take is a sector's, as is one it does not say.
    let (_, blk) = setup_with(FEATURES, 1000);
    assert_eq!(blk.info().logical_block, 512);
    let (_, blk) = setup_with(FEATURES & !feature::BLK_SIZE, 4096);
    assert_eq!((blk.info().logical_block, blk.info().block_size), (512, 4096));
}

#[test]
fn sets_up_with_the_features_it_wants() {
    let (device, blk) = setup(FEATURES);
    let info = blk.info();
    assert_eq!(info.sectors, SECTORS);
    assert_eq!((info.logical_block, info.block_size), (512, 4096));
    assert_eq!(info.max_segments, 8);
    assert!(info.flush && !info.read_only);
    let s = device.state.lock().unwrap();
    assert_eq!(
        s.driver_features,
        common_feature::VERSION_1
            | common_feature::INDIRECT_DESC
            | common_feature::EVENT_IDX
            | feature::SEG_MAX
            | feature::BLK_SIZE
            | feature::FLUSH
            | feature::TOPOLOGY,
        "not packed rings (bit 34), which it does not use"
    );
    assert_eq!(
        s.status,
        status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK
    );
    assert_eq!(s.queue_size, 256);
    assert!(s.enabled);
}

#[test]
fn accepts_dma_through_the_iommu_when_offered() {
    let (device, _blk) = setup(FEATURES | common_feature::ACCESS_PLATFORM);
    let s = device.state.lock().unwrap();
    assert_ne!(s.driver_features & common_feature::ACCESS_PLATFORM, 0);
    assert_ne!(s.status & status::DRIVER_OK, 0);
}

#[test]
fn held_requests_wait_for_the_doorbell() {
    let (_device, mut blk) = setup(FEATURES);
    let buffer = Dma::fake(8192);
    let first = blk.submit_held(Op::Read, 0, &[(buffer.phys(), 512)]).unwrap();
    let second = blk.submit_held(Op::Read, 1, &[(buffer.phys() + 512, 512)]).unwrap();
    assert_eq!(blk.complete(), None, "the device was not told yet");
    blk.notify();
    assert_eq!(blk.complete(), Some((first, Outcome::Done)));
    assert_eq!(blk.complete(), Some((second, Outcome::Done)));
    let mut got = [0u8; 1024];
    buffer.read(0, &mut got);
    let expected: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    assert_eq!(&got[..], &expected[..]);
}

#[test]
fn refuses_legacy_devices() {
    let device = Device::new(FEATURES & !common_feature::VERSION_1);
    let f = function(2, 0x1042);
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(device.clone(), BAR_SIZE))).unwrap();
    assert_eq!(
        Blk::new(transport, 0, REQUESTS, &mut alloc).err(),
        Some(Error::MissingFeature("virtio 1.0"))
    );
    assert_eq!(device.state.lock().unwrap().status & status::FAILED, status::FAILED);
}

#[test]
fn reads_and_writes_sectors_zero_copy() {
    let (device, mut blk) = setup(FEATURES);
    let buffer = Dma::fake(8192);
    let token = blk.submit(Op::Read, 2, &[(buffer.phys(), 1024)]).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    let mut got = [0u8; 1024];
    buffer.read(0, &mut got);
    let expected: Vec<u8> = (1024..2048).map(|i| (i % 251) as u8).collect();
    assert_eq!(&got[..], &expected[..]);

    // A write from two buffers, then a read of it back.
    buffer.write(0, &[7u8; 512]);
    buffer.write(4096, &[9u8; 512]);
    let token =
        blk.submit(Op::Write, 10, &[(buffer.phys(), 512), (buffer.phys() + 4096, 512)]).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    {
        let s = device.state.lock().unwrap();
        assert!(s.disk[10 * 512..11 * 512].iter().all(|&b| b == 7));
        assert!(s.disk[11 * 512..12 * 512].iter().all(|&b| b == 9));
    }
    let token = blk.submit(Op::Flush, 0, &[]).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    assert_eq!(device.state.lock().unwrap().flushes, 1);
}

#[test]
fn discards_when_the_device_does() {
    let (_, mut blk) = setup(FEATURES);
    assert_eq!(blk.info().discard_sectors, 0);
    assert_eq!(blk.submit(Op::Discard(1), 0, &[]), Err(Error::BadRequest));

    let (device, mut blk) = setup(FEATURES | feature::DISCARD);
    assert_eq!(blk.info().discard_sectors, 32);
    let token = blk.submit(Op::Discard(3), 5, &[]).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    {
        let s = device.state.lock().unwrap();
        assert_eq!(s.discarded, 3);
        assert!(s.disk[5 * 512..8 * 512].iter().all(|&b| b == 0));
        assert_eq!(s.disk[8 * 512], ((8 * 512) % 251) as u8);
    }
    let p = Dma::fake(4096).phys();
    assert_eq!(blk.submit(Op::Discard(33), 0, &[]), Err(Error::BadRequest), "32 at most");
    assert_eq!(blk.submit(Op::Discard(0), 0, &[]), Err(Error::BadRequest));
    assert_eq!(blk.submit(Op::Discard(2), SECTORS - 1, &[]), Err(Error::BadRequest));
    assert_eq!(blk.submit(Op::Discard(1), 0, &[(p, 512)]), Err(Error::BadRequest));
    // The slot's status byte moved past the ranges: reads still end well.
    let buffer = Dma::fake(4096);
    let token = blk.submit(Op::Read, 0, &[(buffer.phys(), 512)]).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);

    let (_, mut blk) = setup(FEATURES | feature::DISCARD | feature::RO);
    assert_eq!(blk.submit(Op::Discard(1), 0, &[]), Err(Error::BadRequest));
}

#[test]
fn refuses_requests_outside_the_disk_or_limits() {
    let (_, mut blk) = setup(FEATURES);
    let buffer = Dma::fake(4096);
    let p = buffer.phys();
    assert_eq!(blk.submit(Op::Read, SECTORS, &[(p, 512)]), Err(Error::BadRequest));
    assert_eq!(blk.submit(Op::Read, SECTORS - 1, &[(p, 1024)]), Err(Error::BadRequest));
    assert_eq!(blk.submit(Op::Read, u64::MAX, &[(p, 512)]), Err(Error::BadRequest));
    assert_eq!(blk.submit(Op::Read, 0, &[(p, 100)]), Err(Error::BadRequest));
    assert_eq!(blk.submit(Op::Read, 0, &[]), Err(Error::BadRequest));
    assert_eq!(blk.submit(Op::Flush, 0, &[(p, 512)]), Err(Error::BadRequest));
    let nine = [(p, 512u32); 9];
    assert_eq!(blk.submit(Op::Read, 0, &nine), Err(Error::BadRequest), "seg_max is 8");
    assert!(blk.complete().is_none());
}

#[test]
fn read_only_disks_refuse_writes() {
    let (_, mut blk) = setup(FEATURES | feature::RO);
    assert!(blk.info().read_only);
    let buffer = Dma::fake(4096);
    assert_eq!(blk.submit(Op::Write, 0, &[(buffer.phys(), 512)]), Err(Error::BadRequest));
    let token = blk.submit(Op::Read, 0, &[(buffer.phys(), 512)]).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
}

#[test]
fn many_requests_wrap_the_indexes() {
    let (device, mut blk) = setup(FEATURES);
    let buffer = Dma::fake(4096);
    for i in 0..70_000u64 {
        let sector = i % SECTORS;
        let token = blk.submit(Op::Read, sector, &[(buffer.phys(), 512)]).unwrap();
        assert_eq!(wait(&mut blk, token), Outcome::Done);
        if i % 9_999 == 0 {
            let mut first = [0u8; 1];
            buffer.read(0, &mut first);
            assert_eq!(first[0], ((sector * SECTOR) % 251) as u8);
        }
    }
    assert_eq!(device.state.lock().unwrap().requests, 70_000);
    assert_eq!(blk.room(1), REQUESTS);
}

#[test]
fn the_queue_fills_and_empties() {
    // A device that ignores its doorbell falls behind: requests pile up
    // until the queue is full, then complete in order when it catches up.
    let paused = Arc::new(Paused(Device::new(FEATURES)));
    let f = function(2, 0x1042);
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(paused.clone(), BAR_SIZE))).unwrap();
    let mut blk = Blk::new(transport, 0, REQUESTS, &mut alloc).unwrap();
    let buffer = Dma::fake(4096);
    let mut tokens = Vec::new();
    loop {
        match blk.submit(Op::Read, 0, &[(buffer.phys(), 512)]) {
            Ok(token) => tokens.push(token),
            Err(Error::Full) => break,
            Err(err) => panic!("{err}"),
        }
    }
    assert_eq!(tokens.len(), REQUESTS, "a table each");
    assert!(blk.complete().is_none());
    Device::serve(&mut paused.0.state.lock().unwrap());
    for token in tokens {
        assert_eq!(wait(&mut blk, token), Outcome::Done);
    }
    assert!(blk.complete().is_none());
    assert_eq!(blk.room(1), REQUESTS);
}

/// A device whose doorbell does nothing until the test serves it.
struct Paused(Arc<Device>);

impl Registers for Paused {
    fn read(&self, offset: usize, size: usize) -> u64 {
        self.0.read(offset, size)
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        if offset == 0x3000 {
            self.0.state.lock().unwrap().doorbells += 1;
        } else {
            self.0.write(offset, size, value);
        }
    }
}

/// Puts an entry naming `head` on the used ring, as a device that
/// misbehaves would.
fn used_entry(device: &Device, head: u32) {
    let mut s = device.state.lock().unwrap();
    let slot = s.device + 4 + 8 * (u64::from(s.used_idx) % u64::from(s.queue_size));
    fake_memory_write(slot, &head.to_le_bytes());
    fake_memory_write(slot + 4, &0u32.to_le_bytes());
    s.used_idx = s.used_idx.wrapping_add(1);
    fake_memory_write(s.device + 2, &s.used_idx.to_le_bytes());
}

#[test]
fn used_entries_naming_no_request_are_skipped() {
    // A descriptor out of range, one inside a chain but not its head, and
    // a chain given back twice: the driver frees nothing it did not hand
    // over, and goes on with the requests it did. The chains are in the
    // ring, so that they have descriptors other than their heads.
    let paused = Arc::new(Paused(Device::new(FEATURES & !common_feature::INDIRECT_DESC)));
    let f = function(2, 0x1042);
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(paused.clone(), BAR_SIZE))).unwrap();
    let mut blk = Blk::new(transport, 0, REQUESTS, &mut alloc).unwrap();
    let buffer = Dma::fake(4096);
    let first = blk.submit(Op::Read, 0, &[(buffer.phys(), 512)]).unwrap();
    let second = blk.submit(Op::Read, 1, &[(buffer.phys(), 512)]).unwrap();
    used_entry(&paused.0, 9999);
    used_entry(&paused.0, u32::from(first) + 1);
    Device::serve(&mut paused.0.state.lock().unwrap());
    assert_eq!(wait(&mut blk, first), Outcome::Done);
    used_entry(&paused.0, u32::from(first));
    assert_eq!(wait(&mut blk, second), Outcome::Done);
    assert!(blk.complete().is_none());
    assert_eq!(blk.bad_entries(), 3);
    assert_eq!(blk.room(1), 256 / 3);
}

/// Bytes `from..from + len` of the simulated disk as it starts.
fn disk_bytes(from: u64, len: u64) -> Vec<u8> {
    (from..from + len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn requests_go_in_tables_of_their_own() {
    // 64 reads of eight buffers each, at one doorbell: in the ring they
    // would take ten descriptors each, and 25 at most would fit. Their
    // completions raise one interrupt.
    let (device, mut blk) = setup(FEATURES);
    assert_eq!(blk.room(8), REQUESTS);
    let buffers: Vec<Dma> = (0..64).map(|_| Dma::fake(4096)).collect();
    let mut tokens = Vec::new();
    for (i, buffer) in buffers.iter().enumerate() {
        let parts: Vec<(u64, u32)> = (0..8).map(|k| (buffer.phys() + 512 * k, 512)).collect();
        tokens.push(blk.submit_held(Op::Read, (i as u64 * 8) % SECTORS, &parts).unwrap());
    }
    assert_eq!(blk.room(8), 0, "every table is taken");
    assert_eq!(blk.submit(Op::Read, 0, &[(buffers[0].phys(), 512)]), Err(Error::Full));
    blk.notify();
    for token in tokens {
        assert_eq!(wait(&mut blk, token), Outcome::Done);
    }
    // As the interrupt handler does: on until there is none, which asks
    // for an interrupt at the next.
    assert_eq!(blk.complete(), None);
    for (i, buffer) in buffers.iter().enumerate() {
        let mut got = [0u8; 4096];
        buffer.read(0, &mut got);
        assert_eq!(&got[..], &disk_bytes(((i as u64 * 8) % SECTORS) * SECTOR, 4096)[..]);
    }
    let s = device.state.lock().unwrap();
    assert_eq!((s.requests, s.indirect, s.doorbells, s.interrupts), (64, 64, 1, 1));
    drop(s);
    assert_eq!(blk.room(8), REQUESTS);

    // A write from tables too, and a flush, whose table has no buffer.
    let buffer = &buffers[0];
    buffer.write(0, &[3u8; 4096]);
    let parts: Vec<(u64, u32)> = (0..8).map(|k| (buffer.phys() + 512 * k, 512)).collect();
    let token = blk.submit(Op::Write, 40, &parts).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    let token = blk.submit(Op::Flush, 0, &[]).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    let s = device.state.lock().unwrap();
    assert!(s.disk[40 * 512..48 * 512].iter().all(|&b| b == 3));
    assert_eq!((s.indirect, s.flushes), (66, 1));
    assert_eq!((s.doorbells, s.interrupts), (3, 2), "none for the flush: not asked for yet");
    drop(s);
    assert_eq!(blk.complete(), None);
    let token = blk.submit(Op::Flush, 0, &[]).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    assert_eq!(device.state.lock().unwrap().interrupts, 3);
}

#[test]
fn a_table_takes_up_to_254_buffers() {
    // Without seg_max the device takes any number: a table's worth, or
    // what fits in the ring several times without tables.
    let (device, mut blk) = setup(FEATURES & !feature::SEG_MAX);
    assert_eq!(blk.info().max_segments, 254);
    // Two half sectors and quarter sectors, so that 254 of them fill the
    // disk, each with a gap after it so that none shares a descriptor.
    let len = SECTORS * SECTOR;
    let buffer = Dma::fake(2 * len as usize);
    let mut at = buffer.phys();
    let parts: Vec<(u64, u32)> = (0..254)
        .map(|k| {
            let part = (at, if k < 2 { 256 } else { 128 });
            at += 2 * u64::from(part.1);
            part
        })
        .collect();
    let token = blk.submit(Op::Read, 0, &parts).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    let mut got = Vec::new();
    for &(phys, part) in &parts {
        let mut bytes = std::vec![0u8; part as usize];
        buffer.read((phys - buffer.phys()) as usize, &mut bytes);
        got.extend(bytes);
    }
    assert_eq!(got, disk_bytes(0, len));
    let s = device.state.lock().unwrap();
    assert_eq!((s.indirect, &s.chains[..]), (1, &[254][..]));
    drop(s);
    let more = [(buffer.phys(), 512u32); 255];
    assert_eq!(blk.submit(Op::Read, 0, &more), Err(Error::BadRequest));
    let (_, blk) = setup(FEATURES & !(feature::SEG_MAX | common_feature::INDIRECT_DESC));
    assert_eq!(blk.info().max_segments, 30);
}

#[test]
fn buffers_that_follow_each_other_share_a_descriptor() {
    let (device, mut blk) = setup(FEATURES);
    let buffer = Dma::fake(8192);
    let p = buffer.phys();
    // Eight pieces of one buffer, in order, are one descriptor; two runs
    // apart in memory are two.
    let parts: Vec<(u64, u32)> = (0..8).map(|k| (p + 512 * k, 512)).collect();
    let token = blk.submit(Op::Read, 0, &parts).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    let apart = [(p, 1024), (p + 1024, 1024), (p + 4096, 512), (p + 4608, 512)];
    let token = blk.submit(Op::Read, 8, &apart).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    let mut got = [0u8; 3072];
    buffer.read(0, &mut got[..2048]);
    buffer.read(4096, &mut got[2048..]);
    assert_eq!(&got[..], &disk_bytes(8 * SECTOR, 3072)[..]);
    // Nine that follow each other are one buffer, within seg_max's eight.
    let nine: Vec<(u64, u32)> = (0..9).map(|k| (p + 512 * k, 512)).collect();
    let token = blk.submit(Op::Write, 16, &nine).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    assert_eq!(device.state.lock().unwrap().chains, [1, 2, 1]);
    // A device's size_max bounds a descriptor: 2048 bytes here.
    let (device, mut blk) = setup(FEATURES | feature::SIZE_MAX);
    assert_eq!(blk.info().max_segment_size, 2048);
    let token = blk.submit(Op::Read, 0, &parts).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    let mut got = [0u8; 4096];
    buffer.read(0, &mut got);
    assert_eq!(&got[..], &disk_bytes(0, 4096)[..]);
    assert_eq!(device.state.lock().unwrap().chains, [2]);
}

#[test]
fn doorbells_ring_only_when_the_device_asks() {
    let buffer = Dma::fake(4096);
    let read = |blk: &mut Blk, sector| blk.submit(Op::Read, sector, &[(buffer.phys(), 512)]);
    let ring = |device: &Device| device.state.lock().unwrap().doorbells;
    // Without event indexes: not while the device says it needs none, as
    // it does while it works through the ring and looks again after.
    let paused = Arc::new(Paused(Device::new(FEATURES & !common_feature::EVENT_IDX)));
    let f = function(2, 0x1042);
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(paused.clone(), BAR_SIZE))).unwrap();
    let mut blk = Blk::new(transport, 0, REQUESTS, &mut alloc).unwrap();
    let used_flags = paused.0.state.lock().unwrap().device;
    read(&mut blk, 0).unwrap();
    assert_eq!(ring(&paused.0), 1);
    fake_memory_write(used_flags, &1u16.to_le_bytes());
    read(&mut blk, 1).unwrap();
    assert_eq!(ring(&paused.0), 1, "the device needs no doorbell");
    fake_memory_write(used_flags, &0u16.to_le_bytes());
    read(&mut blk, 2).unwrap();
    blk.notify();
    assert_eq!(ring(&paused.0), 2, "none for nothing new");
    Device::serve(&mut paused.0.state.lock().unwrap());
    assert_eq!(paused.0.state.lock().unwrap().interrupts, 3, "one for each, without them");
    for _ in 0..3 {
        assert_eq!(blk.complete().map(|(_, outcome)| outcome), Some(Outcome::Done));
    }

    // With them: only for buffers past the index the device asked about.
    let paused = Arc::new(Paused(Device::new(FEATURES)));
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(paused.clone(), BAR_SIZE))).unwrap();
    let mut blk = Blk::new(transport, 0, REQUESTS, &mut alloc).unwrap();
    let avail_event = {
        let s = paused.0.state.lock().unwrap();
        s.device + 4 + 8 * u64::from(s.queue_size)
    };
    read(&mut blk, 0).unwrap();
    assert_eq!(ring(&paused.0), 1);
    read(&mut blk, 1).unwrap();
    assert_eq!(ring(&paused.0), 1, "the device will look past the first");
    fake_memory_write(avail_event, &2u16.to_le_bytes());
    read(&mut blk, 2).unwrap();
    assert_eq!(ring(&paused.0), 2, "the device asked about the third");
    Device::serve(&mut paused.0.state.lock().unwrap());
    for _ in 0..3 {
        assert_eq!(blk.complete().map(|(_, outcome)| outcome), Some(Outcome::Done));
    }
    assert_eq!(blk.complete(), None);
    let s = paused.0.state.lock().unwrap();
    assert_eq!(s.interrupts, 1);
    assert_eq!(Device::u16_at(s.driver + 4 + 2 * u64::from(s.queue_size)), 3, "asked after all");
}

#[test]
fn chains_go_in_the_ring_without_indirect_descriptors() {
    let (device, mut blk) = setup(FEATURES & !common_feature::INDIRECT_DESC);
    assert_eq!(device.state.lock().unwrap().driver_features & common_feature::INDIRECT_DESC, 0);
    assert_eq!(blk.room(8), 256 / 10);
    let buffer = Dma::fake(4096);
    let parts: Vec<(u64, u32)> = (0..8).map(|k| (buffer.phys() + 512 * k, 512)).collect();
    let token = blk.submit(Op::Read, 8, &parts).unwrap();
    assert_eq!(wait(&mut blk, token), Outcome::Done);
    let mut got = [0u8; 4096];
    buffer.read(0, &mut got);
    assert_eq!(&got[..], &disk_bytes(8 * SECTOR, 4096)[..]);
    let s = device.state.lock().unwrap();
    assert_eq!((s.requests, s.indirect), (1, 0));
}

#[test]
fn a_flood_of_bad_entries_is_read_a_queue_at_a_time() {
    let paused = Arc::new(Paused(Device::new(FEATURES)));
    let f = function(2, 0x1042);
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(paused.clone(), BAR_SIZE))).unwrap();
    let mut blk = Blk::new(transport, 0, REQUESTS, &mut alloc).unwrap();
    for _ in 0..300 {
        used_entry(&paused.0, 7);
    }
    assert!(blk.complete().is_none());
    assert_eq!(blk.bad_entries(), 256);
    assert!(blk.complete().is_none());
    assert_eq!(blk.bad_entries(), 300);
}

#[test]
fn structures_that_start_unaligned_are_skipped() {
    // A device structure at an odd offset would be read with misaligned
    // 32-bit accesses: the driver does without it, so without the device.
    let config = config_space(2, 0x1042);
    config.0.lock().unwrap()[0x74 + 8..0x74 + 12].copy_from_slice(&0x2001u32.to_le_bytes());
    let address = Address { segment: 0, bus: 0, device: 3, function: 0 };
    let f = Function::probe(address, Mmio::fake(config, pci::CONFIG_SIZE)).unwrap();
    let device = Device::new(FEATURES);
    let found = Transport::find(&f, &mut |_, _| Some(Mmio::fake(device.clone(), BAR_SIZE)));
    assert_eq!(found.err(), Some(Error::MissingStructure("device configuration")));
}

#[test]
fn dropping_the_driver_resets_the_device() {
    let (device, blk) = setup(FEATURES);
    drop(blk);
    let s = device.state.lock().unwrap();
    assert_eq!(s.status, 0);
    assert!(!s.enabled);
}
