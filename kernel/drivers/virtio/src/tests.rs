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

const BAR_SIZE: usize = 0x4000;
const SECTORS: u64 = 64;

/// Configuration space bytes, with BAR 4 a 16 KiB 64-bit memory BAR.
struct Config(Mutex<Vec<u8>>);

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

fn config_space(subsystem: u16, device: u16) -> Arc<Config> {
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
            let mut desc = head;
            loop {
                let d = Device::read_mem(s.desc + 16 * u64::from(desc), 16);
                let addr = u64::from_le_bytes(d[0..8].try_into().unwrap());
                let len = u32::from_le_bytes(d[8..12].try_into().unwrap());
                let flags = u16::from_le_bytes(d[12..14].try_into().unwrap());
                let next = u16::from_le_bytes(d[14..16].try_into().unwrap());
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
            let slot = s.device + 4 + 8 * (u64::from(s.used_idx) % size);
            fake_memory_write(slot, &u32::from(head).to_le_bytes());
            fake_memory_write(slot + 4, &(written + 1).to_le_bytes());
            s.used_idx = s.used_idx.wrapping_add(1);
            fake_memory_write(s.device + 2, &s.used_idx.to_le_bytes());
            s.requests += 1;
        }
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
            (0x2008, 4) => 0,
            (0x200c, 4) => 8,
            (0x2014, 4) => 4096,
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
                Device::serve(&mut s);
            }
            _ => {}
        }
    }
}

const FEATURES: u64 =
    common_feature::VERSION_1 | feature::SEG_MAX | feature::BLK_SIZE | feature::FLUSH | (1 << 28);

fn function(subsystem: u16, device_id: u16) -> Function {
    let config = Mmio::fake(config_space(subsystem, device_id), pci::CONFIG_SIZE);
    Function::probe(Address { segment: 0, bus: 0, device: 3, function: 0 }, config).unwrap()
}

fn alloc(pages: usize) -> Option<Dma> {
    Some(Dma::fake(pages * 4096))
}

fn setup(features: u64) -> (Arc<Device>, Blk) {
    let device = Device::new(features);
    let f = function(2, 0x1042);
    let regs = device.clone();
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(regs.clone(), BAR_SIZE))).unwrap();
    let blk = Blk::new(transport, 0, &mut alloc).unwrap();
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
fn sets_up_with_the_features_it_wants() {
    let (device, blk) = setup(FEATURES);
    let info = blk.info();
    assert_eq!(info.sectors, SECTORS);
    assert_eq!(info.block_size, 4096);
    assert_eq!(info.max_segments, 8);
    assert!(info.flush && !info.read_only);
    let s = device.state.lock().unwrap();
    assert_eq!(
        s.driver_features,
        common_feature::VERSION_1 | feature::SEG_MAX | feature::BLK_SIZE | feature::FLUSH
    );
    assert_eq!(
        s.status,
        status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK
    );
    assert_eq!(s.queue_size, 256);
    assert!(s.enabled);
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
    assert_eq!(Blk::new(transport, 0, &mut alloc).err(), Some(Error::MissingFeature("virtio 1.0")));
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
    assert_eq!(blk.room(1), 256 / 3);
}

#[test]
fn the_queue_fills_and_empties() {
    // A device that ignores its doorbell falls behind: requests pile up
    // until the queue is full, then complete in order when it catches up.
    let paused = Arc::new(Paused(Device::new(FEATURES)));
    let f = function(2, 0x1042);
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(paused.clone(), BAR_SIZE))).unwrap();
    let mut blk = Blk::new(transport, 0, &mut alloc).unwrap();
    let buffer = Dma::fake(4096);
    let mut tokens = Vec::new();
    loop {
        match blk.submit(Op::Read, 0, &[(buffer.phys(), 512)]) {
            Ok(token) => tokens.push(token),
            Err(Error::Full) => break,
            Err(err) => panic!("{err}"),
        }
    }
    assert_eq!(tokens.len(), 256 / 3);
    assert!(blk.complete().is_none());
    Device::serve(&mut paused.0.state.lock().unwrap());
    for token in tokens {
        assert_eq!(wait(&mut blk, token), Outcome::Done);
    }
    assert!(blk.complete().is_none());
    assert_eq!(blk.room(1), 256 / 3);
}

/// A device whose doorbell does nothing until the test serves it.
struct Paused(Arc<Device>);

impl Registers for Paused {
    fn read(&self, offset: usize, size: usize) -> u64 {
        self.0.read(offset, size)
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        if offset != 0x3000 {
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
    // over, and goes on with the requests it did.
    let paused = Arc::new(Paused(Device::new(FEATURES)));
    let f = function(2, 0x1042);
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(paused.clone(), BAR_SIZE))).unwrap();
    let mut blk = Blk::new(transport, 0, &mut alloc).unwrap();
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

#[test]
fn a_flood_of_bad_entries_is_read_a_queue_at_a_time() {
    let paused = Arc::new(Paused(Device::new(FEATURES)));
    let f = function(2, 0x1042);
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(paused.clone(), BAR_SIZE))).unwrap();
    let mut blk = Blk::new(transport, 0, &mut alloc).unwrap();
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
