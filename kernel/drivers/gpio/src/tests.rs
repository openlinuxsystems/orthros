// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The drivers against simulated controllers. AMD's has a register a
//! pin whose pending bits are cleared by writing them as 1, summary
//! registers, and a master register that counts ends of interrupts.
//! Intel's has three communities, pads the firmware owns, keeps for its
//! GPEs or locks, and status bits cleared by writing them as 1. A pin's
//! input is driven by the test; a level that is active sets its status
//! again as soon as it is cleared, as the hardware's does.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Mmio, Registers};

use super::*;

const LEVEL_TRIGGERED: u32 = 1 << 8;
const INTERRUPT_ENABLE: u32 = 1 << 11;
const INTERRUPT_MASK: u32 = 1 << 12;
const WAKE: u32 = 7 << 13;
const PIN_STATUS: u32 = 1 << 16;
const INTERRUPT_STATUS: u32 = 1 << 28;
const WAKE_STATUS: u32 = 1 << 29;
/// A pin as the simulation starts it: an input, high, with its drive
/// strength's bit set.
const IDLE: u32 = PIN_STATUS | 1 << 18;

struct AmdState {
    pins: Vec<u32>,
    ends: u32,
}

impl AmdState {
    /// Sets the status of a pin whose level is active, after a change.
    fn settle(&mut self, pin: usize) {
        let value = self.pins[pin];
        let high = value & PIN_STATUS != 0;
        let active = match (value >> 9) & 3 {
            0 => high,
            1 => !high,
            _ => false,
        };
        if value & INTERRUPT_ENABLE != 0 && value & LEVEL_TRIGGERED != 0 && active {
            self.pins[pin] |= INTERRUPT_STATUS;
        }
    }

    fn drive(&mut self, pin: usize, high: bool) {
        let value = self.pins[pin];
        let was = value & PIN_STATUS != 0;
        self.pins[pin] = if high { value | PIN_STATUS } else { value & !PIN_STATUS };
        let edge = match (value >> 9) & 3 {
            0 => !was && high,
            1 => was && !high,
            _ => was != high,
        };
        if value & INTERRUPT_ENABLE != 0 && value & LEVEL_TRIGGERED == 0 && edge {
            self.pins[pin] |= INTERRUPT_STATUS;
        }
        self.settle(pin);
    }
}

#[derive(Clone)]
struct AmdSim(Arc<Mutex<AmdState>>);

impl AmdSim {
    fn new() -> AmdSim {
        let mut pins = vec![IDLE; 256];
        pins[63] = 0;
        AmdSim(Arc::new(Mutex::new(AmdState { pins, ends: 0 })))
    }

    fn mmio(&self) -> Mmio {
        Mmio::fake(Arc::new(self.clone()), 0x400)
    }

    fn drive(&self, pin: usize, high: bool) {
        self.0.lock().unwrap().drive(pin, high);
    }

    fn pin(&self, pin: usize) -> u32 {
        self.0.lock().unwrap().pins[pin]
    }

    fn set(&self, pin: usize, value: u32) {
        self.0.lock().unwrap().pins[pin] = value;
    }

    fn ends(&self) -> u32 {
        self.0.lock().unwrap().ends
    }
}

impl Registers for AmdSim {
    fn read(&self, offset: usize, size: usize) -> u64 {
        assert_eq!(size, 4);
        let state = self.0.lock().unwrap();
        match offset {
            0x2f8 | 0x2fc => {
                let mut summary = 0u64;
                for group in 0..46 {
                    let pending = (group * 4..group * 4 + 4)
                        .any(|pin| pin != 63 && state.pins[pin] & INTERRUPT_STATUS != 0);
                    summary |= u64::from(pending) << group;
                }
                if offset == 0x2f8 { summary & 0xffff_ffff } else { summary >> 32 }
            }
            _ => state.pins[offset / 4].into(),
        }
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 4);
        assert!(offset < 0x2f8, "a write to the summary registers");
        let value = value as u32;
        let mut state = self.0.lock().unwrap();
        let pin = offset / 4;
        if pin == 63 {
            if value & (1 << 29) != 0 {
                state.ends += 1;
            }
            state.pins[63] = value & !(1 << 29);
            return;
        }
        let old = state.pins[pin];
        let pending = INTERRUPT_STATUS | WAKE_STATUS;
        let kept = old & pending & !(value & pending);
        state.pins[pin] = (value & !(pending | PIN_STATUS)) | (old & PIN_STATUS) | kept;
        state.settle(pin);
    }
}

fn pending(controller: &Controller) -> Vec<u16> {
    let mut pins = Vec::new();
    controller.pending(&mut |pin| pins.push(pin));
    pins
}

