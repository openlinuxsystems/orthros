// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Debug Capability against a simulated one, and a debug host on the
//! other end of its cable that plugs in, configures it, sends, halts its
//! endpoints, resets its port and pulls out.

use super::*;
use crate::dbc::{Change, Dbc, Identity, Status};

/// DCCTRL's, DCPORTSC's and DCST's fields, as the simulation sets them.
const RUN: u32 = 1 << 0;
const PORT_ENABLE: u32 = 1 << 1;
const HALT_OUT: u32 = 1 << 2;
const HALT_IN: u32 = 1 << 3;
const RUN_CHANGE: u32 = 1 << 4;
const ENABLE: u32 = 1 << 31;
const CONNECTED: u32 = 1 << 0;
const ENABLED: u32 = 1 << 1;
const CHANGES: u32 = (1 << 17) | (1 << 21) | (1 << 22) | (1 << 23);
const RESET_CHANGE: u32 = 1 << 21;
/// The root port the debug host is on.
const PORT: u8 = 2;
/// The max burst it says it takes.
const BURST: u32 = 3;

/// The simulated controller's event ring for its Debug Capability.
#[derive(Default)]
struct SimEvents {
    base: u64,
    size: u64,
    enqueue: u64,
    cycle: bool,
}

/// A transfer ring of the capability, as it reads it.
#[derive(Clone, Copy, Default)]
struct DbcRing {
    dequeue: u64,
    cycle: bool,
}

/// The simulated Debug Capability, and the debug host on its cable.
#[derive(Default)]
pub(super) struct SimDbc {
    control: u32,
    portsc: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    context: u64,
    info1: u32,
    info2: u32,
    events: SimEvents,
    out: DbcRing,
    input: DbcRing,
    /// The endpoints' IDs in transfer events, from this: 0 as AMD's
    /// controllers number them, 2 as Intel's.
    first_endpoint: u8,
    /// It never turns on.
    stuck: bool,
    /// A debug host is plugged in.
    host: bool,
    /// What the debug host received, and what it sends next, a packet
    /// each.
    received: Vec<u8>,
    sends: VecDeque<Vec<u8>>,
    /// The next OUT transfer stalls.
    stall_next: bool,
    /// How many times it was turned on.
    enables: u32,
    /// What the debug host read when it configured it: the Device
    /// Descriptor Info registers and the strings.
    seen: Option<(u32, u32, Vec<String>)>,
}

impl SimDbc {
    pub(super) fn read(&self, offset: usize) -> u32 {
        match offset {
            // Its ID, Debug, and no next capability.
            0x00 => xcap::DEBUG,
            0x08 => self.erstsz,
            0x18 => self.erdp as u32,
            0x20 => self.control | BURST << 16,
            0x24 => {
                if self.control & RUN != 0 {
                    u32::from(PORT) << 24
                } else {
                    0
                }
            }
            0x28 => self.portsc,
            0x38 => self.info1,
            0x3c => self.info2,
            _ => 0,
        }
    }

    pub(super) fn write(&mut self, offset: usize, value: u32) {
        let low = |old: u64| (old & !0xffff_ffff) | u64::from(value);
        let high = |old: u64| (old & 0xffff_ffff) | u64::from(value) << 32;
        match offset {
            0x04 => match (value >> 8) & 0xff {
                0 => self.run_out(),
                1 => self.run_in(),
                target => panic!("a doorbell for target {target}"),
            },
            0x08 => self.erstsz = value,
            0x10 => self.erstba = low(self.erstba),
            0x14 => self.erstba = high(self.erstba),
            0x18 => self.erdp = low(self.erdp),
            0x1c => self.erdp = high(self.erdp),
            0x20 => self.write_control(value),
            0x28 => self.portsc &= !(value & CHANGES),
            0x30 => self.context = low(self.context),
            0x34 => self.context = high(self.context),
            0x38 => self.info1 = value,
            0x3c => self.info2 = value,
            _ => {}
        }
    }

