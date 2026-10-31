// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The EHCI driver against a simulated controller: its registers, root
//! ports that devices are plugged into, and a schedule walker that runs
//! the queue heads and qTDs in memory as the specification says, a whole
//! qTD at a visit, against devices that answer control transfers from
//! their descriptors, stall what a test names, never answer, or send
//! interrupt reports a test queues.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::pci::{Address, Function};
use radix_hw::{Mmio, Registers};
use radix_usb::host::{Data, Endpoint, Error, Hcd, Target, Tt, Waiter};
use radix_usb::{Setup, Speed, TransferType, descriptor};

use super::sim::{self, Answer, Pid, SimDevice, alloc, keyboard, r32, w32, write};

use crate::Handoff;
use crate::ehci::{Ehci, Tables};

const CAPLENGTH: usize = 0x20;
const PORTS: usize = 4;
/// Where the legacy support capability is in configuration space.
const EECP: usize = 0x68;

#[derive(Default)]
struct State {
    usbcmd: u32,
    usbsts: u32,
    frindex: u32,
    periodic: u32,
    async_list: u32,
    configflag: u32,
    portsc: [u32; PORTS],
    devices: [Option<SimDevice>; PORTS],
    /// Times the controller went past each queue head.
    visits: HashMap<u32, u32>,
    companions: u32,
    wide: bool,
}

struct Sim(Mutex<State>);

const RWC: u32 = 0b10_1010;

impl Registers for Sim {
    fn read(&self, offset: usize, _size: usize) -> u64 {
        let s = self.0.lock().unwrap();
        let value = match offset {
            0x00 => CAPLENGTH as u32 | 0x0100 << 16,
            0x04 => PORTS as u32 | 1 << 4 | s.companions << 12,
            0x08 => (EECP as u32) << 8 | u32::from(s.wide),
            o if o >= CAPLENGTH => match o - CAPLENGTH {
                0x00 => s.usbcmd,
                0x04 => s.usbsts,
                0x0c => s.frindex,
                0x14 => s.periodic,
                0x18 => s.async_list,
                0x40 => s.configflag,
                p @ 0x44..0x54 => s.portsc[(p - 0x44) / 4],
                _ => 0,
            },
            _ => 0,
        };
        u64::from(value)
    }

    fn write(&self, offset: usize, _size: usize, value: u64) {
        let value = value as u32;
        let mut s = self.0.lock().unwrap();
        let Some(o) = offset.checked_sub(CAPLENGTH) else { return };
        match o {
            0x00 => {
                if value & 2 != 0 {
                    s.usbcmd = 0;
                    s.usbsts = 1 << 12;
                    s.configflag = 0;
                    s.portsc = [0; PORTS];
                    return;
                }
                s.usbcmd = value;
                let mut sts = s.usbsts & !(1 << 12 | 1 << 14 | 1 << 15);
                if value & 1 == 0 {
                    sts |= 1 << 12;
                } else {
                    if value & 1 << 4 != 0 {
                        sts |= 1 << 14;
                    }
                    if value & 1 << 5 != 0 {
                        sts |= 1 << 15;
                    }
                }
                s.usbsts = sts;
            }
            0x04 => s.usbsts &= !(value & 0x3f),
            0x14 => s.periodic = value,
            0x18 => s.async_list = value,
            0x40 => {
                s.configflag = value;
                for port in 0..PORTS {
                    if s.devices[port].is_some() {
                        let line = match s.devices[port].as_ref().unwrap().speed {
                            Some(Speed::Low) => 0b01 << 10,
                            _ => 0b10 << 10,
                        };
                        s.portsc[port] |= 1 | 2 | line;
                    }
                }
            }
            p @ 0x44..0x54 => {
                let port = (p - 0x44) / 4;
                let old = s.portsc[port];
                let mut new = old & !(value & RWC);
                // Software can only clear PED.
                if value & 4 == 0 {
                    new &= !4;
                }
                new = new & !(1 << 8 | 1 << 12 | 1 << 13) | value & (1 << 8 | 1 << 12 | 1 << 13);
                if old & 1 << 8 != 0 && value & 1 << 8 == 0 {
                    let high = s.devices[port].as_ref().and_then(|d| d.speed) == Some(Speed::High);
                    if new & 1 != 0 && high {
                        new |= 4;
                    }
                    if let Some(d) = &mut s.devices[port] {
                        d.address = 0;
                    }
                }
                if old & 1 << 13 == 0 && new & 1 << 13 != 0 {
                    // Gone to the companion.
                    new &= !(1 | 4);
                    new |= 2;
                }
                s.portsc[port] = new;
            }
            _ => {}
        }
    }
}