#[test]
fn an_amd_pin_level_low_interrupts_while_low_and_again_once_unmasked() {
    let sim = AmdSim::new();
    let gpio = Controller::amd(sim.mmio()).unwrap();
    assert_eq!((gpio.name(), gpio.pins()), ("AMD's", 184));
    gpio.set_interrupt(9, Trigger::LevelLow).unwrap();
    let value = sim.pin(9);
    assert_eq!(value & (LEVEL_TRIGGERED | 3 << 9), LEVEL_TRIGGERED | 1 << 9);
    assert_eq!(value & (INTERRUPT_ENABLE | INTERRUPT_MASK), INTERRUPT_ENABLE);
    // Masked: its status is kept, not reported.
    sim.drive(9, false);
    assert_eq!(pending(&gpio), []);
    assert_ne!(sim.pin(9) & INTERRUPT_STATUS, 0);
    gpio.unmask(9);
    assert_eq!(pending(&gpio), [9]);
    gpio.finish();
    assert_eq!(sim.ends(), 1);
    // Still low while its device is served: masked, then unmasked, it is
    // pending again; once high, it is not.
    gpio.mask(9);
    assert_eq!(pending(&gpio), []);
    gpio.unmask(9);
    assert_eq!(pending(&gpio), [9]);
    sim.drive(9, true);
    assert_eq!(pending(&gpio), [9]);
    assert_eq!(pending(&gpio), []);
    assert_eq!(gpio.level(9), Ok(true));
}

#[test]
fn amd_pins_interrupt_on_their_edges_and_are_each_reported_once() {
    let sim = AmdSim::new();
    let gpio = Controller::amd(sim.mmio()).unwrap();
    // An edge from before the pin was set up is dropped.
    sim.set(130, IDLE | INTERRUPT_STATUS);
    gpio.set_interrupt(130, Trigger::FallingEdge).unwrap();
    gpio.set_interrupt(131, Trigger::RisingEdge).unwrap();
    gpio.set_interrupt(4, Trigger::BothEdges).unwrap();
    for pin in [130, 131, 4] {
        gpio.unmask(pin);
    }
    assert_eq!(pending(&gpio), []);
    sim.drive(130, false);
    sim.drive(131, false);
    sim.drive(4, false);
    assert_eq!(pending(&gpio), [4, 130]);
    sim.drive(131, true);
    sim.drive(4, true);
    assert_eq!(pending(&gpio), [4, 131]);
    assert_eq!(pending(&gpio), []);
    // A mask and unmask write the pending bits as 0, keeping them.
    sim.drive(130, true);
    sim.drive(130, false);
    gpio.mask(130);
    gpio.unmask(130);
    assert_eq!(pending(&gpio), [130]);
}

#[test]
fn amd_quiet_turns_off_what_the_firmware_left_on() {
    let sim = AmdSim::new();
    sim.set(5, IDLE | INTERRUPT_ENABLE | INTERRUPT_MASK | WAKE | INTERRUPT_STATUS);
    sim.set(63, 0x0000_8000);
    let gpio = Controller::amd(sim.mmio()).unwrap();
    gpio.quiet();
    assert_eq!(sim.pin(5), IDLE);
    assert_eq!(sim.pin(63), 0x0000_8000);
    assert_eq!(sim.ends(), 1);
    assert_eq!(pending(&gpio), []);
}

#[test]
fn amd_refuses_missing_pins_and_absent_controllers() {
    let sim = AmdSim::new();
    let gpio = Controller::amd(sim.mmio()).unwrap();
    assert_eq!(gpio.set_interrupt(63, Trigger::LevelLow), Err(Error::NoPin));
    assert_eq!(gpio.set_interrupt(184, Trigger::LevelLow), Err(Error::NoPin));
    assert_eq!(gpio.level(200), Err(Error::NoPin));
    sim.set(0, u32::MAX);
    assert!(matches!(Controller::amd(sim.mmio()), Err(Unusable::Absent)));
    let short = Mmio::fake(Arc::new(AmdSim::new()), 0x100);
    assert!(matches!(Controller::amd(short), Err(Unusable::Registers)));
}

const PADBAR: u32 = 0x400;
const RX_STATE: u32 = 1 << 1;
const TX_DISABLE: u32 = 1 << 8;
const RX_DISABLE: u32 = 1 << 9;
const RX_INVERT: u32 = 1 << 23;
const EDGE: u32 = 1 << 25;

/// A Sunrise Point-LP community: its registers by word, and which pads
/// are locked.
struct Community {
    words: Vec<u32>,
    locked: Vec<bool>,
}

struct IntelState {
    communities: Vec<Community>,
}

const FIRST: [usize; 3] = [0, 48, 120];