    fn write_control(&mut self, value: u32) {
        if value & ENABLE == 0 {
            self.turn_off();
            return;
        }
        // The halts and Run Change are cleared by writing 1.
        self.control &= !(value & (HALT_OUT | HALT_IN | RUN_CHANGE));
        if self.control & ENABLE != 0 || self.stuck {
            return;
        }
        // Turned on: it reads its event ring's table, and finds the debug
        // host if one is plugged in.
        assert!(self.erstsz == 1 && self.context != 0, "turned on before it was set up");
        self.enables += 1;
        self.control = ENABLE | (value & PORT_ENABLE);
        self.events = SimEvents {
            base: State::read64(self.erstba),
            size: u64::from(State::read32(self.erstba + 8)),
            enqueue: 0,
            cycle: true,
        };
        if self.host {
            self.connect();
        }
    }

    /// Off, by the driver or a reset of the controller.
    pub(super) fn turn_off(&mut self) {
        self.control = 0;
        self.portsc = 0;
    }

    fn connect(&mut self) {
        self.portsc = CONNECTED | (1 << 17);
        self.port_event();
    }

    fn post(&mut self, parameter: u64, status: u32, control: u32) {
        let e = &mut self.events;
        let dequeue = (self.erdp & !0xf).wrapping_sub(e.base) / TRB_SIZE as u64;
        assert_ne!((e.enqueue + 1) % e.size, dequeue, "the event ring filled");
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&parameter.to_le_bytes());
        bytes[8..12].copy_from_slice(&status.to_le_bytes());
        bytes[12..].copy_from_slice(&(control | u32::from(e.cycle)).to_le_bytes());
        fake_memory_write(e.base + e.enqueue * TRB_SIZE as u64, &bytes);
        e.enqueue += 1;
        if e.enqueue == e.size {
            e.enqueue = 0;
            e.cycle = !e.cycle;
        }
    }

    fn port_event(&mut self) {
        self.post(1 << 24, u32::from(code::SUCCESS) << 24, kind::PORT_STATUS_CHANGE << 10);
    }

    fn transfer_event(&mut self, trb: u64, endpoint: u8, code: u8, residual: u32) {
        self.post(
            trb,
            u32::from(code) << 24 | residual,
            kind::TRANSFER_EVENT << 10 | u32::from(endpoint) << 16,
        );
    }

    /// The next TRB of `ring` that is handed over, past Link and No Op
    /// TRBs: its address and fields.
    fn next(ring: &mut DbcRing) -> Option<(u64, u64, u32)> {
        loop {
            let (parameter, status, control) = State::read_trb(ring.dequeue);
            if (control & 1 != 0) != ring.cycle {
                return None;
            }
            match (control >> 10) & 0x3f {
                kind::LINK => {
                    ring.dequeue = parameter;
                    if control & control::TOGGLE_CYCLE != 0 {
                        ring.cycle = !ring.cycle;
                    }
                }
                kind::NO_OP => ring.dequeue += TRB_SIZE as u64,
                kind::NORMAL => return Some((ring.dequeue, parameter, status)),
                other => panic!("a TRB of type {other} on a Debug Capability ring"),
            }
        }
    }

    /// Sends what the OUT ring holds to the debug host.
    fn run_out(&mut self) {
        while self.control & RUN != 0 && self.control & HALT_OUT == 0 {
            let mut ring = self.out;
            let Some((at, buffer, status)) = SimDbc::next(&mut ring) else {
                self.out = ring;
                return;
            };
            if self.stall_next {
                // The endpoint halts on the TRB, and its context says so.
                self.stall_next = false;
                self.out = ring;
                self.control |= HALT_OUT;
                fake_memory_write(
                    self.context + 64 + 8,
                    &(at | u64::from(ring.cycle)).to_le_bytes(),
                );
                self.transfer_event(at, self.first_endpoint, code::STALL, status & 0x1_ffff);
                return;
            }
            let mut data = std::vec![0u8; (status & 0x1_ffff) as usize];
            fake_memory(buffer, &mut data);
            self.received.extend_from_slice(&data);
            ring.dequeue += TRB_SIZE as u64;
            self.out = ring;
            self.transfer_event(at, self.first_endpoint, code::SUCCESS, 0);
        }
    }

    /// Gives the reads waiting on the IN ring what the debug host sends.
    fn run_in(&mut self) {
        while self.control & RUN != 0 && self.control & HALT_IN == 0 && !self.sends.is_empty() {
            let mut ring = self.input;
            let Some((at, buffer, status)) = SimDbc::next(&mut ring) else {
                self.input = ring;
                return;
            };
            let data = self.sends.pop_front().unwrap();
            let length = (status & 0x1_ffff) as usize;
            assert!(data.len() <= length, "a packet longer than the read");
            fake_memory_write(buffer, &data);
            ring.dequeue += TRB_SIZE as u64;
            self.input = ring;
            let (code, residual) = if data.len() < length {
                (code::SHORT_PACKET, (length - data.len()) as u32)
            } else {
                (code::SUCCESS, 0)
            };
            self.transfer_event(at, self.first_endpoint + 1, code, residual);
        }
    }

    /// The debug host plugs in.
    fn plug(&mut self) {
        self.host = true;
        if self.control & ENABLE != 0 {
            self.connect();
        }
    }

    /// The debug host pulls out.
    fn unplug(&mut self) {
        self.host = false;
        self.portsc = 1 << 17;
        if self.control & RUN != 0 {
            self.control = (self.control & !RUN) | RUN_CHANGE;
        }
        self.port_event();
    }

    /// The debug host resets the port: it is no longer configured.
    fn reset_port(&mut self) {
        self.portsc = (self.portsc & !ENABLED) | RESET_CHANGE;
        self.control = (self.control & !RUN) | RUN_CHANGE;
        self.port_event();
    }

    /// The debug host enumerates and configures it: it reads its
    /// descriptors from the context, and the endpoints start where their
    /// contexts say. Resetting the port on the way leaves a change.
    fn configure(&mut self) {
        assert!(self.control & ENABLE != 0 && self.portsc & CONNECTED != 0);
        let lengths = State::read32(self.context + 32);
        let mut strings = Vec::new();
        for i in 0..4u64 {
            let at = State::read64(self.context + 8 * i);
            let length = (lengths >> (8 * i)) as u8;
            let mut bytes = std::vec![0u8; usize::from(length)];
            fake_memory(at, &mut bytes);
            assert_eq!((bytes[0], bytes[1]), (length, 3), "string descriptor {i}");
            if i == 0 {
                assert_eq!(bytes, [4, 3, 0x09, 0x04]);
                continue;
            }
            let units: Vec<u16> =
                bytes[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            strings.push(String::from_utf16(&units).unwrap());
        }
        self.seen = Some((self.info1, self.info2, strings));
        for (ring, at, kind) in [(&mut self.out, 64, 2), (&mut self.input, 128, 6)] {
            let info2 = State::read32(self.context + at + 4);
            assert_eq!(info2, kind << 3 | BURST << 8 | 1024 << 16);
            let dequeue = State::read64(self.context + at + 8);
            *ring = DbcRing { dequeue: dequeue & !0xf, cycle: dequeue & 1 != 0 };
        }
        self.portsc |= ENABLED | RESET_CHANGE;
        self.control |= RUN | RUN_CHANGE;
        self.run_in();
    }
}