impl State {
    fn device(&mut self, address: u8) -> Option<&mut SimDevice> {
        let ports = &self.portsc;
        self.devices
            .iter_mut()
            .enumerate()
            .filter(|(i, _)| ports[*i] & 4 != 0)
            .filter_map(|(_, d)| d.as_mut())
            .find(|d| d.address == address)
    }

    /// One frame: the periodic schedule's frame, then the asynchronous
    /// schedule once round, then the doorbell.
    fn step(&mut self) {
        if self.usbcmd & 1 == 0 {
            return;
        }
        if self.usbcmd & 1 << 4 != 0 {
            let frame = (self.frindex >> 3) & 1023;
            let mut link = r32(self.periodic + 4 * frame);
            for _ in 0..200 {
                if link & 1 != 0 {
                    break;
                }
                let qh = link & !0x1f;
                if r32(qh + 8) & 0xff != 0 {
                    self.run_qh(qh);
                }
                link = r32(qh);
            }
        }
        if self.usbcmd & 1 << 5 != 0 {
            let start = self.async_list;
            let mut qh = start;
            for _ in 0..200 {
                self.run_qh(qh);
                qh = r32(qh) & !0x1f;
                if qh == start {
                    break;
                }
            }
        }
        if self.usbcmd & 1 << 6 != 0 {
            self.usbcmd &= !(1 << 6);
            self.usbsts |= 1 << 5;
        }
        self.frindex = (self.frindex + 8) & 0x3fff;
    }

    fn run_qh(&mut self, qh: u32) {
        *self.visits.entry(qh).or_default() += 1;
        let chars = r32(qh + 4);
        if chars & 1 << 15 != 0 {
            assert_eq!(r32(qh + 0x18) & 0x40, 0x40, "the head's overlay is halted");
            return;
        }
        let mut token = r32(qh + 0x18);
        if token & 0x80 == 0 {
            if token & 0x40 != 0 {
                return;
            }
            let next = r32(qh + 0x10);
            if next & 1 != 0 {
                return;
            }
            // The qTD into the overlay; without DTC the toggle stays.
            let qtd = next & !0x1f;
            w32(qh + 0x0c, qtd);
            for word in 0..8 {
                w32(qh + 0x10 + 4 * word, r32(qtd + 4 * word));
            }
            if chars & 1 << 14 == 0 {
                let toggle = token & 1 << 31;
                let t = r32(qh + 0x18) & !(1 << 31) | toggle;
                w32(qh + 0x18, t);
            }
            token = r32(qh + 0x18);
        }
        let qtd = r32(qh + 0x0c);
        let address = (chars & 0x7f) as u8;
        let endpoint = (chars >> 8) & 0xf;
        let caps = r32(qh + 8);
        let pid = (token >> 8) & 3;
        let bytes = ((token >> 16) & 0x7fff) as usize;
        let buffer = r32(qh + 0x1c);
        let toggle = token >> 31 != 0;
        let Some(device) = self.device(address) else {
            // No answer: three tries, then halted.
            let t = token & !0x80 | 0x40 | 0x08;
            w32(qh + 0x18, t);
            w32(qtd + 8, t);
            return;
        };
        if device.speed != Some(Speed::High) {
            device.hub_ports.push((((caps >> 16) & 0x7f) as u8, ((caps >> 23) & 0x7f) as u8));
        }
        let kind = match pid {
            0 => Pid::Out,
            1 => Pid::In,
            _ => Pid::Setup,
        };
        let answer = sim::answer(device, kind, endpoint, bytes, buffer, toggle);
        let done = match answer {
            Answer::Nak | Answer::Silent => return,
            Answer::Stall => token & !0x80 | 0x40,
            Answer::Data(data) => {
                let moved = if pid == 1 {
                    let n = data.len().min(bytes);
                    write(buffer, &data[..n]);
                    n
                } else {
                    bytes
                };
                let left = (bytes - moved) as u32;
                let mut t = token & !0x80 & !(0x7fff << 16) | left << 16;
                if moved > 0 || pid != 1 || bytes == 0 {
                    t ^= 1 << 31;
                }
                t
            }
        };
        w32(qh + 0x18, done);
        w32(qtd + 8, done);
    }

}

