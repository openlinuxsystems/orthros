// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The UHCI driver against a simulated controller: its I/O registers, two
//! root ports that devices are plugged into, and a frame that follows the
//! frame list's entry through the QHs, and runs each QH's TDs as the
//! design guide says: one at a time, on down while they ask to be run
//! depth first, and stopping on a short packet.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::pci::{Address, Function};
use radix_hw::{Mmio, Registers};
use radix_usb::host::{Data, Endpoint, Error, Hcd, Target, Waiter};
use radix_usb::{Setup, Speed, TransferType, descriptor};

use super::sim::{self, Answer, Pid, SimDevice, alloc, keyboard, r32, w32, write};
use crate::Handoff;
use crate::uhci::{Ports, Tables, Uhci};

const PORTS: usize = 2;
const HALTED: u16 = 1 << 5;

#[derive(Default)]
struct State {
    cmd: u16,
    sts: u16,
    frnum: u16,
    frbase: u32,
    ports: [u16; PORTS],
    devices: [Option<SimDevice>; PORTS],
    /// The addresses of the devices reached at low speed.
    low_speed: Vec<u8>,
}

#[derive(Clone)]
struct Sim(Arc<Mutex<State>>);

impl Ports for Sim {
    fn len(&self) -> u16 {
        0x20
    }

    fn read16(&self, offset: u16) -> u16 {
        let s = self.0.lock().unwrap();
        match offset {
            0x00 => s.cmd,
            0x02 => s.sts,
            0x06 => s.frnum,
            p @ 0x10..0x20 => match s.ports.get(usize::from(p - 0x10) / 2) {
                Some(&status) => status | 0x80,
                None => 0xff7f,
            },
            _ => 0,
        }
    }

    fn write16(&self, offset: u16, value: u16) {
        let mut s = self.0.lock().unwrap();
        match offset {
            0x00 => {
                if value & 2 != 0 {
                    s.cmd = 0;
                    s.sts = HALTED;
                    for port in 0..PORTS {
                        s.ports[port] &= 1 | 1 << 8;
                        if s.ports[port] & 1 != 0 {
                            s.ports[port] |= 2;
                        }
                    }
                    return;
                }
                s.cmd = value;
                if value & 1 != 0 {
                    s.sts &= !HALTED;
                } else {
                    s.sts |= HALTED;
                }
            }
            0x02 => s.sts &= !(value & 0x1f),
            0x06 => s.frnum = value,
            p @ 0x10..0x14 => {
                let port = usize::from(p - 0x10) / 2;
                let old = s.ports[port];
                let mut new = old & !(value & 0b1010);
                new = new & !(1 << 2 | 1 << 9) | value & (1 << 2 | 1 << 9);
                if old & 1 == 0 {
                    new &= !(1 << 2);
                }
                if value & 1 << 9 != 0 {
                    new &= !(1 << 2);
                    if let Some(d) = &mut s.devices[port] {
                        d.address = 0;
                    }
                }
                s.ports[port] = new;
            }
            _ => {}
        }
    }

    fn read32(&self, offset: u16) -> u32 {
        let s = self.0.lock().unwrap();
        if offset == 0x08 { s.frbase } else { 0 }
    }

    fn write32(&self, offset: u16, value: u32) {
        if offset == 0x08 {
            self.0.lock().unwrap().frbase = value;
        }
    }
}

impl State {
    fn device(&mut self, address: u8) -> Option<&mut SimDevice> {
        let ports = &self.ports;
        self.devices
            .iter_mut()
            .enumerate()
            .filter(|(i, _)| ports[*i] & 4 != 0)
            .filter_map(|(_, d)| d.as_mut())
            .find(|d| d.address == address)
    }

    /// One frame: the frame list's entry, followed to its end.
    fn step(&mut self) {
        if self.cmd & 1 == 0 {
            return;
        }
        self.frnum = (self.frnum + 1) & 0x7ff;
        let mut link = r32(self.frbase + 4 * u32::from(self.frnum & 1023));
        for _ in 0..300 {
            if link & 1 != 0 {
                break;
            }
            assert_ne!(link & 2, 0, "the frame list leads to QHs");
            let qh = link & !0xf;
            self.run_qh(qh);
            link = r32(qh);
        }
    }