impl State {
    /// A controller with a Debug Capability.
    fn with_dbc(first_endpoint: u8) -> State {
        let mut state = State::new();
        state.dbc = Some(SimDbc { first_endpoint, ..SimDbc::default() });
        state
    }

    fn dbc(&mut self) -> &mut SimDbc {
        self.dbc.as_mut().unwrap()
    }
}

fn identity() -> Identity {
    Identity {
        vendor: 0x1d6b,
        product: 0x0010,
        revision: 0x0010,
        protocol: 1,
        manufacturer: "Dukovo",
        product_name: "Radix debug console",
        serial: "0001",
    }
}

/// Starts a driver on `sim`, and its Debug Capability, which turns on.
fn start_dbc(sim: &Arc<Sim>) -> (Xhci, Dbc) {
    let (xhci, _host) = start(sim);
    let mut below = Vec::new();
    let mut dbc = Dbc::new(&xhci, identity(), &mut alloc(&mut below)).unwrap();
    assert_eq!(dbc.status(), Status::Off);
    assert_eq!(dbc.poll(0), None);
    dbc.start(0);
    assert_eq!(dbc.status(), Status::Starting);
    assert_eq!(dbc.poll(0), Some(Change::Enabled));
    assert_eq!(dbc.status(), Status::Waiting);
    (xhci, dbc)
}