impl IntelState {
    fn new() -> IntelState {
        let communities = [48, 72, 32]
            .iter()
            .map(|&pads| {
                let mut words = vec![0; 0x800 / 4];
                words[0x0c / 4] = PADBAR;
                // Every pad the host's and in the GPIO driver's mode, as
                // an input with its output buffer on.
                for group in 0..3 {
                    words[0xd0 / 4 + group] = 0x00ff_ffff;
                }
                for pad in 0..pads {
                    words[(PADBAR as usize + pad * 8) / 4] = RX_STATE;
                }
                Community { words, locked: vec![false; pads] }
            })
            .collect();
        IntelState { communities }
    }

    fn place(pin: usize) -> (usize, usize) {
        let community = FIRST.iter().rposition(|&first| pin >= first).unwrap();
        (community, pin - FIRST[community])
    }

    fn dw0(&mut self, pin: usize) -> &mut u32 {
        let (community, pad) = IntelState::place(pin);
        &mut self.communities[community].words[(PADBAR as usize + pad * 8) / 4]
    }

    /// Sets the status of a pad in the driver's mode whose level is
    /// active, or that saw its edge.
    fn event(&mut self, community: usize, pad: usize, edge_seen: bool) {
        let words = &mut self.communities[community].words;
        let dw0 = words[(PADBAR as usize + pad * 8) / 4];
        let (group, bit) = (pad / 24, 1 << (pad % 24));
        if words[0xd0 / 4 + group] & bit == 0 {
            return;
        }
        let active = (dw0 & RX_STATE != 0) != (dw0 & RX_INVERT != 0);
        let event = if dw0 & EDGE != 0 { edge_seen } else { active };
        if event {
            words[0x100 / 4 + group] |= bit;
        }
    }

    fn drive(&mut self, pin: usize, high: bool) {
        let dw0 = self.dw0(pin);
        let before = (*dw0 & RX_STATE != 0) != (*dw0 & RX_INVERT != 0);
        *dw0 = if high { *dw0 | RX_STATE } else { *dw0 & !RX_STATE };
        let after = (*dw0 & RX_STATE != 0) != (*dw0 & RX_INVERT != 0);
        let (community, pad) = IntelState::place(pin);
        self.event(community, pad, !before && after);
    }
}

#[derive(Clone)]
struct IntelSim(Arc<Mutex<IntelState>>);

/// One community's registers.
struct CommunityView(IntelSim, usize);

impl Registers for CommunityView {
    fn read(&self, offset: usize, size: usize) -> u64 {
        assert_eq!(size, 4);
        self.0.0.lock().unwrap().communities[self.1].words[offset / 4].into()
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 4);
        let value = value as u32;
        let mut state = self.0.0.lock().unwrap();
        let community = &mut state.communities[self.1];
        let word = offset / 4;
        match offset {
            0x0c => {}
            0x100..0x10c => community.words[word] &= !value,
            0x400.. => {
                let pad = (offset - 0x400) / 8;
                if offset.is_multiple_of(8) && !community.locked[pad] {
                    let rx = community.words[word] & RX_STATE;
                    community.words[word] = (value & !RX_STATE) | rx;
                }
            }
            _ => community.words[word] = value,
        }
        // Levels still active set their status again.
        let pads = community.locked.len();
        for pad in 0..pads {
            state.event(self.1, pad, false);
        }
    }
}

impl IntelSim {
    fn new() -> IntelSim {
        IntelSim(Arc::new(Mutex::new(IntelState::new())))
    }

    fn ranges(&self) -> Vec<Mmio> {
        (0..3)
            .map(|community| Mmio::fake(Arc::new(CommunityView(self.clone(), community)), 0x800))
            .collect()
    }

    fn drive(&self, pin: usize, high: bool) {
        self.0.lock().unwrap().drive(pin, high);
    }

    fn dw0(&self, pin: usize) -> u32 {
        *self.0.lock().unwrap().dw0(pin)
    }

    fn word(&self, community: usize, offset: usize) -> u32 {
        self.0.lock().unwrap().communities[community].words[offset / 4]
    }

    fn set_word(&self, community: usize, offset: usize, value: u32) {
        self.0.lock().unwrap().communities[community].words[offset / 4] = value;
    }
}

