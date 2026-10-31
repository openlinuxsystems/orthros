// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated DesignWare controller and the devices
//! on its bus. The bus's time passes only while the driver waits: then
//! the controller takes commands from its transmit FIFO, one a byte's
//! time. As the hardware does without IC_EMPTYFIFO_HOLD_MASTER_EN, it
//! ends a transfer with a stop when the FIFO runs dry before one was
//! asked for, which the tests count as a failure; so is a byte read with
//! the receive FIFO full. Each device has 256 bytes of registers: the
//! first byte written after a start says which, and reads and writes go
//! on from there.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Mmio, Registers};

use super::*;
use crate::designware::{DesignWare, SIZE, Timing, Unusable};
use crate::lpss::{self, NotI2c};

const CMD_READ: u32 = 1 << 8;
const CMD_STOP: u32 = 1 << 9;
const CMD_RESTART: u32 = 1 << 10;
const TX_ABRT: u32 = 1 << 6;
const STOP_DET: u32 = 1 << 9;
const ABRT_7BIT_NACK: u32 = 1 << 0;
const ABRT_10BIT_NACK: u32 = 1 << 1;
const ABRT_DATA_NACK: u32 = 1 << 3;
const ABRT_USER: u32 = 1 << 16;
const VERSION_1_21A: u32 = 0x3132_312a;
const VERSION_1_01A: u32 = 0x3130_312a;
/// The bus's time for a byte, about fast mode's.
const BYTE_US: u64 = 25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Start(u16, bool),
    Restart(bool),
    Stop,
    Abort(u32),
}

struct Device {
    address: u16,
    ten_bit: bool,
    memory: [u8; 256],
    pointer: u8,
    /// Whether the next byte written says which register.
    addressing: bool,
    /// How many data bytes it acknowledges before it stops.
    acknowledges: usize,
}

struct State {
    enabled: bool,
    con: u32,
    tar: u32,
    /// IC_SS_SCL_HCNT to IC_FS_SCL_LCNT.
    counts: [u32; 4],
    sda_hold: u32,
    mask: u32,
    tx: VecDeque<u32>,
    rx: VecDeque<u8>,
    tx_depth: usize,
    rx_depth: usize,
    version: u32,
    comp_type: u32,
    raw: u32,
    abort_source: u32,
    /// The device being talked to and whether it is read.
    active: Option<(usize, bool)>,
    /// Bus time not yet used.
    time: u64,
    /// Whether the bus is stuck: no command is taken.
    stuck: bool,
    devices: Vec<Device>,
    events: Vec<Event>,
    ran_dry: u32,
    overflowed: u32,
    underflowed: u32,
    slept: u64,
    spun: u64,
}

impl State {
    fn advance(&mut self, us: u64) {
        if self.stuck || !self.enabled {
            return;
        }
        self.time += us;
        while self.time >= BYTE_US {
            let Some(command) = self.tx.pop_front() else {
                if self.active.is_some() {
                    self.ran_dry += 1;
                    self.stop();
                }
                self.time = 0;
                return;
            };
            self.time -= BYTE_US;
            self.take(command);
        }
    }

    fn take(&mut self, command: u32) {
        let read = command & CMD_READ != 0;
        let restart = match self.active {
            None => false,
            Some((_, was)) if command & CMD_RESTART != 0 || was != read => true,
            Some(_) => {
                self.byte(command);
                return;
            }
        };
        let ten_bit = self.con & (1 << 4) != 0 || self.tar & (1 << 12) != 0;
        let address = (self.tar & 0x3ff) as u16;
        let Some(index) =
            self.devices.iter().position(|d| d.address == address && d.ten_bit == ten_bit)
        else {
            self.abort(if ten_bit { ABRT_10BIT_NACK } else { ABRT_7BIT_NACK });
            return;
        };
        self.events.push(if restart { Event::Restart(read) } else { Event::Start(address, read) });
        self.devices[index].addressing = !read;
        self.active = Some((index, read));
        self.byte(command);
    }

    fn byte(&mut self, command: u32) {
        let Some((index, read)) = self.active else { unreachable!() };
        let device = &mut self.devices[index];
        if read {
            let byte = device.memory[usize::from(device.pointer)];
            device.pointer = device.pointer.wrapping_add(1);
            if self.rx.len() < self.rx_depth {
                self.rx.push_back(byte);
            } else {
                self.overflowed += 1;
            }
        } else if device.addressing {
            device.pointer = command as u8;
            device.addressing = false;
        } else if device.acknowledges == 0 {
            self.abort(ABRT_DATA_NACK);
            return;
        } else {
            device.acknowledges -= 1;
            device.memory[usize::from(device.pointer)] = command as u8;
            device.pointer = device.pointer.wrapping_add(1);
        }
        if command & CMD_STOP != 0 {
            self.stop();
        }
    }