    fn run_qh(&mut self, qh: u32) {
        for _ in 0..600 {
            let element = r32(qh + 4);
            if element & 1 != 0 {
                return;
            }
            let td = element & !0xf;
            let status = r32(td + 4);
            if status & 1 << 23 == 0 {
                return;
            }
            let link = r32(td);
            let token = r32(td + 8);
            let buffer = r32(td + 12);
            let pid = match token & 0xff {
                0x2d => Pid::Setup,
                0x69 => Pid::In,
                0xe1 => Pid::Out,
                other => panic!("PID {other:#x}"),
            };
            let address = ((token >> 8) & 0x7f) as u8;
            let endpoint = (token >> 15) & 0xf;
            let toggle = token & 1 << 19 != 0;
            let max = ((token >> 21) + 1) as usize & 0x7ff;
            let low = status & 1 << 26 != 0;
            let short_stops = status & 1 << 29 != 0;
            let answer = match self.device(address) {
                None => Answer::Silent,
                Some(device) => {
                    let found = device.address;
                    let answer = sim::answer(device, pid, endpoint, max, buffer, toggle);
                    if low {
                        self.low_speed.push(found);
                    }
                    answer
                }
            };
            let done = status & !(1 << 23) & !0x7ff;
            match answer {
                Answer::Nak => return,
                Answer::Silent => {
                    w32(td + 4, done | 1 << 22 | 1 << 18);
                    return;
                }
                Answer::Stall => {
                    w32(td + 4, done | 1 << 22);
                    return;
                }
                Answer::Data(data) => {
                    let moved = if pid == Pid::In {
                        let n = data.len().min(max);
                        write(buffer, &data[..n]);
                        n
                    } else {
                        max
                    };
                    w32(td + 4, done | (moved as u32).wrapping_sub(1) & 0x7ff);
                    if pid == Pid::In && moved < max && short_stops {
                        return;
                    }
                    w32(qh + 4, link);
                    if link & 4 == 0 {
                        return;
                    }
                }
            }
        }
    }
}

/// The test's clock: each wait runs the controller a frame.
struct Clock {
    sim: Sim,
    now: u64,
}

impl Waiter for Clock {
    fn wait(&mut self, us: u32) {
        self.now += u64::from(us.max(1));
        self.sim.0.lock().unwrap().step();
    }

    fn now_us(&mut self) -> u64 {
        self.now
    }
}

/// Configuration space, with the legacy support register.
struct Config(Mutex<Vec<u8>>);

impl Registers for Config {
    fn read(&self, offset: usize, size: usize) -> u64 {
        if (0x10..0x28).contains(&offset) {
            return 0;
        }
        let b = self.0.lock().unwrap();
        (0..size).fold(0, |v, i| v | u64::from(b[offset + i]) << (8 * i))
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut b = self.0.lock().unwrap();
        for i in 0..size {
            b[offset + i] = (value >> (8 * i)) as u8;
        }
    }
}

fn function(legsup: u16) -> (Function, Arc<Config>) {
    let mut bytes = vec![0u8; radix_hw::pci::CONFIG_SIZE];
    bytes[0..4].copy_from_slice(&0x7020_8086u32.to_le_bytes());
    bytes[0x09..0x0c].copy_from_slice(&[0x00, 0x03, 0x0c]);
    bytes[0xc0..0xc2].copy_from_slice(&legsup.to_le_bytes());
    let config = Arc::new(Config(Mutex::new(bytes)));
    let mmio = Mmio::fake(config.clone(), radix_hw::pci::CONFIG_SIZE);
    let address = Address { segment: 0, bus: 0, device: 1, function: 2 };
    (Function::probe(address, mmio).unwrap(), config)
}

struct Rig {
    sim: Sim,
    clock: Clock,
    uhci: Uhci<Sim, Box<Tables>>,
}

