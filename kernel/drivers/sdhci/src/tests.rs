// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated SD host controller and card: what the
//! card says of itself, reads and writes through ADMA2's descriptors of
//! both sizes, failures and the stop after them.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Dma, Mmio, Registers};

use crate::*;

/// A simulated card.
#[derive(Clone)]
struct SimCard {
    data: Vec<u8>,
    /// It answers CMD8.
    version2: bool,
    /// It is SDHC: addressed in blocks.
    high: bool,
    write_protected: bool,
}

/// A simulated slot and its card.
struct State {
    regs: [u8; 256],
    card: Option<SimCard>,
    /// The card's state.
    app: bool,
    tries: u32,
    rca: u16,
    selected: bool,
    block_length: u32,
    /// The commands it got, in order (an ACMD as 100 + its index).
    commands: Vec<u8>,
    /// The next data command fails with these error bits.
    fail: Option<u16>,
    /// Addresses below 4 GiB that stand for host memory, for 32-bit
    /// descriptors: (low, host).
    aliases: Vec<(u32, u64)>,
}

struct Sim(Mutex<State>);

const CID_NAME: &[u8; 5] = b"TEST1";

impl State {
    fn get(&self, at: usize, size: usize) -> u64 {
        let mut value = 0u64;
        for i in (0..size).rev() {
            value = value << 8 | u64::from(self.regs[at + i]);
        }
        value
    }

    fn put(&mut self, at: usize, size: usize, value: u64) {
        for i in 0..size {
            self.regs[at + i] = (value >> (8 * i)) as u8;
        }
    }

    fn set_status(&mut self, normal: u16, error: u16) {
        let now = self.get(reg::NORMAL_STATUS, 2) as u16;
        self.put(reg::NORMAL_STATUS, 2, u64::from(now | normal));
        let now = self.get(reg::ERROR_STATUS, 2) as u16;
        self.put(reg::ERROR_STATUS, 2, u64::from(now | error));
    }

    fn respond(&mut self, words: [u32; 4]) {
        for (i, word) in words.iter().enumerate() {
            self.put(reg::RESPONSE + 4 * i, 4, u64::from(*word));
        }
    }

    /// A 128-bit register as an R2 response: its bits 127 to 8.
    fn long(register: u128) -> [u32; 4] {
        let shifted = register >> 8;
        core::array::from_fn(|i| (shifted >> (32 * i)) as u32)
    }

    fn cid() -> u128 {
        let mut cid = 0x03u128 << 120 | u128::from(u16::from_be_bytes(*b"SD")) << 104;
        for (i, &c) in CID_NAME.iter().enumerate() {
            cid |= u128::from(c) << (96 - 8 * i);
        }
        cid | 0x10 << 56 | 0xdead_beef << 24 | (24 << 4 | 6) << 8 | 1
    }

    fn csd(card: &SimCard) -> u128 {
        let blocks = card.data.len() as u128 / 512;
        if card.high {
            1 << 126 | (blocks / 1024 - 1) << 48 | 1
        } else {
            // 512-byte blocks, C_SIZE_MULT 7: 256 KiB for each C_SIZE.
            let size = blocks * 512 / (256 * 1024) - 1;
            9 << 80 | size << 62 | 7 << 47 | 1
        }
    }

    /// The host memory a descriptor's address stands for.
    fn host(&self, address: u64, wide: bool) -> u64 {
        if wide {
            return address;
        }
        let within =
            |&&(low, _): &&(u32, u64)| (u64::from(low)..u64::from(low) + 4096).contains(&address);
        let (low, host) = self.aliases.iter().find(within).unwrap();
        host + (address - u64::from(*low))
    }

