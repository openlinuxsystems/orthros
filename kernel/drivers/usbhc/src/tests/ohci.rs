// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The OHCI driver against a simulated controller: its registers, a root
//! hub with ports that devices are plugged into, and a frame that runs
//! the interrupt EDs the HCCA names and then the control list, a TD at a
//! time, as the specification says.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Mmio, Registers};
use radix_usb::host::{Data, Endpoint, Error, Hcd, Target, Waiter};
use radix_usb::{Setup, Speed, TransferType, descriptor};

use super::sim::{self, Answer, Pid, SimDevice, alloc, keyboard, r32, w32, write};
use crate::Handoff;
use crate::ohci::{Ohci, Tables};

const PORTS: usize = 3;

#[derive(Default)]
struct State {
    control: u32,
    command: u32,
    interrupt: u32,
    hcca: u32,
    control_head: u32,
    control_current: u32,
    fm_interval: u32,
    fm_number: u32,
    ports: [u32; PORTS],
    devices: [Option<SimDevice>; PORTS],
    /// The firmware routes interrupts to SMIs, and lets go when asked
    /// unless stubborn.
    stubborn: bool,
    /// The low-speed EDs that reached a device.
    low_speed: Vec<u8>,
}

struct Sim(Mutex<State>);

const IR: u32 = 1 << 8;
const OPERATIONAL: u32 = 2 << 6;

impl Registers for Sim {
    fn read(&self, offset: usize, _size: usize) -> u64 {
        let s = self.0.lock().unwrap();
        let value = match offset {
            0x00 => 0x10,
            0x04 => s.control,
            0x08 => s.command,
            0x0c => s.interrupt,
            0x18 => s.hcca,
            0x20 => s.control_head,
            0x24 => s.control_current,
            0x34 => s.fm_interval,
            0x3c => s.fm_number,
            0x48 => PORTS as u32 | 1 << 9 | 1 << 24,
            p @ 0x54..0x60 => s.ports[(p - 0x54) / 4],
            _ => 0,
        };
        u64::from(value)
    }

    fn write(&self, offset: usize, _size: usize, value: u64) {
        let value = value as u32;
        let mut s = self.0.lock().unwrap();
        match offset {
            0x04 => s.control = value,
            0x08 => {
                if value & 1 != 0 {
                    // A software reset: suspended, the defaults back.
                    s.control = 3 << 6 | s.control & IR;
                    s.fm_interval = 11999;
                    s.hcca = 0;
                    s.control_head = 0;
                    return;
                }
                if value & 1 << 3 != 0 && !s.stubborn {
                    s.control &= !IR;
                }
                s.command |= value & 0x6;
            }
            0x0c => s.interrupt &= !value,
            0x18 => s.hcca = value,
            0x20 => s.control_head = value,
            0x24 => s.control_current = value,
            0x34 => s.fm_interval = value,
            p @ 0x54..0x60 => {
                let port = (p - 0x54) / 4;
                let mut status = s.ports[port] & !(value & 0x1f << 16);
                if value & 1 != 0 {
                    status &= !2;
                }
                if value & 1 << 4 != 0 && status & 1 != 0 {
                    status |= 2 | 1 << 20;
                    if let Some(d) = &mut s.devices[port] {
                        d.address = 0;
                    }
                }
                s.ports[port] = status;
            }
            _ => {}
        }
    }
}

impl State {
    fn device(&mut self, address: u8) -> Option<&mut SimDevice> {
        let ports = &self.ports;
        self.devices
            .iter_mut()
            .enumerate()
            .filter(|(i, _)| ports[*i] & 2 != 0)
            .filter_map(|(_, d)| d.as_mut())
            .find(|d| d.address == address)
    }

    /// One frame: the interrupt EDs of this millisecond, then the control
    /// list, while it is said to be filled.
    fn step(&mut self) {
        if self.control & 3 << 6 != OPERATIONAL || self.hcca == 0 {
            return;
        }
        self.fm_number = (self.fm_number + 1) & 0xffff;
        w32(self.hcca + 0x80, self.fm_number);
        if self.control & 1 << 2 != 0 {
            let first = r32(self.hcca + 4 * (self.fm_number & 31));
            self.run_list(first);
        }
        if self.control & 1 << 4 != 0 && self.command & 2 != 0 {
            if !self.run_list(self.control_head) {
                self.command &= !2;
            }
        }
    }

    /// Runs each ED from `ed` on; says whether any had work.
    fn run_list(&mut self, mut ed: u32) -> bool {
        let mut any = false;
        for _ in 0..200 {
            if ed == 0 {
                break;
            }
            any |= self.run_ed(ed);
            ed = r32(ed + 0x0c) & !0xf;
        }
        any
    }