    fn stop(&mut self) {
        self.events.push(Event::Stop);
        self.active = None;
        self.raw |= STOP_DET;
    }

    /// Ends the transfer: the transmit FIFO is flushed and stays so
    /// until the abort is cleared.
    fn abort(&mut self, source: u32) {
        self.events.push(Event::Abort(source));
        self.abort_source = source;
        self.raw |= TX_ABRT | STOP_DET;
        self.tx.clear();
        self.active = None;
    }
}

#[derive(Clone)]
struct Sim(Arc<Mutex<State>>);

impl Sim {
    fn new(tx_depth: usize, rx_depth: usize) -> Sim {
        Sim(Arc::new(Mutex::new(State {
            enabled: false,
            con: 0x7f,
            tar: 0x55,
            counts: [0; 4],
            sda_hold: 1,
            mask: 0x8ff,
            tx: VecDeque::new(),
            rx: VecDeque::new(),
            tx_depth,
            rx_depth,
            version: VERSION_1_21A,
            comp_type: 0x4457_0140,
            raw: 0,
            abort_source: 0,
            active: None,
            time: 0,
            stuck: false,
            devices: Vec::new(),
            events: Vec::new(),
            ran_dry: 0,
            overflowed: 0,
            underflowed: 0,
            slept: 0,
            spun: 0,
        })))
    }

    fn with(&self, change: impl FnOnce(&mut State)) -> &Sim {
        change(&mut self.0.lock().unwrap());
        self
    }

    fn state<T>(&self, look: impl FnOnce(&State) -> T) -> T {
        look(&self.0.lock().unwrap())
    }

    fn device(&self, address: u16, ten_bit: bool) -> &Sim {
        let memory = core::array::from_fn(pattern);
        self.with(|state| {
            state.devices.push(Device {
                address,
                ten_bit,
                memory,
                pointer: 0,
                addressing: false,
                acknowledges: usize::MAX,
            });
        })
    }

    fn memory(&self, device: usize) -> [u8; 256] {
        self.state(|state| state.devices[device].memory)
    }

    /// The bus's events since the last look.
    fn events(&self) -> Vec<Event> {
        core::mem::take(&mut self.0.lock().unwrap().events)
    }

    /// Checks that no transfer ended early and no byte was lost.
    fn clean(&self) {
        self.state(|state| {
            assert_eq!(
                (state.ran_dry, state.overflowed, state.underflowed),
                (0, 0, 0),
                "the transmit FIFO ran dry, or a receive FIFO overflowed or underflowed"
            );
        });
    }

    fn controller(&self, speed: Speed) -> DesignWare<Clock> {
        let timing = Timing::computed(speed, 120_000_000, Some(230));
        DesignWare::new(
            Mmio::fake(Arc::new(self.clone()), SIZE),
            Clock(self.clone()),
            speed,
            timing,
        )
        .unwrap()
    }
}

/// What a device's register holds before it is written.
fn pattern(register: usize) -> u8 {
    (register as u8).wrapping_mul(7) ^ 0x5a
}