/// As `start_dbc`, with a debug host plugged in that configures it.
fn running(first_endpoint: u8) -> (Arc<Sim>, Xhci, Dbc) {
    let sim = Sim::new(State::with_dbc(first_endpoint));
    let (xhci, mut dbc) = start_dbc(&sim);
    sim.state().dbc().plug();
    assert_eq!(dbc.poll(10), Some(Change::Connected));
    sim.state().dbc().configure();
    assert_eq!(dbc.poll(20), Some(Change::Configured { port: PORT }));
    assert_eq!(dbc.status(), Status::Running);
    (sim, xhci, dbc)
}

/// What the debug host received.
fn received(sim: &Arc<Sim>) -> Vec<u8> {
    sim.state().dbc().received.clone()
}

#[test]
fn the_debug_capability_is_set_up_and_configured_as_linux_does_it() {
    let sim = Sim::new(State::with_dbc(2));
    let (xhci, mut dbc) = start_dbc(&sim);
    assert_eq!(xhci.info().debug_capability, Some(DBC));
    {
        let mut s = sim.state();
        let d = s.dbc();
        assert_eq!(d.enables, 1);
        assert_eq!(d.control, ENABLE | PORT_ENABLE);
        assert_eq!((d.erstsz, d.events.size), (1, 256));
        assert_eq!(d.info1, 0x1d6b << 16 | 1);
        assert_eq!(d.info2, 0x0010 << 16 | 0x0010);
    }
    // Nothing until a debug host plugs in, however long that takes.
    assert_eq!(dbc.poll(60_000_000), None);
    sim.state().dbc().plug();
    assert_eq!(dbc.poll(60_000_010), Some(Change::Connected));
    assert_eq!(dbc.status(), Status::Connecting);
    assert_eq!(dbc.poll(60_000_020), None);
    sim.state().dbc().configure();
    assert_eq!(dbc.poll(60_000_030), Some(Change::Configured { port: PORT }));
    let mut s = sim.state();
    let d = s.dbc();
    let (info1, info2, strings) = d.seen.clone().unwrap();
    assert_eq!((info1, info2), (0x1d6b << 16 | 1, 0x0010 << 16 | 0x0010));
    assert_eq!(strings, ["Dukovo", "Radix debug console", "0001"]);
    // The changes that configuring it left are cleared, and a read of a
    // packet waits on the IN ring.
    assert_eq!((d.portsc & CHANGES, d.control & RUN_CHANGE), (0, 0));
    let (_, status, control) = State::read_trb(d.input.dequeue);
    assert_eq!((control >> 10) & 0x3f, kind::NORMAL);
    assert_eq!((status, control & 1), (1024, 1));
}