    /// Runs the TDs of `ed` until one waits; says whether it had any.
    fn run_ed(&mut self, ed: u32) -> bool {
        let info = r32(ed);
        if info & 1 << 14 != 0 {
            return false;
        }
        let mut any = false;
        for _ in 0..8 {
            let head = r32(ed + 8);
            let tail = r32(ed + 4) & !0xf;
            if head & 1 != 0 || head & !0xf == tail {
                return any;
            }
            any = true;
            let td = head & !0xf;
            let td_info = r32(td);
            let buffer = r32(td + 4);
            let next = r32(td + 8);
            let end = r32(td + 0x0c);
            let address = (info & 0x7f) as u8;
            let endpoint = (info >> 7) & 0xf;
            let low = info & 1 << 13 != 0;
            let pid = match (td_info >> 19) & 3 {
                0 => Pid::Setup,
                1 => Pid::Out,
                _ => Pid::In,
            };
            let toggle_from_td = td_info & 1 << 25 != 0;
            let toggle = if toggle_from_td { td_info & 1 << 24 != 0 } else { head & 2 != 0 };
            let bytes = if buffer == 0 { 0 } else { (end - buffer + 1) as usize };
            let rounding = td_info & 1 << 18 != 0;
            let answer = match self.device(address) {
                None => Answer::Silent,
                Some(device) => {
                    if low {
                        let address = device.address;
                        self.low_speed.push(address);
                    }
                    let device = self.device(address).unwrap();
                    sim::answer(device, pid, endpoint, bytes, buffer, toggle)
                }
            };
            let (cc, left, flipped) = match answer {
                Answer::Nak => return any,
                Answer::Silent => (5, buffer, toggle),
                Answer::Stall => (4, buffer, toggle),
                Answer::Data(data) => {
                    let moved = if pid == Pid::In {
                        let n = data.len().min(bytes);
                        write(buffer, &data[..n]);
                        n
                    } else {
                        bytes
                    };
                    let left = if moved == bytes { 0 } else { buffer + moved as u32 };
                    let cc = if moved < bytes && !rounding { 9 } else { 0 };
                    (cc, left, !toggle)
                }
            };
            w32(td, td_info & 0x0fff_ffff | cc << 28);
            w32(td + 4, left);
            w32(td + 8, 0);
            let halted = u32::from(cc != 0);
            w32(ed + 8, next & !0xf | u32::from(flipped) << 1 | halted);
            self.interrupt |= 1 << 1;
        }
        any
    }
}