fn rig_with(devices: [Option<SimDevice>; PORTS], legsup: u16) -> (Rig, Arc<Config>) {
    let mut state = State { devices, sts: HALTED, ..State::default() };
    for port in 0..PORTS {
        if let Some(d) = &state.devices[port] {
            state.ports[port] = 1 | 2 | u16::from(d.speed == Some(Speed::Low)) << 8;
        }
    }
    let sim = Sim(Arc::new(Mutex::new(state)));
    let (function, config) = function(legsup);
    let mut clock = Clock { sim: sim.clone(), now: 0 };
    let uhci =
        Uhci::new(&function, sim.clone(), &mut clock, &mut alloc, Box::new(Tables::EMPTY)).unwrap();
    (Rig { sim, clock, uhci }, config)
}

fn rig(devices: [Option<SimDevice>; PORTS]) -> Rig {
    rig_with(devices, 0).0
}

impl Rig {
    fn device(&self, port: usize) -> SimDevice {
        self.sim.0.lock().unwrap().devices[port].clone().unwrap()
    }
}

#[test]
fn runs_with_its_schedule_built() {
    let rig = rig([Some(keyboard(Speed::Full)), None]);
    let s = rig.sim.0.lock().unwrap();
    assert_eq!(s.cmd & 0x41, 0x41, "running and configured");
    assert_eq!(s.sts & HALTED, 0);
    // Every entry leads through an empty QH of each period, then the
    // control QHs' head, and ends.
    for entry in [0u32, 1, 31, 1023] {
        let mut link = r32(s.frbase + 4 * entry);
        let mut qhs = 0;
        while link & 1 == 0 {
            assert_eq!(link & 2, 2);
            assert_ne!(r32((link & !0xf) + 4) & 1, 0, "nothing hung yet");
            qhs += 1;
            link = r32(link & !0xf);
        }
        assert_eq!(qhs, 7, "32 ms down to 1 ms, then the control head");
    }
    drop(s);
    assert_eq!(rig.uhci.info().ports, 2, "the third port's register reads as none");
    assert_eq!(rig.uhci.info().handoff, Handoff::Free);
}

#[test]
fn turns_off_the_firmwares_smis() {
    let (rig, config) = rig_with([None, None], 0x0010);
    assert_eq!(rig.uhci.info().handoff, Handoff::Given);
    let b = config.0.lock().unwrap();
    assert_eq!(u16::from_le_bytes([b[0xc0], b[0xc1]]), 0x8f00);
}

#[test]
fn a_reset_port_is_enabled_and_says_its_speed() {
    let mut rig = rig([Some(keyboard(Speed::Full)), Some(keyboard(Speed::Low))]);
    let port = rig.uhci.port(1);
    assert!(port.connected && port.connect_changed && !port.enabled);
    rig.uhci.acknowledge(1);
    assert!(!rig.uhci.port(1).connect_changed);
    assert_eq!(rig.uhci.reset(1, &mut rig.clock), Ok(Some(Speed::Full)));
    assert!(rig.uhci.port(1).enabled);
    assert!(rig.clock.now >= 50_000, "the reset lasted 50 ms");
    assert_eq!(rig.uhci.reset(2, &mut rig.clock), Ok(Some(Speed::Low)));
    rig.uhci.disable(1);
    assert!(!rig.uhci.port(1).enabled);
}

#[test]
fn an_empty_port_is_not_reset() {
    let mut rig = rig([None, None]);
    assert_eq!(rig.uhci.reset(1, &mut rig.clock), Err(Error::Gone));
}

fn addressed(rig: &mut Rig, port: u8, speed: Speed) -> Target {
    rig.uhci.reset(port, &mut rig.clock).unwrap();
    let zero = Target { address: 0, speed, tt: None };
    let mut bytes = [0u8; 8];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 8);
    assert_eq!(rig.uhci.control(zero, 8, setup, &mut bytes, 100, &mut rig.clock), Ok(8));
    assert_eq!(bytes[7], 8);
    let came = rig.uhci.control(zero, 8, Setup::set_address(5), &mut [], 100, &mut rig.clock);
    assert_eq!(came, Ok(0));
    rig.uhci.forget(0, &mut rig.clock);
    Target { address: 5, ..zero }
}