    /// Moves a data command's blocks through the descriptors.
    fn transfer(&mut self, index: u8, argument: u32) -> Result<(), u16> {
        if let Some(bits) = self.fail.take() {
            return Err(bits);
        }
        let card = self.card.clone().unwrap();
        let wide = self.regs[reg::HOST_CONTROL] >> 3 & 3 == 3;
        let size = if wide { 12 } else { 8 };
        let mut table = self.get(reg::ADMA_ADDRESS, 4);
        if wide {
            table |= self.get(reg::ADMA_ADDRESS + 4, 4) << 32;
        }
        let table = self.host(table, wide);
        let count = self.get(reg::BLOCK_COUNT, 2) as usize;
        let multiple = matches!(index, 18 | 25);
        let blocks = if multiple { count } else { 1 };
        let start = if card.high { argument as usize * 512 } else { argument as usize };
        let total = blocks * 512;
        if start + total > card.data.len() {
            return Err(1 << 4);
        }
        let mut done = 0;
        let mut at = table;
        loop {
            let mut entry = [0u8; 12];
            radix_hw::fake_memory(at, &mut entry[..size]);
            let attributes = u16::from_le_bytes([entry[0], entry[1]]);
            if attributes & 1 == 0 {
                return Err(1 << 9);
            }
            let len = usize::from(u16::from_le_bytes([entry[2], entry[3]]));
            let mut address = u64::from(u32::from_le_bytes(entry[4..8].try_into().unwrap()));
            if wide {
                address |= u64::from(u32::from_le_bytes(entry[8..12].try_into().unwrap())) << 32;
            }
            let host = self.host(address, wide);
            if done + len > total {
                return Err(1 << 9);
            }
            let place = start + done;
            if matches!(index, 17 | 18) {
                radix_hw::fake_memory_write(host, &card.data[place..place + len]);
            } else {
                let mut bytes = std::vec![0u8; len];
                radix_hw::fake_memory(host, &mut bytes);
                self.card.as_mut().unwrap().data[place..place + len].copy_from_slice(&bytes);
            }
            done += len;
            if attributes & 2 != 0 {
                break;
            }
            at += size as u64;
        }
        if done == total { Ok(()) } else { Err(1 << 9) }
    }

    /// The command just written.
    fn command(&mut self, value: u16) {
        let index = (value >> 8 & 0x3f) as u8;
        let argument = self.get(reg::ARGUMENT, 4) as u32;
        let app = core::mem::take(&mut self.app);
        self.commands.push(if app { 100 + index } else { index });
        let powered = self.regs[reg::POWER_CONTROL] & 1 != 0;
        let clocked = self.get(reg::CLOCK_CONTROL, 2) & 4 != 0;
        let Some(card) = self.card.clone().filter(|_| powered && clocked) else {
            self.set_status(status::ERROR, status::COMMAND_TIMEOUT);
            return;
        };
        let timeout = |state: &mut State| state.set_status(status::ERROR, status::COMMAND_TIMEOUT);
        let r1 = |state: &mut State, bits: u32| state.respond([bits | 0x900, 0, 0, 0]);
        let mut busy = false;
        match (app, index) {
            (_, 0) => {
                self.tries = 0;
                self.rca = 0;
                self.selected = false;
            }
            (false, 8) if card.version2 => self.respond([argument & 0xfff, 0, 0, 0]),
            (false, 8) => return timeout(self),
            (false, 55) => {
                self.app = true;
                r1(self, 1 << 5);
            }
            (true, 41) => {
                self.tries += 1;
                let mut ocr = 0x00ff_8000;
                if self.tries >= 3 {
                    ocr |= 1 << 31;
                    if card.high && argument & 1 << 30 != 0 {
                        ocr |= 1 << 30;
                    }
                }
                self.respond([ocr, 0, 0, 0]);
            }
            (false, 2) => self.respond(Self::long(Self::cid())),
            (false, 3) => {
                self.rca = 0xb368;
                self.respond([u32::from(self.rca) << 16 | 0x500, 0, 0, 0]);
            }
            (false, 9) => self.respond(Self::long(Self::csd(&card))),
            (false, 7) => {
                self.selected = argument >> 16 == u32::from(self.rca);
                r1(self, 0);
                busy = true;
            }
            (true, 6) => r1(self, 0),
            (false, 16) => {
                self.block_length = argument;
                r1(self, 0);
            }
            (false, 12) => {
                r1(self, 0);
                busy = true;
            }
            (false, 17 | 18 | 24 | 25) if self.selected => {
                let writes = matches!(index, 24 | 25);
                if writes && card.write_protected {
                    r1(self, 1 << 26);
                } else if let Err(bits) = self.transfer(index, argument) {
                    r1(self, 0);
                    self.set_status(status::COMMAND_COMPLETE | status::ERROR, bits);
                    return;
                } else {
                    r1(self, 0);
                }
                busy = true;
            }
            _ => return timeout(self),
        }
        let mut normal = status::COMMAND_COMPLETE;
        if busy {
            normal |= status::TRANSFER_COMPLETE;
        }
        self.set_status(normal, 0);
    }
}