/// The test's clock: each wait runs the controller a frame.
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

/// Configuration space with the legacy support capability, whose
/// firmware lets go when asked unless `stubborn`.
struct Config {
    bytes: Mutex<Vec<u8>>,
    stubborn: bool,
}

impl Registers for Config {
    fn read(&self, offset: usize, size: usize) -> u64 {
        if (0x10..0x28).contains(&offset) {
            return 0;
        }
        let b = self.bytes.lock().unwrap();
        let mut v = 0u64;
        for i in 0..size {
            v |= u64::from(b[offset + i]) << (8 * i);
        }
        v
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut b = self.bytes.lock().unwrap();
        for i in 0..size {
            b[offset + i] = (value >> (8 * i)) as u8;
        }
        if b[EECP + 3] & 1 != 0 && !self.stubborn {
            b[EECP + 2] = 0;
        }
    }
}

fn function(firmware_owns: bool, stubborn: bool) -> (Function, Arc<Config>) {
    let mut bytes = vec![0u8; radix_hw::pci::CONFIG_SIZE];
    bytes[0..4].copy_from_slice(&0x24cd_8086u32.to_le_bytes());
    bytes[0x09..0x0c].copy_from_slice(&[0x20, 0x03, 0x0c]);
    bytes[EECP] = 1;
    bytes[EECP + 2] = u8::from(firmware_owns);
    bytes[EECP + 4] = 0x3f;
    let config = Arc::new(Config { bytes: Mutex::new(bytes), stubborn });
    let mmio = Mmio::fake(config.clone(), radix_hw::pci::CONFIG_SIZE);
    let address = Address { segment: 0, bus: 0, device: 0x1d, function: 7 };
    (Function::probe(address, mmio).unwrap(), config)
}

struct Rig {
    sim: Arc<Sim>,
    clock: Clock,
    ehci: Ehci<Box<Tables>>,
}

fn rig(devices: [Option<SimDevice>; PORTS], companions: u32) -> Rig {
    let state = State { companions, devices, ..State::default() };
    let sim = Arc::new(Sim(Mutex::new(state)));
    let regs = Mmio::fake(sim.clone(), 0x100);
    let (function, _) = function(false, false);
    let mut clock = Clock { sim: sim.clone(), now: 0 };
    let ehci = Ehci::new(&function, regs, &mut clock, &mut alloc, Box::new(Tables::EMPTY)).unwrap();
    Rig { sim, clock, ehci }
}

impl Rig {
    fn device(&self, port: usize) -> SimDevice {
        self.sim.0.lock().unwrap().devices[port].clone().unwrap()
    }
}

#[test]
fn starts_with_every_port_routed_and_powered() {
    let rig = rig([Some(keyboard(Speed::High)), None, None, None], 1);
    let s = rig.sim.0.lock().unwrap();
    assert_eq!(s.configflag, 1);
    assert_eq!(s.usbcmd & 0x31, 0x31, "running, both schedules on");
    assert!(s.portsc.iter().all(|p| p & 1 << 12 != 0), "powered");
    assert_eq!(s.portsc[0] & 3, 3, "the device shows");
    // Every frame leads into the tree, which ends.
    let first = r32(s.periodic);
    assert_eq!(first & 0x1f, 2, "a queue head");
    let mut link = first;
    let mut heads = 0;
    while link & 1 == 0 {
        heads += 1;
        link = r32(link & !0x1f);
    }
    assert_eq!(heads, 6, "a head for each period, 32 ms to 1 ms");
    drop(s);
    assert_eq!(rig.ehci.info().ports, PORTS as u8);
    assert_eq!(rig.ehci.info().handoff, Handoff::Free);
}

#[test]
fn takes_the_controller_from_the_firmware() {
    for (owns, stubborn, expected) in [
        (false, false, Handoff::Free),
        (true, false, Handoff::Given),
        (true, true, Handoff::Taken),
    ] {
        let sim = Arc::new(Sim(Mutex::new(State::default())));
        let regs = Mmio::fake(sim.clone(), 0x100);
        let (function, config) = function(owns, stubborn);
        let mut clock = Clock { sim: sim.clone(), now: 0 };
        let ehci = Ehci::new(&function, regs, &mut clock, &mut alloc, Box::new(Tables::EMPTY)).unwrap();
        assert_eq!(ehci.info().handoff, expected);
        let b = config.bytes.lock().unwrap();
        assert_eq!(b[EECP + 3], 1, "the system's semaphore is set");
        assert_eq!(b[EECP + 2], 0, "the firmware's is clear");
        assert_eq!(b[EECP + 4], 0, "the SMIs are off");
    }
}