#[test]
fn control_transfers_reach_the_device_at_its_address() {
    let mut rig = rig([Some(keyboard(Speed::Full)), None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    assert_eq!(rig.device(0).address, 5);
    // 64 asked in packets of 8, 18 come: the third packet is short.
    let mut bytes = [0u8; 64];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 64);
    assert_eq!(rig.uhci.control(target, 8, setup, &mut bytes, 100, &mut rig.clock), Ok(18));
    assert_eq!(&bytes[8..10], &[0x27, 0x06]);
    assert_eq!(bytes[17], 1);
    let setups = rig.device(0).setups;
    assert_eq!(setups.len(), 3);
    assert_eq!(setups[1].request, 5);
    assert!(rig.sim.0.lock().unwrap().low_speed.is_empty());
}

#[test]
fn a_low_speed_device_is_reached_at_low_speed() {
    let mut rig = rig([None, Some(keyboard(Speed::Low))]);
    let target = addressed(&mut rig, 2, Speed::Low);
    let mut bytes = [0u8; 18];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(rig.uhci.control(target, 8, setup, &mut bytes, 100, &mut rig.clock), Ok(18));
    assert!(rig.sim.0.lock().unwrap().low_speed.contains(&5));
}

#[test]
fn a_stall_is_said_and_the_pipe_goes_on() {
    let mut device = keyboard(Speed::Full);
    device.stalls.push(9);
    let mut rig = rig([Some(device), None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    let set = Setup::set_configuration(1);
    assert_eq!(rig.uhci.control(target, 8, set, &mut [], 100, &mut rig.clock), Err(Error::Stall));
    let mut bytes = [0u8; 18];
    let get = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(rig.uhci.control(target, 8, get, &mut bytes, 100, &mut rig.clock), Ok(18));
}

#[test]
fn a_silent_device_does_not_answer_and_the_pipe_goes_on() {
    let mut rig = rig([Some(keyboard(Speed::Full)), None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().silent = true;
    let get = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    let mut bytes = [0u8; 18];
    assert_eq!(rig.uhci.control(target, 8, get, &mut bytes, 20, &mut rig.clock), Err(Error::NoAnswer));
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().silent = false;
    assert_eq!(rig.uhci.control(target, 8, get, &mut bytes, 20, &mut rig.clock), Ok(18));
}

#[test]
fn forgetting_a_device_takes_its_qh_out() {
    let mut rig = rig([Some(keyboard(Speed::Full)), None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    let mut bytes = [0u8; 18];
    let get = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(rig.uhci.control(target, 8, get, &mut bytes, 100, &mut rig.clock), Ok(18));
    let head = {
        let s = rig.sim.0.lock().unwrap();
        // The control head is the last of the tree's chain.
        let mut link = r32(s.frbase);
        for _ in 0..6 {
            link = r32(link & !0xf);
        }
        let head = link & !0xf;
        assert_eq!(r32(head) & 1, 0, "the device's QH follows the head");
        head
    };
    rig.uhci.forget(target.address, &mut rig.clock);
    assert_eq!(r32(head) & 1, 1, "only the head is left");
}

fn interrupt(target: Target, interval: u8) -> Endpoint {
    Endpoint {
        target,
        number: 1,
        into_host: true,
        kind: TransferType::Interrupt,
        max_packet: 8,
        transactions: 1,
        interval,
    }
}

fn poll_for(rig: &mut Rig, pipe: u8, frames: usize) -> Option<Result<usize, Error>> {
    for _ in 0..frames {
        rig.clock.wait(1000);
        if let Some(result) = rig.uhci.poll(pipe) {
            return Some(result);
        }
    }
    None
}

#[test]
fn interrupt_reports_come_with_alternating_toggles() {
    let mut rig = rig([Some(keyboard(Speed::Full)), None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    let pipe = rig.uhci.open(interrupt(target, 10), 8).unwrap();
    rig.uhci.start(pipe, Data::In(8)).unwrap();
    assert_eq!(poll_for(&mut rig, pipe, 40), None, "the device has nothing to say");
    for report in 0..3u8 {
        rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().reports.push_back(vec![report; 8]);
        // Polled every 8 ms: within 9 frames.
        assert_eq!(poll_for(&mut rig, pipe, 9), Some(Ok(8)));
        let mut bytes = [0u8; 8];
        rig.uhci.received(pipe, &mut bytes);
        assert_eq!(bytes, [report; 8]);
        rig.uhci.start(pipe, Data::In(8)).unwrap();
    }
    let toggles = rig.device(0).toggles;
    assert!(!toggles[0], "DATA0 first");
    assert_eq!(toggles.windows(2).filter(|w| w[0] != w[1]).count(), 2, "DATA0, DATA1, DATA0");
    // A short report ends the transfer well.
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().reports.push_back(vec![9; 3]);
    assert_eq!(poll_for(&mut rig, pipe, 9), Some(Ok(3)));
    // Cancelled and closed: out of the tree.
    rig.uhci.start(pipe, Data::In(8)).unwrap();
    rig.uhci.cancel(pipe, &mut rig.clock);
    rig.uhci.close(pipe, &mut rig.clock);
    let s = rig.sim.0.lock().unwrap();
    for entry in 0..32 {
        let mut link = r32(s.frbase + 4 * entry);
        while link & 1 == 0 {
            assert_ne!(r32((link & !0xf) + 4) & 1, 0, "only empty QHs are left");
            link = r32(link & !0xf);
        }
    }
}

#[test]
fn a_report_longer_than_a_packet_takes_two_and_keeps_the_toggle() {
    let mut rig = rig([Some(keyboard(Speed::Full)), None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    let pipe = rig.uhci.open(interrupt(target, 1), 16).unwrap();
    {
        let mut s = rig.sim.0.lock().unwrap();
        let d = s.devices[0].as_mut().unwrap();
        d.reports.push_back(vec![1; 8]);
        d.reports.push_back(vec![2; 8]);
        d.reports.push_back(vec![3; 8]);
    }
    rig.uhci.start(pipe, Data::In(16)).unwrap();
    assert_eq!(poll_for(&mut rig, pipe, 4), Some(Ok(16)));
    let mut bytes = [0u8; 16];
    rig.uhci.received(pipe, &mut bytes);
    assert_eq!(&bytes[..8], &[1; 8]);
    assert_eq!(&bytes[8..], &[2; 8]);
    rig.uhci.start(pipe, Data::In(8)).unwrap();
    assert_eq!(poll_for(&mut rig, pipe, 4), Some(Ok(8)));
    assert_eq!(rig.device(0).toggles, [false, true, false]);
    // Reset: DATA0 again.
    rig.uhci.reset_pipe(pipe);
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().reports.push_back(vec![4; 8]);
    rig.uhci.start(pipe, Data::In(8)).unwrap();
    assert_eq!(poll_for(&mut rig, pipe, 4), Some(Ok(8)));
    assert_eq!(rig.device(0).toggles, [false, true, false, false]);
}

#[test]
fn a_stalled_interrupt_pipe_says_so() {
    let mut rig = rig([Some(keyboard(Speed::Full)), None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    let pipe = rig.uhci.open(interrupt(target, 1), 8).unwrap();
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().silent = true;
    rig.uhci.start(pipe, Data::In(8)).unwrap();
    assert_eq!(poll_for(&mut rig, pipe, 4), Some(Err(Error::NoAnswer)));
    assert_eq!(rig.uhci.start(pipe, Data::In(64)), Err(Error::BadRequest), "too long");
}

#[test]
fn halting_stops_it() {
    let mut rig = rig([None, None]);
    rig.uhci.halt(&mut rig.clock);
    let s = rig.sim.0.lock().unwrap();
    assert_eq!(s.cmd & 1, 0);
    assert_ne!(s.sts & HALTED, 0);
}