#[test]
fn a_controller_without_one_has_none() {
    let sim = Sim::new(State::new());
    let (xhci, _host) = start(&sim);
    assert_eq!(xhci.info().debug_capability, None);
    let mut below = Vec::new();
    assert!(matches!(
        Dbc::new(&xhci, identity(), &mut alloc(&mut below)),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn bytes_go_both_ways_with_either_numbering_of_its_endpoints() {
    for first_endpoint in [0, 2] {
        let (sim, _xhci, mut dbc) = running(first_endpoint);
        // One write at a time.
        assert_eq!(dbc.write(b"Radix boots\n"), 12);
        assert!(dbc.writing());
        assert_eq!(dbc.write(b"more"), 0);
        assert_eq!(dbc.poll(30), None);
        assert!(!dbc.writing());
        assert_eq!(received(&sim), b"Radix boots\n");
        // A page at most.
        assert_eq!(dbc.write(&[b'x'; 5000]), PAGE_SIZE);
        dbc.poll(40);
        assert_eq!(received(&sim).len(), 12 + PAGE_SIZE);
        // What the debug host sends is held until it is all taken.
        sim.state().dbc().sends.extend([b"help\n".to_vec(), b"continue\n".to_vec()]);
        sim.state().dbc().run_in();
        dbc.poll(50);
        let mut bytes = [0u8; 16];
        assert_eq!(dbc.read(&mut bytes[..3]), 3);
        assert_eq!(&bytes[..3], b"hel");
        assert_eq!(sim.state().dbc().sends.len(), 1);
        assert_eq!(dbc.read(&mut bytes), 2);
        assert_eq!(&bytes[..2], b"p\n");
        // Taking it all started the next read, which the second packet
        // ended.
        assert_eq!(dbc.read(&mut bytes), 0);
        dbc.poll(60);
        assert_eq!(dbc.read(&mut bytes), 9);
        assert_eq!(&bytes[..9], b"continue\n");
        assert_eq!(dbc.read(&mut bytes), 0);
    }
}

#[test]
fn pulling_out_or_resetting_the_port_empties_the_rings_for_the_next_host() {
    let (sim, _xhci, mut dbc) = running(2);
    assert_eq!(dbc.write(b"a"), 1);
    sim.state().dbc().unplug();
    assert_eq!(dbc.poll(30), Some(Change::Disconnected { reset: false }));
    assert_eq!(dbc.status(), Status::Waiting);
    assert!(!dbc.writing());
    assert_eq!(dbc.write(b"b"), 0);
    sim.state().dbc().plug();
    assert_eq!(dbc.poll(40), Some(Change::Connected));
    sim.state().dbc().configure();
    assert_eq!(dbc.poll(50), Some(Change::Configured { port: PORT }));
    assert_eq!(dbc.write(b"c"), 1);
    dbc.poll(60);
    assert_eq!(received(&sim), b"ac");
    // The debug host resets the port, and configures it again.
    sim.state().dbc().reset_port();
    assert_eq!(dbc.poll(70), Some(Change::Disconnected { reset: true }));
    assert_eq!(dbc.poll(80), Some(Change::Connected));
    sim.state().dbc().configure();
    assert_eq!(dbc.poll(90), Some(Change::Configured { port: PORT }));
    sim.state().dbc().sends.push_back(b"x".to_vec());
    sim.state().dbc().run_in();
    assert_eq!(dbc.write(b"d"), 1);
    dbc.poll(100);
    assert_eq!(received(&sim), b"acd");
    let mut bytes = [0u8; 4];
    assert_eq!(dbc.read(&mut bytes), 1);
    assert_eq!(bytes[0], b'x');
}

#[test]
fn turned_off_under_it_it_is_set_up_again() {
    let (sim, mut xhci, mut dbc) = running(2);
    // A reset of the controller turns it off.
    {
        let mut s = sim.state();
        s.usbsts |= op::HALTED;
        s.reset();
    }
    assert_eq!(dbc.poll(30), Some(Change::TurnedOff));
    assert_eq!(dbc.status(), Status::Starting);
    assert_eq!(dbc.poll(40), Some(Change::Enabled));
    assert_eq!(sim.state().dbc().enables, 2);
    // The debug host is still there.
    assert_eq!(dbc.poll(50), Some(Change::Connected));
    sim.state().dbc().configure();
    assert_eq!(dbc.poll(60), Some(Change::Configured { port: PORT }));
    assert_eq!(dbc.write(b"again"), 5);
    dbc.poll(70);
    assert_eq!(received(&sim), b"again");
    let mut host = host(&sim);
    let _ = xhci.halt(&mut host);
}

#[test]
fn a_debug_host_that_does_not_configure_it_in_time_has_it_turned_off_and_on() {
    let sim = Sim::new(State::with_dbc(2));
    let (_xhci, mut dbc) = start_dbc(&sim);
    sim.state().dbc().plug();
    assert_eq!(dbc.poll(1_000), Some(Change::Connected));
    assert_eq!(dbc.poll(5_000_999), None);
    assert_eq!(dbc.poll(5_001_000), Some(Change::NotConfigured));
    assert_eq!(dbc.poll(5_001_010), Some(Change::Enabled));
    assert_eq!(sim.state().dbc().enables, 2);
    assert_eq!(dbc.poll(5_001_020), Some(Change::Connected));
    // One that goes away before configuring it is waited for again.
    sim.state().dbc().unplug();
    assert_eq!(dbc.poll(5_001_030), None);
    assert_eq!(dbc.status(), Status::Waiting);
}

#[test]
fn a_stalled_write_is_dropped_and_a_halted_read_goes_on_once_the_halt_clears() {
    let (sim, _xhci, mut dbc) = running(0);
    sim.state().dbc().stall_next = true;
    assert_eq!(dbc.write(b"lost"), 4);
    assert_eq!(dbc.poll(30), None);
    assert!(!dbc.writing());
    // Halted: no write starts, and the stalled TRB is a No Op now.
    assert_eq!(dbc.write(b"held"), 0);
    let at = sim.state().dbc().out.dequeue;
    assert_eq!((State::read_trb(at).2 >> 10) & 0x3f, kind::NO_OP);
    // The debug host clears the halt: the halt bit is never written back
    // until then.
    assert_eq!(sim.state().dbc().control & HALT_OUT, HALT_OUT);
    sim.state().dbc().control &= !HALT_OUT;
    assert_eq!(dbc.poll(40), None);
    assert_eq!(dbc.write(b"next"), 4);
    dbc.poll(50);
    assert_eq!(received(&sim), b"next");
    // A halt of the IN endpoint holds the waiting read until it clears.
    sim.state().dbc().control |= HALT_IN;
    assert_eq!(dbc.poll(60), None);
    sim.state().dbc().sends.push_back(b"q".to_vec());
    sim.state().dbc().run_in();
    let mut bytes = [0u8; 4];
    assert_eq!(dbc.poll(70), None);
    assert_eq!(dbc.read(&mut bytes), 0);
    sim.state().dbc().control &= !HALT_IN;
    assert_eq!(dbc.poll(80), None);
    dbc.poll(90);
    assert_eq!(dbc.read(&mut bytes), 1);
    assert_eq!(bytes[0], b'q');
}

#[test]
fn one_that_will_not_turn_on_is_given_up_and_stop_turns_it_off() {
    let mut state = State::with_dbc(2);
    state.dbc().stuck = true;
    let sim = Sim::new(state);
    let (xhci, _host) = start(&sim);
    let mut below = Vec::new();
    let mut dbc = Dbc::new(&xhci, identity(), &mut alloc(&mut below)).unwrap();
    dbc.start(1_000);
    assert_eq!(dbc.poll(1_000), None);
    assert_eq!(dbc.poll(100_999), None);
    assert_eq!(dbc.poll(101_000), Some(Change::Failed("it did not turn on")));
    assert_eq!(dbc.status(), Status::Failed);
    assert_eq!(dbc.poll(200_000), None);
    assert_eq!(dbc.write(b"x"), 0);
    // One that works, stopped.
    let (sim, _xhci, mut dbc) = running(2);
    dbc.stop();
    assert_eq!(dbc.status(), Status::Off);
    assert_eq!(dbc.poll(30), None);
    assert_eq!(sim.state().dbc().control, 0);
}