impl Registers for Sim {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let state = self.0.lock().unwrap();
        if offset == reg::PRESENT_STATE {
            let mut value = 0u64;
            if let Some(card) = &state.card {
                value |= 0x7 << 16;
                if !card.write_protected {
                    value |= 1 << 19;
                }
            }
            return value;
        }
        state.get(offset, size)
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut state = self.0.lock().unwrap();
        match offset {
            reg::NORMAL_STATUS | reg::ERROR_STATUS => {
                let now = state.get(offset, size);
                state.put(offset, size, now & !value);
            }
            reg::SOFTWARE_RESET => {
                if value & 1 != 0 {
                    let keep: Vec<u8> = state.regs[0x40..0x50].to_vec();
                    let version = state.get(reg::VERSION, 2);
                    state.regs = [0; 256];
                    state.regs[0x40..0x50].copy_from_slice(&keep);
                    state.put(reg::VERSION, 2, version);
                }
            }
            reg::CLOCK_CONTROL => {
                let stable = if value & 1 != 0 { 2 } else { 0 };
                state.put(offset, size, value | stable);
            }
            reg::COMMAND => {
                state.put(offset, size, value);
                state.command(value as u16);
            }
            _ => state.put(offset, size, value),
        }
    }
}

/// A slot of a controller of `version`, `base_mhz` and, with `wide`, 64-bit
/// DMA, holding `card`.
fn slot(version: u8, base_mhz: u64, wide: bool, card: Option<SimCard>) -> (Slot, Arc<Sim>) {
    let mut state = State {
        regs: [0; 256],
        card,
        app: false,
        tries: 0,
        rca: 0,
        selected: false,
        block_length: 0,
        commands: Vec::new(),
        fail: None,
        aliases: Vec::new(),
    };
    let caps = base_mhz << 8 | 1 << 19 | 1 << 21 | 1 << 24 | if wide { 1 << 28 } else { 0 };
    state.put(reg::CAPABILITIES, 8, caps);
    state.put(reg::VERSION, 2, u64::from(version));
    let sim = Arc::new(Sim(Mutex::new(state)));
    let regs = Mmio::fake(sim.clone(), REGISTERS);
    // The table's fake address is a host one, above 4 GiB: its low half
    // stands for it in 32-bit descriptors.
    let mut tables = Vec::new();
    let mut alloc = |pages: usize, _low: bool| {
        let dma = Dma::fake(pages * 4096);
        tables.push(dma.phys());
        Some(dma)
    };
    let slot = Slot::new(regs, &mut alloc, &mut |_| {}).unwrap();
    let aliases = tables.iter().map(|&phys| (phys as u32, phys));
    sim.0.lock().unwrap().aliases.extend(aliases);
    (slot, sim)
}

fn card(blocks: usize, high: bool) -> SimCard {
    SimCard { data: std::vec![0; blocks * 512], version2: true, high, write_protected: false }
}

/// A pattern of `len` bytes for block `block` on.
fn pattern(block: usize, len: usize) -> Vec<u8> {
    (0..len).map(|i| ((block * 512 + i) * 7 % 251) as u8).collect()
}