impl Registers for Sim {
    fn read(&self, offset: usize, size: usize) -> u64 {
        assert_eq!(size, 4);
        let mut state = self.0.lock().unwrap();
        let busy = state.active.is_some() || !state.tx.is_empty();
        let value = match offset {
            0x00 => state.con,
            0x04 => state.tar,
            0x10 => state.rx.pop_front().map_or_else(
                || {
                    state.underflowed += 1;
                    0
                },
                u32::from,
            ),
            0x14 | 0x18 | 0x1c | 0x20 => state.counts[(offset - 0x14) / 4],
            0x2c => state.raw & state.mask,
            0x30 => state.mask,
            0x34 => state.raw,
            0x40 => {
                state.raw = 0;
                state.abort_source = 0;
                0
            }
            0x54 => {
                state.raw &= !TX_ABRT;
                state.abort_source = 0;
                0
            }
            0x60 => {
                state.raw &= !STOP_DET;
                0
            }
            0x6c => u32::from(state.enabled),
            0x70 => {
                u32::from(busy)
                    | u32::from(state.tx.len() < state.tx_depth) << 1
                    | u32::from(state.tx.is_empty()) << 2
                    | u32::from(!state.rx.is_empty()) << 3
                    | u32::from(busy) << 5
            }
            0x74 => state.tx.len() as u32,
            0x78 => state.rx.len() as u32,
            0x7c => state.sda_hold,
            0x80 => state.abort_source,
            0x9c => u32::from(state.enabled),
            0xf4 => ((state.tx_depth as u32 - 1) << 16) | ((state.rx_depth as u32 - 1) << 8),
            0xf8 => state.version,
            0xfc => state.comp_type,
            _ => panic!("a read of {offset:#x}"),
        };
        value.into()
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 4);
        let value = value as u32;
        let mut state = self.0.lock().unwrap();
        match offset {
            0x00 | 0x04 | 0x14 | 0x18 | 0x1c | 0x20 | 0x7c => {
                assert!(!state.enabled, "{offset:#x} written while the controller is enabled");
                match offset {
                    0x00 => state.con = value,
                    0x04 => state.tar = value,
                    0x7c => state.sda_hold = value,
                    _ => state.counts[(offset - 0x14) / 4] = value,
                }
            }
            0x10 => {
                assert!(state.enabled, "a command while the controller is disabled");
                if state.raw & TX_ABRT == 0 {
                    assert!(state.tx.len() < state.tx_depth, "the transmit FIFO overflowed");
                    state.tx.push_back(value);
                }
            }
            0x30 => state.mask = value,
            0x6c => {
                if value & 2 != 0 && (state.active.is_some() || !state.tx.is_empty()) {
                    state.abort(ABRT_USER);
                }
                state.enabled = value & 1 != 0;
                if !state.enabled {
                    state.tx.clear();
                    state.rx.clear();
                    state.active = None;
                    state.time = 0;
                }
            }
            _ => panic!("a write of {value:#x} to {offset:#x}"),
        }
    }
}

/// The driver's waits, which let the bus's time pass.
struct Clock(Sim);

impl Platform for Clock {
    fn wait_us(&self, us: u64) {
        self.0.with(|state| {
            state.slept += us;
            state.advance(us);
        });
    }

    fn spin_us(&self, us: u64) {
        self.0.with(|state| {
            state.spun += us;
            state.advance(us);
        });
    }
}

#[test]
fn computed_timing() {
    assert_eq!(
        Timing::computed(Speed::Standard, 120_000_000, Some(230)),
        Timing { high: 513, low: 599, hold: Some(28) }
    );
    assert_eq!(
        Timing::computed(Speed::Fast, 120_000_000, None),
        Timing { high: 105, low: 191, hold: None }
    );
    assert_eq!(
        Timing::computed(Speed::Fast, 150_000_000, Some(300)),
        Timing { high: 132, low: 239, hold: Some(45) }
    );
    assert_eq!(Speed::for_devices(1_000_000), Speed::Fast);
    assert_eq!(Speed::for_devices(400_000), Speed::Fast);
    assert_eq!(Speed::for_devices(100_000), Speed::Standard);
}

#[test]
fn set_up() {
    let sim = Sim::new(64, 64);
    let controller = sim.controller(Speed::Fast);
    assert_eq!(controller.depths(), (64, 64));
    assert_eq!(controller.version(), VERSION_1_21A);
    sim.state(|state| {
        // Master, fast mode, repeated starts, no slave; polled.
        assert_eq!(state.con, 0x65);
        assert_eq!(state.counts, [0, 0, 105, 191]);
        assert_eq!(state.sda_hold, 0x1c | 1 << 16);
        assert_eq!(state.mask, 0);
        assert!(!state.enabled);
    });

    // Before 1.10a there is no SDA hold register.
    let sim = Sim::new(16, 8);
    sim.with(|state| state.version = VERSION_1_01A);
    let controller = sim.controller(Speed::Standard);
    assert_eq!(controller.depths(), (16, 8));
    sim.state(|state| {
        assert_eq!(state.con, 0x63);
        assert_eq!(state.counts, [513, 599, 0, 0]);
        assert_eq!(state.sda_hold, 1);
    });
}

#[test]
fn unusable() {
    let timing = Timing::computed(Speed::Fast, 120_000_000, None);
    let new = |sim: &Sim, size| {
        DesignWare::new(
            Mmio::fake(Arc::new(sim.clone()), size),
            Clock(sim.clone()),
            Speed::Fast,
            timing,
        )
        .err()
    };
    let sim = Sim::new(16, 16);
    assert_eq!(new(&sim, 0x80), Some(Unusable::Registers));
    sim.with(|state| state.comp_type = u32::MAX);
    assert_eq!(new(&sim, SIZE), Some(Unusable::Absent));
    sim.with(|state| state.comp_type = 0x1234);
    assert_eq!(new(&sim, SIZE), Some(Unusable::NotDesignWare(0x1234)));
}