#[test]
fn a_high_speed_device_is_reset_and_kept() {
    let mut rig = rig([Some(keyboard(Speed::High)), None, None, None], 1);
    let port = rig.ehci.port(1);
    assert!(port.connected && port.connect_changed && !port.enabled);
    rig.ehci.acknowledge(1);
    assert!(!rig.ehci.port(1).connect_changed);
    assert_eq!(rig.ehci.reset(1, &mut rig.clock), Ok(Some(Speed::High)));
    assert!(rig.ehci.port(1).enabled);
    assert!(rig.clock.now >= 50_000, "the reset lasted 50 ms");
}

#[test]
fn slower_devices_go_to_the_companion() {
    let devices = [Some(keyboard(Speed::Full)), Some(keyboard(Speed::Low)), None, None];
    let mut rig = rig(devices, 1);
    assert_eq!(rig.ehci.reset(1, &mut rig.clock), Ok(None));
    assert!(rig.ehci.port(1).companion);
    let before = rig.clock.now;
    assert_eq!(rig.ehci.reset(2, &mut rig.clock), Ok(None));
    assert_eq!(rig.clock.now, before, "a low-speed device is handed over without a reset");
    assert!(rig.ehci.port(2).companion);
    let devices = [Some(keyboard(Speed::Full)), None, None, None];
    let mut rig = self::rig(devices, 0);
    assert_eq!(rig.ehci.reset(1, &mut rig.clock), Err(Error::NoCompanion));
}

fn addressed(rig: &mut Rig) -> Target {
    rig.ehci.reset(1, &mut rig.clock).unwrap();
    let zero = Target { address: 0, speed: Speed::High, tt: None };
    let mut bytes = [0u8; 18];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 8);
    let came = rig.ehci.control(zero, 64, setup, &mut bytes[..8], 100, &mut rig.clock);
    assert_eq!(came, Ok(8));
    assert_eq!(bytes[7], 64);
    let came = rig.ehci.control(zero, 64, Setup::set_address(5), &mut [], 100, &mut rig.clock);
    assert_eq!(came, Ok(0));
    rig.ehci.forget(0, &mut rig.clock);
    Target { address: 5, ..zero }
}

#[test]
fn control_transfers_reach_the_device_at_its_address() {
    let mut rig = rig([Some(keyboard(Speed::High)), None, None, None], 0);
    let target = addressed(&mut rig);
    assert_eq!(rig.device(0).address, 5);
    let mut bytes = [0u8; 64];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 64);
    assert_eq!(rig.ehci.control(target, 64, setup, &mut bytes, 100, &mut rig.clock), Ok(18));
    assert_eq!(&bytes[8..10], &[0x27, 0x06]);
    let setups = rig.device(0).setups;
    assert_eq!(setups.len(), 3);
    assert_eq!(setups[1].request, 5);
}

#[test]
fn a_stall_is_said_and_the_pipe_goes_on() {
    let mut device = keyboard(Speed::High);
    device.stalls.push(9);
    let mut rig = rig([Some(device), None, None, None], 0);
    let target = addressed(&mut rig);
    let set = Setup::set_configuration(1);
    assert_eq!(rig.ehci.control(target, 64, set, &mut [], 100, &mut rig.clock), Err(Error::Stall));
    let mut bytes = [0u8; 18];
    let get = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(rig.ehci.control(target, 64, get, &mut bytes, 100, &mut rig.clock), Ok(18));
}

#[test]
fn a_silent_device_times_out_and_its_pipe_leaves_the_schedule() {
    let mut rig = rig([Some(keyboard(Speed::High)), None, None, None], 0);
    let target = addressed(&mut rig);
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().silent = true;
    let get = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    let mut bytes = [0u8; 18];
    assert_eq!(
        rig.ehci.control(target, 64, get, &mut bytes, 20, &mut rig.clock),
        Err(Error::TimedOut)
    );
    let s = rig.sim.0.lock().unwrap();
    let head = s.async_list;
    assert_eq!(r32(head) & !0x1f, head, "only the head is left in the ring");
}