#[test]
fn sdhc_card_is_set_up_and_moves_blocks_with_wide_descriptors() {
    let (mut slot, sim) = slot(version::V3, 200, true, Some(card(4096, true)));
    assert!(slot.present() && slot.reaches_64());
    assert_eq!(slot.version(), (3, 0));
    let card = slot.identify(&mut |_| {}).unwrap();
    assert!(card.high_capacity);
    assert_eq!((card.blocks, card.rca, card.width, card.read_only), (4096, 0xb368, 4, false));
    assert_eq!(card.name(), "TEST1");
    assert_eq!(card.kind(), "SDHC");
    assert_eq!((card.cid.maker, card.cid.oem, card.cid.serial), (3, *b"SD", 0xdead_beef));
    assert_eq!((card.cid.year, card.cid.month, card.cid.revision), (2024, 6, 0x10));
    // 200 MHz over 2 × 4.
    assert_eq!(card.khz, 25_000);
    let commands = sim.0.lock().unwrap().commands.clone();
    assert_eq!(commands, [0, 8, 55, 141, 55, 141, 55, 141, 2, 3, 9, 7, 55, 106]);
    // Three buffers of five blocks, written from block 100, then read back.
    let pages: Vec<Dma> = (0..3).map(|_| Dma::fake(4096)).collect();
    let lens = [1024u32, 512, 1024];
    for (i, page) in pages.iter().enumerate() {
        page.write(0, &pattern(100 + i * 2, lens[i] as usize));
    }
    let buffers: Vec<(u64, u32)> = pages.iter().zip(lens).map(|(p, l)| (p.phys(), l)).collect();
    slot.start(Op::Write, 100, 5, &buffers, &mut |_| {}).unwrap();
    assert_eq!(slot.poll(), Some(Ok(())));
    assert_eq!(slot.poll(), None);
    let data = sim.0.lock().unwrap().card.as_ref().unwrap().data[100 * 512..102 * 512].to_vec();
    assert_eq!(data, pattern(100, 1024));
    let back = Dma::fake(4096);
    slot.start(Op::Read, 101, 2, &[(back.phys(), 1024)], &mut |_| {}).unwrap();
    assert_eq!(slot.poll(), Some(Ok(())));
    let mut got = std::vec![0u8; 1024];
    back.read(0, &mut got);
    let mut want = pattern(101, 512);
    want.extend(pattern(102, 512));
    assert_eq!(got, want);
    // One block: CMD17.
    slot.start(Op::Read, 4095, 1, &[(back.phys(), 512)], &mut |_| {}).unwrap();
    assert_eq!(slot.poll(), Some(Ok(())));
    assert_eq!(sim.0.lock().unwrap().commands.last(), Some(&17));
}

#[test]
fn standard_capacity_cards_are_addressed_in_bytes_through_narrow_descriptors() {
    // A version 2.00 controller of 52 MHz: 13 MHz is the most at 25.
    let (mut slot, sim) = slot(version::V2, 52, false, Some(card(8192, false)));
    assert!(!slot.reaches_64());
    let card = slot.identify(&mut |_| {}).unwrap();
    assert!(!card.high_capacity);
    assert_eq!((card.blocks, card.khz, card.kind()), (8192, 13_000, "SD"));
    assert_eq!(sim.0.lock().unwrap().block_length, 512);
    // Host memory above 4 GiB is refused; below, the simulator maps it.
    let page = Dma::fake(4096);
    assert_eq!(
        slot.start(Op::Write, 0, 1, &[(page.phys(), 512)], &mut |_| {}),
        if page.phys() + 512 > 1 << 32 { Err(Error::BadRequest) } else { Ok(()) }
    );
    let _ = slot.poll();
    sim.0.lock().unwrap().aliases.push((0x1000_0000, page.phys()));
    page.write(0, &pattern(7, 1024));
    slot.start(Op::Write, 7, 2, &[(0x1000_0000, 1024)], &mut |_| {}).unwrap();
    assert_eq!(slot.poll(), Some(Ok(())));
    let state = sim.0.lock().unwrap();
    assert_eq!(state.get(reg::ARGUMENT, 4), 7 * 512);
    assert_eq!(state.card.as_ref().unwrap().data[7 * 512..9 * 512], pattern(7, 1024));
}

#[test]
fn version_one_cards_do_not_answer_cmd8() {
    let mut old = card(2048, false);
    old.version2 = false;
    let (mut slot, sim) = slot(version::V3, 100, true, Some(old));
    let card = slot.identify(&mut |_| {}).unwrap();
    assert!(!card.high_capacity);
    assert_eq!(card.blocks, 2048);
    // ACMD41 without HCS.
    assert!(sim.0.lock().unwrap().commands.starts_with(&[0, 8, 55, 141]));
}

#[test]
fn empty_slots_and_cards_of_other_kinds() {
    let (mut slot, _) = slot(version::V3, 200, true, None);
    assert!(!slot.present());
    assert_eq!(slot.identify(&mut |_| {}), Err(Error::NoCard));
    let page = Dma::fake(4096);
    assert_eq!(slot.start(Op::Read, 0, 1, &[(page.phys(), 512)], &mut |_| {}), Err(Error::NoCard));
    // Without ADMA2, the slot is not driven.
    let mut state = State {
        regs: [0; 256],
        card: None,
        app: false,
        tries: 0,
        rca: 0,
        selected: false,
        block_length: 0,
        commands: Vec::new(),
        fail: None,
        aliases: Vec::new(),
    };
    state.put(reg::CAPABILITIES, 8, 200 << 8 | 1 << 22);
    let sim = Arc::new(Sim(Mutex::new(state)));
    let mut alloc = |_: usize, _: bool| Some(Dma::fake(4096));
    let made = Slot::new(Mmio::fake(sim, REGISTERS), &mut alloc, &mut |_| {});
    assert!(matches!(made, Err(Error::Unsupported(_))));
}