#[test]
fn register_read() {
    let sim = Sim::new(16, 16);
    sim.device(0x2c, false);
    let mut controller = sim.controller(Speed::Fast);
    let mut buffer = [0; 4];
    controller
        .transfer(0x2c, false, &mut [Message::Write(&[0x10]), Message::Read(&mut buffer)])
        .unwrap();
    assert_eq!(buffer, sim.memory(0)[0x10..0x14]);
    assert_eq!(sim.events(), [Event::Start(0x2c, false), Event::Restart(true), Event::Stop]);
    sim.clean();
    // Waiting for the bus once all was queued, it slept.
    assert!(sim.state(|state| state.slept) > 0);
}

#[test]
fn long_transfers() {
    // Small FIFOs, so that commands are queued as others are taken and
    // reads wait for room in the receive FIFO.
    let sim = Sim::new(8, 4);
    sim.device(0x50, false).device(0x51, false);
    let mut controller = sim.controller(Speed::Fast);
    let data: Vec<u8> = (0..100u8).map(|i| i.wrapping_mul(3)).collect();
    let mut write = vec![0x20];
    write.extend_from_slice(&data);
    controller.transfer(0x51, false, &mut [Message::Write(&write)]).unwrap();
    assert_eq!(sim.memory(1)[0x20..0x20 + 100], data[..]);
    assert_eq!(sim.memory(0), core::array::from_fn(pattern));
    assert_eq!(sim.events(), [Event::Start(0x51, false), Event::Stop]);

    let mut buffer = [0; 200];
    controller
        .transfer(0x51, false, &mut [Message::Write(&[0x20]), Message::Read(&mut buffer)])
        .unwrap();
    assert_eq!(buffer[..], sim.memory(1)[0x20..0xe8]);
    sim.clean();

    // A read alone; then a write after a read, each after a repeated
    // start.
    let (mut first, mut second) = ([0; 3], [0; 5]);
    controller.transfer(0x50, false, &mut [Message::Read(&mut first)]).unwrap();
    assert_eq!(first[..], [pattern(0), pattern(1), pattern(2)]);
    sim.events();
    controller
        .transfer(
            0x50,
            false,
            &mut [
                Message::Write(&[0x40]),
                Message::Read(&mut first),
                Message::Write(&[0x80, 9, 8]),
                Message::Read(&mut second),
            ],
        )
        .unwrap();
    let memory = sim.memory(0);
    assert_eq!(first, memory[0x40..0x43]);
    assert_eq!(memory[0x80..0x82], [9, 8]);
    assert_eq!(second, memory[0x82..0x87]);
    assert_eq!(
        sim.events(),
        [
            Event::Start(0x50, false),
            Event::Restart(true),
            Event::Restart(false),
            Event::Restart(true),
            Event::Stop
        ]
    );
    sim.clean();
}

#[test]
fn errors() {
    let sim = Sim::new(16, 16);
    sim.device(0x15, false);
    let mut controller = sim.controller(Speed::Standard);
    let mut buffer = [0; 2];

    let mut register = |controller: &mut DesignWare<Clock>, address| {
        controller.transfer(address, false, &mut [Message::Write(&[0]), Message::Read(&mut buffer)])
    };
    assert_eq!(register(&mut controller, 0x16), Err(Error::NoDevice));
    assert_eq!(sim.events(), [Event::Abort(ABRT_7BIT_NACK)]);
    // The bus recovers.
    assert_eq!(register(&mut controller, 0x15), Ok(()));
    sim.events();

    // The device stops acknowledging after two bytes.
    sim.with(|state| state.devices[0].acknowledges = 2);
    let bytes = [0x30, 1, 2, 3, 4];
    assert_eq!(controller.transfer(0x15, false, &mut [Message::Write(&bytes)]), Err(Error::Nack));
    assert_eq!(sim.memory(0)[0x30..0x33], [1, 2, pattern(0x32)]);
    assert_eq!(sim.events(), [Event::Start(0x15, false), Event::Abort(ABRT_DATA_NACK)]);
    sim.with(|state| state.devices[0].acknowledges = usize::MAX);
    assert_eq!(register(&mut controller, 0x15), Ok(()));

    // What cannot be sent.
    assert_eq!(controller.transfer(0x80, false, &mut [Message::Write(&[0])]), Err(Error::Invalid));
    assert_eq!(controller.transfer(0x15, false, &mut []), Err(Error::Invalid));
    assert_eq!(
        controller.transfer(0x15, false, &mut [Message::Read(&mut [])]),
        Err(Error::Invalid)
    );
    sim.clean();
}