#[test]
fn an_unplugged_device_does_not_answer() {
    let mut rig = rig([None, None, None, None], 0);
    let zero = Target { address: 0, speed: Speed::High, tt: None };
    let get = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 8);
    let mut bytes = [0u8; 8];
    assert_eq!(rig.ehci.control(zero, 64, get, &mut bytes, 100, &mut rig.clock), Err(Error::NoAnswer));
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
    let mut rig = rig([Some(keyboard(Speed::High)), None, None, None], 0);
    let target = addressed(&mut rig);
    let pipe = rig.ehci.open(interrupt(target, 4), 8).unwrap();
    rig.ehci.start(pipe, Data::In(8)).unwrap();
    for _ in 0..20 {
        rig.clock.wait(1000);
    }
    assert_eq!(rig.ehci.poll(pipe), None, "the device has nothing to say");
    for report in 0..3u8 {
        rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().reports.push_back(vec![report; 8]);
        let mut result = None;
        for _ in 0..40 {
            rig.clock.wait(1000);
            result = rig.ehci.poll(pipe);
            if result.is_some() {
                break;
            }
        }
        assert_eq!(result, Some(Ok(8)));
        let mut bytes = [0u8; 8];
        rig.ehci.received(pipe, &mut bytes);
        assert_eq!(bytes, [report; 8]);
        rig.ehci.start(pipe, Data::In(8)).unwrap();
    }
    // NAKs leave the toggle; each report flips it.
    let toggles = rig.device(0).toggles;
    assert!(!toggles[0], "DATA0 first");
    assert_eq!(toggles.windows(2).filter(|w| w[0] != w[1]).count(), 2, "DATA0, DATA1, DATA0");
    assert!(!toggles[toggles.len() - 1]);
    // Reset: DATA0 again.
    rig.ehci.cancel(pipe, &mut rig.clock);
    rig.ehci.reset_pipe(pipe);
    rig.ehci.close(pipe, &mut rig.clock);
}

#[test]
fn a_slow_device_behind_a_hub_is_reached_through_its_translator() {
    let mut rig = rig([Some(keyboard(Speed::High)), None, None, None], 0);
    // The high-speed device at address 5 stands for the hub: the slow
    // device behind it answers at address 6.
    let _hub = addressed(&mut rig);
    {
        let mut s = rig.sim.0.lock().unwrap();
        let d = s.devices[0].as_mut().unwrap();
        d.speed = Some(Speed::Low);
        d.address = 6;
    }
    let target = Target { address: 6, speed: Speed::Low, tt: Some(Tt { hub: 5, port: 3 }) };
    let get = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 8);
    let mut bytes = [0u8; 8];
    assert_eq!(rig.ehci.control(target, 8, get, &mut bytes, 100, &mut rig.clock), Ok(8));
    let pipe = rig.ehci.open(interrupt(target, 10), 8).unwrap();
    rig.ehci.start(pipe, Data::In(8)).unwrap();
    rig.sim.0.lock().unwrap().devices[0].as_mut().unwrap().reports.push_back(vec![7; 3]);
    // Polled every 8 ms: within 9 frames.
    let mut result = None;
    for _ in 0..9 {
        rig.clock.wait(1000);
        result = rig.ehci.poll(pipe);
        if result.is_some() {
            break;
        }
    }
    assert_eq!(result, Some(Ok(3)), "a short report");
    assert!(rig.device(0).hub_ports.iter().all(|&hp| hp == (5, 3)));
    // Closed: out of the tree.
    rig.ehci.close(pipe, &mut rig.clock);
    let s = rig.sim.0.lock().unwrap();
    for frame in 0..32 {
        let mut link = r32(s.periodic + 4 * frame);
        while link & 1 == 0 {
            assert_eq!(r32((link & !0x1f) + 8) & 0xff, 0, "only the tree's heads are left");
            link = r32(link & !0x1f);
        }
    }
}

#[test]
fn halting_gives_the_ports_back() {
    let mut rig = rig([None, None, None, None], 0);
    rig.ehci.halt(&mut rig.clock);
    let s = rig.sim.0.lock().unwrap();
    assert_eq!(s.configflag, 0);
    assert_eq!(s.usbcmd & 1, 0);
}