/// The test's clock: each wait of 1 ms or more runs the controller a frame.
struct Clock {
    sim: Arc<Sim>,
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

struct Rig {
    sim: Arc<Sim>,
    clock: Clock,
    ohci: Ohci<Box<Tables>>,
}

fn rig_with(devices: [Option<SimDevice>; PORTS], control: u32, stubborn: bool) -> Rig {
    let mut state = State { devices, control, stubborn, ..State::default() };
    for port in 0..PORTS {
        if state.devices[port].is_some() {
            let low = state.devices[port].as_ref().unwrap().speed == Some(Speed::Low);
            state.ports[port] = 1 | 1 << 8 | u32::from(low) << 9 | 1 << 16;
        }
    }
    let sim = Arc::new(Sim(Mutex::new(state)));
    let regs = Mmio::fake(sim.clone(), 0x100);
    let mut clock = Clock { sim: sim.clone(), now: 0 };
    let ohci = Ohci::new(regs, &mut clock, &mut alloc, Box::new(Tables::EMPTY)).unwrap();
    Rig { sim, clock, ohci }
}

fn rig(devices: [Option<SimDevice>; PORTS]) -> Rig {
    rig_with(devices, 0, false)
}

impl Rig {
    fn device(&self, port: usize) -> SimDevice {
        self.sim.0.lock().unwrap().devices[port].clone().unwrap()
    }
}

#[test]
fn runs_with_its_lists_built() {
    let rig = rig([Some(keyboard(Speed::Full)), None, None]);
    let s = rig.sim.0.lock().unwrap();
    assert_eq!(s.control & 3 << 6, OPERATIONAL);
    assert_eq!(s.control & (1 << 2 | 1 << 4), 1 << 2 | 1 << 4, "the periodic and control lists on");
    assert_eq!(s.fm_interval & 0x3fff, 11999);
    assert_ne!(s.fm_interval >> 16 & 0x7fff, 0, "the largest packet is set");
    // Each of the 32 entries leads through a skipped ED of each period.
    for entry in 0..32 {
        let mut ed = r32(s.hcca + 4 * entry);
        let mut eds = 0;
        while ed != 0 {
            assert_ne!(r32(ed) & 1 << 14, 0, "skipped");
            eds += 1;
            ed = r32(ed + 0x0c) & !0xf;
        }
        assert_eq!(eds, 6, "32 ms down to 1 ms");
    }
    assert_ne!(r32(s.control_head) & 1 << 14, 0, "the control list's head is skipped");
    drop(s);
    assert_eq!(rig.ohci.info().ports, PORTS as u8);
    assert_eq!(rig.ohci.info().revision, 0x10);
    assert_eq!(rig.ohci.info().handoff, Handoff::Free);
}

#[test]
fn takes_the_controller_from_the_firmware() {
    let rig = rig_with([None, None, None], IR | OPERATIONAL, false);
    assert_eq!(rig.ohci.info().handoff, Handoff::Given);
    assert_eq!(rig.sim.0.lock().unwrap().control & IR, 0);
    let rig = rig_with([None, None, None], IR | OPERATIONAL, true);
    assert_eq!(rig.ohci.info().handoff, Handoff::Taken);
    assert_eq!(rig.sim.0.lock().unwrap().control & 3 << 6, OPERATIONAL);
}

#[test]
fn a_reset_port_says_its_speed() {
    let mut rig = rig([Some(keyboard(Speed::Full)), Some(keyboard(Speed::Low)), None]);
    let port = rig.ohci.port(1);
    assert!(port.connected && port.connect_changed && !port.enabled);
    rig.ohci.acknowledge(1);
    assert!(!rig.ohci.port(1).connect_changed);
    assert_eq!(rig.ohci.reset(1, &mut rig.clock), Ok(Some(Speed::Full)));
    assert!(rig.ohci.port(1).enabled);
    assert_eq!(rig.ohci.reset(2, &mut rig.clock), Ok(Some(Speed::Low)));
    assert_eq!(rig.ohci.reset(3, &mut rig.clock), Err(Error::Gone));
    rig.ohci.disable(1);
    assert!(!rig.ohci.port(1).enabled);
}

fn addressed(rig: &mut Rig, port: u8, speed: Speed) -> Target {
    rig.ohci.reset(port, &mut rig.clock).unwrap();
    let zero = Target { address: 0, speed, tt: None };
    let mut bytes = [0u8; 8];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 8);
    assert_eq!(rig.ohci.control(zero, 8, setup, &mut bytes, 100, &mut rig.clock), Ok(8));
    assert_eq!(bytes[7], 8);
    let came = rig.ohci.control(zero, 8, Setup::set_address(5), &mut [], 100, &mut rig.clock);
    assert_eq!(came, Ok(0));
    rig.ohci.forget(0, &mut rig.clock);
    Target { address: 5, ..zero }
}