#[test]
fn an_intel_pad_level_low_interrupts_and_again_while_still_low() {
    let sim = IntelSim::new();
    let gpio = Controller::sunrise_point_lp(&sim.ranges()).unwrap();
    assert_eq!((gpio.name(), gpio.pins()), ("Intel Sunrise Point-LP's", 152));
    // Pad 53: community 1, its first group, bit 5.
    gpio.set_interrupt(53, Trigger::LevelLow).unwrap();
    let dw0 = sim.dw0(53);
    assert_eq!(dw0 & (RX_INVERT | 3 << 25 | RX_DISABLE | TX_DISABLE), RX_INVERT | TX_DISABLE);
    sim.drive(53, false);
    assert_eq!(pending(&gpio), []);
    gpio.unmask(53);
    assert_eq!(pending(&gpio), [53]);
    gpio.mask(53);
    assert_eq!(pending(&gpio), []);
    gpio.unmask(53);
    assert_eq!(pending(&gpio), [53]);
    sim.drive(53, true);
    gpio.mask(53);
    gpio.unmask(53);
    assert_eq!(pending(&gpio), []);
    assert_eq!(gpio.level(53), Ok(true));
}

#[test]
fn intel_pads_are_numbered_across_communities_and_interrupt_on_edges() {
    let sim = IntelSim::new();
    let gpio = Controller::sunrise_point_lp(&sim.ranges()).unwrap();
    gpio.set_interrupt(0, Trigger::FallingEdge).unwrap();
    gpio.set_interrupt(151, Trigger::RisingEdge).unwrap();
    gpio.set_interrupt(119, Trigger::LevelHigh).unwrap();
    assert_eq!(sim.dw0(0) & (RX_INVERT | EDGE), RX_INVERT | EDGE);
    assert_eq!(sim.dw0(151) & (RX_INVERT | EDGE), EDGE);
    for pin in [0, 151, 119] {
        gpio.unmask(pin);
    }
    // Pad 119 is high, its level active.
    assert_eq!(pending(&gpio), [119]);
    sim.drive(119, false);
    sim.drive(0, false);
    sim.drive(151, false);
    assert_eq!(pending(&gpio), [0, 119]);
    sim.drive(151, true);
    assert_eq!(pending(&gpio), [151]);
    assert_eq!(pending(&gpio), []);
    // Pad 151, GPP_G7: community 2's second group, bit 7.
    assert_eq!(sim.word(2, 0x124), 1 << 7);
}

#[test]
fn intel_refuses_pads_the_firmware_keeps_sets_up_otherwise_or_locks() {
    let sim = IntelSim::new();
    let gpio = Controller::sunrise_point_lp(&sim.ranges()).unwrap();
    // Pad 10's owner is the Management Engine.
    sim.set_word(0, 0x24, 1 << 8);
    assert_eq!(gpio.set_interrupt(10, Trigger::LevelLow), Err(Error::Firmware));
    // Pad 30 is in the ACPI firmware's mode.
    sim.set_word(0, 0xd4, 0x00ff_ffff & !(1 << 6));
    assert_eq!(gpio.set_interrupt(30, Trigger::LevelLow), Err(Error::Firmware));
    // Pad 60 is in a native function.
    *sim.0.lock().unwrap().dw0(60) |= 1 << 10;
    assert_eq!(gpio.set_interrupt(60, Trigger::LevelLow), Err(Error::Function));
    // Pad 70 is locked as an edge's.
    *sim.0.lock().unwrap().dw0(70) |= EDGE;
    sim.0.lock().unwrap().communities[1].locked[22] = true;
    assert_eq!(gpio.set_interrupt(70, Trigger::LevelLow), Err(Error::Locked));
    // Locked as asked, it is used.
    *sim.0.lock().unwrap().dw0(70) |= TX_DISABLE;
    assert_eq!(gpio.set_interrupt(70, Trigger::RisingEdge), Ok(()));
    assert_eq!(gpio.set_interrupt(71, Trigger::BothEdges), Err(Error::Trigger(Trigger::BothEdges)));
    assert_eq!(gpio.set_interrupt(152, Trigger::LevelLow), Err(Error::NoPin));
}

#[test]
fn intel_quiet_and_controllers_that_are_not_there_or_not_as_known() {
    let sim = IntelSim::new();
    sim.set_word(1, 0x124, 0xff);
    let gpio = Controller::sunrise_point_lp(&sim.ranges()).unwrap();
    gpio.quiet();
    // Every interrupt off; the statuses of levels that are active latch
    // again, and are not pending.
    assert_eq!(sim.word(1, 0x124), 0);
    assert_eq!(pending(&gpio), []);
    let ranges = sim.ranges();
    assert!(matches!(Controller::sunrise_point_lp(&ranges[..2]), Err(Unusable::Registers)));
    sim.set_word(2, 0x0c, 0x7f0);
    assert!(matches!(Controller::sunrise_point_lp(&ranges), Err(Unusable::Layout)));
    sim.set_word(2, 0x0c, u32::MAX);
    assert!(matches!(Controller::sunrise_point_lp(&ranges), Err(Unusable::Absent)));
}
