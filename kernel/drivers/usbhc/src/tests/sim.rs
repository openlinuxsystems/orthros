// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the simulated controllers share: the test's memory, which the
//! drivers' structures reach by 32-bit addresses, and devices that answer
//! the transactions a controller sends them.

use std::collections::{HashMap, VecDeque};
use std::vec::Vec;

use radix_hw::{Dma, fake_memory, fake_memory_write};
use radix_usb::{Setup, Speed, descriptor};

/// A simulated device, at the port it is plugged into.
#[derive(Clone, Default)]
pub struct SimDevice {
    pub speed: Option<Speed>,
    pub address: u8,
    pub descriptors: HashMap<u16, Vec<u8>>,
    /// bRequests it stalls.
    pub stalls: Vec<u8>,
    pub silent: bool,
    /// The data of the control transfer under way, and an address to
    /// take once its status stage is done.
    pub pending: VecDeque<u8>,
    pub new_address: Option<u8>,
    /// Interrupt reports to send, a transfer each.
    pub reports: VecDeque<Vec<u8>>,
    /// The toggles its interrupt packets came with.
    pub toggles: Vec<bool>,
    pub setups: Vec<Setup>,
    /// What each queue head that reached it said of its hub and port.
    pub hub_ports: Vec<(u8, u8)>,
}

thread_local! {
    /// The high half of the host addresses the test's memory is at: the
    /// controller's structures hold the low half, as below 4 GiB.
    static HIGH: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// The host address of the 32-bit address `phys`.
pub fn full(phys: u32) -> u64 {
    HIGH.with(|h| h.get().expect("memory was allocated")) | u64::from(phys)
}

pub fn read(phys: u32, out: &mut [u8]) {
    fake_memory(full(phys), out);
}

pub fn write(phys: u32, data: &[u8]) {
    fake_memory_write(full(phys), data);
}

pub fn r32(phys: u32) -> u32 {
    let mut b = [0u8; 4];
    read(phys, &mut b);
    u32::from_le_bytes(b)
}

pub fn w32(phys: u32, value: u32) {
    write(phys, &value.to_le_bytes());
}

/// What a transaction got from a device.
pub enum Answer {
    Data(Vec<u8>),
    Nak,
    Stall,
    Silent,
}

/// What a transaction is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pid {
    Setup,
    In,
    Out,
}

/// What `device` answers a transaction `pid` to its `endpoint` of up to
/// `bytes`, from memory at `buffer` for a SETUP, with `toggle`.
pub fn answer(
    device: &mut SimDevice,
    pid: Pid,
    endpoint: u32,
    bytes: usize,
    buffer: u32,
    toggle: bool,
) -> Answer {
    if device.silent {
        return Answer::Silent;
    }
    match pid {
        Pid::Setup => {
            let mut bytes = [0u8; 8];
            read(buffer, &mut bytes);
            setup(device, bytes)
        }
        Pid::In if endpoint == 0 => {
            if bytes == 0 {
                if let Some(a) = device.new_address.take() {
                    device.address = a;
                }
                Answer::Data(Vec::new())
            } else {
                let take = bytes.min(device.pending.len());
                Answer::Data(device.pending.drain(..take).collect())
            }
        }
        Pid::In => {
            device.toggles.push(toggle);
            match device.reports.pop_front() {
                Some(report) => Answer::Data(report),
                None => Answer::Nak,
            }
        }
        Pid::Out => {
            // The status stage of a transfer in, or data out.
            if bytes == 0 {
                if let Some(a) = device.new_address.take() {
                    device.address = a;
                }
            }
            Answer::Data(Vec::new())
        }
    }
}

fn setup(device: &mut SimDevice, bytes: [u8; 8]) -> Answer {
    let setup = Setup {
        request_type: bytes[0],
        request: bytes[1],
        value: u16::from_le_bytes([bytes[2], bytes[3]]),
        index: u16::from_le_bytes([bytes[4], bytes[5]]),
        length: u16::from_le_bytes([bytes[6], bytes[7]]),
    };
    device.setups.push(setup);
    if device.stalls.contains(&setup.request) {
        return Answer::Stall;
    }
    match setup.request {
        6 => {
            let Some(d) = device.descriptors.get(&setup.value) else { return Answer::Stall };
            let n = d.len().min(usize::from(setup.length));
            device.pending = d[..n].iter().copied().collect();
        }
        5 => device.new_address = Some(setup.value as u8),
        _ => {}
    }
    Answer::Data(Vec::new())
}

pub fn alloc(pages: usize, below: bool) -> Option<Dma> {
    assert!(below, "the driver's structures are below 4 GiB");
    let dma = Dma::fake(pages * 4096);
    let high = dma.phys() & !0xffff_ffff;
    let end = (dma.phys() + dma.len() as u64 - 1) & !0xffff_ffff;
    HIGH.with(|h| {
        assert_eq!(*h.get().get_or_insert(high), high, "the test's memory is in one 4 GiB");
        h.set(Some(high));
    });
    assert_eq!(end, high, "the test's memory is in one 4 GiB");
    Some(dma)
}

pub fn keyboard(speed: Speed) -> SimDevice {
    let mut device = SimDevice { speed: Some(speed), ..SimDevice::default() };
    let max0 = if speed == Speed::High { 64 } else { 8 };
    let dev = vec![18, 1, 0, 2, 0, 0, 0, max0, 0x27, 0x06, 0x01, 0x00, 0, 0, 1, 2, 0, 1];
    device.descriptors.insert(u16::from(descriptor::DEVICE) << 8, dev);
    device
}