#[test]
fn control_transfers_reach_the_device_at_its_address() {
    let mut rig = rig([Some(keyboard(Speed::Full)), None, None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    assert_eq!(rig.device(0).address, 5);
    let mut bytes = [0u8; 64];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 64);
    assert_eq!(rig.ohci.control(target, 8, setup, &mut bytes, 100, &mut rig.clock), Ok(18));
    assert_eq!(&bytes[8..10], &[0x27, 0x06]);
    let setups = rig.device(0).setups;
    assert_eq!(setups.len(), 3);
    assert_eq!(setups[1].request, 5);
    assert!(rig.sim.0.lock().unwrap().low_speed.is_empty());
}

#[test]
fn a_low_speed_device_is_reached_at_low_speed() {
    let mut rig = rig([None, Some(keyboard(Speed::Low)), None]);
    let target = addressed(&mut rig, 2, Speed::Low);
    let mut bytes = [0u8; 18];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(rig.ohci.control(target, 8, setup, &mut bytes, 100, &mut rig.clock), Ok(18));
    assert!(!rig.sim.0.lock().unwrap().low_speed.is_empty());
}

#[test]
fn a_stall_is_said_and_the_pipe_goes_on() {
    let mut device = keyboard(Speed::Full);
    device.stalls.push(9);
    let mut rig = rig([Some(device), None, None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    let set = Setup::set_configuration(1);
    assert_eq!(rig.ohci.control(target, 8, set, &mut [], 100, &mut rig.clock), Err(Error::Stall));
    let mut bytes = [0u8; 18];
    let get = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(rig.ohci.control(target, 8, get, &mut bytes, 100, &mut rig.clock), Ok(18));
}

#[test]
fn a_silent_device_does_not_answer_and_the_pipe_goes_on() {
    let mut rig = rig([Some(keyboard(Speed::Full)), None, None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().silent = true;
    let get = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    let mut bytes = [0u8; 18];
    assert_eq!(rig.ohci.control(target, 8, get, &mut bytes, 20, &mut rig.clock), Err(Error::NoAnswer));
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().silent = false;
    assert_eq!(rig.ohci.control(target, 8, get, &mut bytes, 20, &mut rig.clock), Ok(18));
}

#[test]
fn forgetting_a_device_takes_its_ed_off_the_control_list() {
    let mut rig = rig([Some(keyboard(Speed::Full)), None, None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    let mut bytes = [0u8; 18];
    let get = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(rig.ohci.control(target, 8, get, &mut bytes, 100, &mut rig.clock), Ok(18));
    {
        let s = rig.sim.0.lock().unwrap();
        assert_ne!(r32(s.control_head + 0x0c), 0, "the device's ED follows the head");
    }
    rig.ohci.forget(target.address, &mut rig.clock);
    let s = rig.sim.0.lock().unwrap();
    assert_eq!(r32(s.control_head + 0x0c), 0, "only the head is left");
    assert_ne!(s.control & 1 << 4, 0, "the control list is back on");
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

#[test]
fn interrupt_reports_come_with_alternating_toggles() {
    let mut rig = rig([Some(keyboard(Speed::Full)), None, None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    let pipe = rig.ohci.open(interrupt(target, 10), 8).unwrap();
    rig.ohci.start(pipe, Data::In(8)).unwrap();
    for _ in 0..40 {
        rig.clock.wait(1000);
    }
    assert_eq!(rig.ohci.poll(pipe), None, "the device has nothing to say");
    for report in 0..3u8 {
        rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().reports.push_back(vec![report; 8]);
        let mut result = None;
        // Polled every 8 ms: within 9 frames.
        for _ in 0..9 {
            rig.clock.wait(1000);
            result = rig.ohci.poll(pipe);
            if result.is_some() {
                break;
            }
        }
        assert_eq!(result, Some(Ok(8)));
        let mut bytes = [0u8; 8];
        rig.ohci.received(pipe, &mut bytes);
        assert_eq!(bytes, [report; 8]);
        rig.ohci.start(pipe, Data::In(8)).unwrap();
    }
    let toggles = rig.device(0).toggles;
    assert!(!toggles[0], "DATA0 first");
    assert_eq!(toggles.windows(2).filter(|w| w[0] != w[1]).count(), 2, "DATA0, DATA1, DATA0");
    // A short report ends the transfer well.
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().reports.push_back(vec![9; 3]);
    let mut result = None;
    for _ in 0..9 {
        rig.clock.wait(1000);
        result = rig.ohci.poll(pipe);
        if result.is_some() {
            break;
        }
    }
    assert_eq!(result, Some(Ok(3)));
    // Cancelled, reset and closed: out of the tree.
    rig.ohci.start(pipe, Data::In(8)).unwrap();
    rig.ohci.cancel(pipe, &mut rig.clock);
    rig.ohci.reset_pipe(pipe);
    rig.ohci.close(pipe, &mut rig.clock);
    let s = rig.sim.0.lock().unwrap();
    for entry in 0..32 {
        let mut ed = r32(s.hcca + 4 * entry);
        while ed != 0 {
            assert_ne!(r32(ed) & 1 << 14, 0, "only the tree's skipped EDs are left");
            ed = r32(ed + 0x0c) & !0xf;
        }
    }
}

#[test]
fn a_stalled_interrupt_pipe_starts_again_at_data0() {
    let mut rig = rig([Some(keyboard(Speed::Full)), None, None]);
    let target = addressed(&mut rig, 1, Speed::Full);
    let pipe = rig.ohci.open(interrupt(target, 1), 8).unwrap();
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().reports.push_back(vec![1; 8]);
    rig.ohci.start(pipe, Data::In(8)).unwrap();
    rig.clock.wait(1000);
    rig.clock.wait(1000);
    assert_eq!(rig.ohci.poll(pipe), Some(Ok(8)));
    // The device goes silent: the ED halts.
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().silent = true;
    rig.ohci.start(pipe, Data::In(8)).unwrap();
    rig.clock.wait(1000);
    rig.clock.wait(1000);
    assert_eq!(rig.ohci.poll(pipe), Some(Err(Error::NoAnswer)));
    rig.ohci.reset_pipe(pipe);
    {
        let mut s = rig.sim.0.lock().unwrap();
        let d = s.devices[0].as_mut().unwrap();
        d.silent = false;
        d.toggles.clear();
        d.reports.push_back(vec![2; 8]);
    }
    rig.ohci.start(pipe, Data::In(8)).unwrap();
    rig.clock.wait(1000);
    rig.clock.wait(1000);
    assert_eq!(rig.ohci.poll(pipe), Some(Ok(8)));
    assert_eq!(rig.device(0).toggles, [false], "DATA0 again");
}

#[test]
fn halting_puts_it_in_reset() {
    let mut rig = rig([None, None, None]);
    rig.ohci.halt(&mut rig.clock);
    assert_eq!(rig.sim.0.lock().unwrap().control & 3 << 6, 0);
    assert!(rig.ohci.failed());
}