#[test]
fn ten_bit_address() {
    let sim = Sim::new(16, 16);
    sim.device(0x2a5, true).device(0x25, false);
    let mut controller = sim.controller(Speed::Fast);
    let mut buffer = [0; 2];
    controller
        .transfer(0x2a5, true, &mut [Message::Write(&[0x07]), Message::Read(&mut buffer)])
        .unwrap();
    assert_eq!(buffer, sim.memory(0)[7..9]);
    sim.state(|state| {
        assert_eq!(state.tar, 0x2a5 | 1 << 12);
        assert_ne!(state.con & 1 << 4, 0);
    });
    // And back to a 7-bit address.
    controller.transfer(0x25, false, &mut [Message::Read(&mut buffer)]).unwrap();
    sim.state(|state| {
        assert_eq!(state.tar, 0x25);
        assert_eq!(state.con & 1 << 4, 0);
    });
    assert_eq!(
        controller.transfer(0x400, true, &mut [Message::Read(&mut buffer)]),
        Err(Error::Invalid)
    );
    sim.clean();
}

#[test]
fn stuck_bus() {
    let sim = Sim::new(16, 16);
    sim.device(0x15, false);
    let mut controller = sim.controller(Speed::Fast);
    sim.with(|state| state.stuck = true);
    let mut buffer = [0; 2];
    assert_eq!(
        controller.transfer(0x15, false, &mut [Message::Write(&[0]), Message::Read(&mut buffer)]),
        Err(Error::Timeout)
    );
    // The transfer was aborted and the controller left disabled.
    assert_eq!(sim.events(), [Event::Abort(ABRT_USER)]);
    sim.state(|state| {
        assert!(!state.enabled);
        assert!(state.slept + state.spun < 100_000);
    });
    // Once the bus is free, transfers work again.
    sim.with(|state| state.stuck = false);
    controller
        .transfer(0x15, false, &mut [Message::Write(&[0]), Message::Read(&mut buffer)])
        .unwrap();
    sim.clean();
}

#[test]
fn display() {
    assert_eq!(Error::NoDevice.to_string(), "no device acknowledged its address");
    assert_eq!(Error::Aborted(0x10000).to_string(), "the controller aborted it (0x10000)");
    assert_eq!(
        Unusable::NotDesignWare(5).to_string(),
        "it is not a DesignWare controller (IC_COMP_TYPE reads 0x5)"
    );
}

/// LPSS's registers: what is written stays, and each write is kept.
struct Private(Mutex<Memory>);

struct Memory {
    registers: Vec<u32>,
    writes: Vec<(usize, u32)>,
}

impl Registers for Private {
    fn read(&self, offset: usize, size: usize) -> u64 {
        assert_eq!(size, 4);
        self.0.lock().unwrap().registers[offset / 4].into()
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 4);
        let mut memory = self.0.lock().unwrap();
        memory.registers[offset / 4] = value as u32;
        memory.writes.push((offset, value as u32));
    }
}

#[test]
fn lpss_reset() {
    let set_up = |caps: u32| {
        let mut registers = vec![0; lpss::PRIVATE_SIZE / 4];
        registers[0xfc / 4] = caps;
        let private = Arc::new(Private(Mutex::new(Memory { registers, writes: Vec::new() })));
        let result = lpss::set_up(&Mmio::fake(private.clone(), lpss::PRIVATE_SIZE), 0x1_d000_1000);
        (result, private.0.lock().unwrap().writes.clone())
    };
    // Reset, out of it with the DMA engine, which is told the address.
    assert_eq!(set_up(0), (Ok(()), vec![(0x04, 0), (0x04, 0x7), (0x40, 0xd000_1000), (0x44, 1)]));
    // Without a DMA engine there is no address to tell.
    assert_eq!(set_up(1 << 8), (Ok(()), vec![(0x04, 0), (0x04, 0x7)]));
    // A UART, an SPI controller, or nothing there.
    assert_eq!(set_up(0x10), (Err(NotI2c(1)), vec![]));
    assert_eq!(set_up(0x20), (Err(NotI2c(2)), vec![]));
    assert_eq!(set_up(u32::MAX).0, Err(NotI2c(0xf)));
}