#[test]
fn bad_requests_are_refused() {
    let (mut slot, _) = slot(version::V3, 200, true, Some(card(1024, true)));
    slot.identify(&mut |_| {}).unwrap();
    let page = Dma::fake(4096);
    let p = page.phys();
    let mut no_wait = |_| {};
    // Not the blocks' bytes, past the end, nothing, too many, and a
    // second while one is in flight.
    assert_eq!(slot.start(Op::Read, 0, 2, &[(p, 512)], &mut no_wait), Err(Error::BadRequest));
    assert_eq!(slot.start(Op::Read, 1023, 2, &[(p, 1024)], &mut no_wait), Err(Error::BadRequest));
    assert_eq!(slot.start(Op::Read, 0, 0, &[], &mut no_wait), Err(Error::BadRequest));
    let many = std::vec![(p, 4); MAX_BUFFERS + 1];
    assert_eq!(slot.start(Op::Read, 0, 1, &many, &mut no_wait), Err(Error::BadRequest));
    assert_eq!(slot.start(Op::Read, 0, 1, &[(p + 2, 512)], &mut no_wait), Err(Error::BadRequest));
}

#[test]
fn failures_stop_the_card_before_the_next_request() {
    let (mut slot, sim) = slot(version::V3, 200, true, Some(card(1024, true)));
    slot.identify(&mut |_| {}).unwrap();
    let page = Dma::fake(4096);
    // A CRC error on the data.
    sim.0.lock().unwrap().fail = Some(1 << 5);
    slot.start(Op::Read, 0, 4, &[(page.phys(), 2048)], &mut |_| {}).unwrap();
    let failed = slot.poll();
    assert_eq!(failed, Some(Err(Error::Command(1 << 5))));
    assert_eq!(Error::Command(1 << 5).as_str(), "the card's data was garbled (CRC)");
    // The next request stops the card first (CMD12), and works.
    slot.start(Op::Read, 0, 4, &[(page.phys(), 2048)], &mut |_| {}).unwrap();
    assert_eq!(slot.poll(), Some(Ok(())));
    let commands = sim.0.lock().unwrap().commands.clone();
    assert_eq!(commands[commands.len() - 3..], [18, 12, 18]);
}

#[test]
fn write_protected_cards_are_read_only() {
    let mut locked = card(1024, true);
    locked.write_protected = true;
    let (mut slot, _) = slot(version::V3, 200, true, Some(locked));
    let card = slot.identify(&mut |_| {}).unwrap();
    assert!(card.read_only);
    let page = Dma::fake(4096);
    assert_eq!(
        slot.start(Op::Write, 0, 1, &[(page.phys(), 512)], &mut |_| {}),
        Err(Error::BadRequest)
    );
    slot.start(Op::Read, 0, 1, &[(page.phys(), 512)], &mut |_| {}).unwrap();
    assert_eq!(slot.poll(), Some(Ok(())));
}

#[test]
fn csd_and_cid_are_read() {
    // A 32 GB SDXC card's CSD 2.0: C_SIZE 0xEDC7 (59,592 MiB / 2 + ...).
    let csd = State::long(1 << 126 | 0xedc7u128 << 48);
    assert_eq!(Csd::parse(&csd), Csd::High((0xedc7 + 1) * 1024));
    let standard = State::csd(&card(8192, false));
    assert_eq!(Csd::parse(&State::long(standard)), Csd::Standard(8192));
    assert_eq!(Csd::parse(&State::long(2 << 126)), Csd::Ultra);
    assert_eq!(Csd::parse(&State::long(3 << 126)), Csd::Unknown);
    let cid = Cid::parse(&State::long(State::cid()));
    assert_eq!(&cid.name, CID_NAME);
    let big = Card {
        rca: 1,
        high_capacity: true,
        blocks: 64 * 1_000_000_000 / 512,
        cid,
        khz: 25_000,
        width: 4,
        read_only: false,
    };
    assert_eq!(big.kind(), "SDXC");
}
