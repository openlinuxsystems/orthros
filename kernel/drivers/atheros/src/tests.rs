// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated chip: registers that act as FreeBSD's
//! `alc(4)` expects them to, a PHY with debug and extended registers
//! behind the MDIO register, a cable the tests plug in and pull out, and
//! a wire whose frames the tests put in and take out through the rings
//! in (fake) DMA memory. A clock moves a microsecond each time it is
//! read, and as far as a test moves it.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Dma, Mmio, Registers, fake_memory, fake_memory_write};
use radix_nic::{Error, Interrupt, Link, Nic, PAGE};

use crate::{Card, Pci, VENDOR, device, identify, intr, mac, queue, reg, start};

const MAC: [u8; 6] = [0x00, 0x13, 0x74, 0x01, 0x02, 0x03];
const FALLBACK: [u8; 6] = [0x02, 0, 0, 0, 0, 1];
/// Where the simulated chip's PCI Express capability is.
const EXPRESS: usize = 0x40;
const LEN: usize = 0x2000;
const RING: usize = 256;
const BUFFER: usize = 1536;
const SECOND: u64 = 1_000_000_000;

const MDIO_EXECUTE: u32 = 0x0080_0000;
const MDIO_READ: u32 = 0x0020_0000;
const MDIO_BUSY: u32 = 0x0800_0000;
const MDIO_EXT: u32 = 0x4000_0000;
const MDIO_CLOCK: u32 = 0x0700_0000;
const RRD_VALID: u32 = 0x8000_0000;
const RRD_ERR_SUM: u32 = 0x0010_0000;
const RRD_ERR_CRC: u32 = 0x0020_0000;
const RRD_ERR_LENGTH: u32 = 0x4000_0000;
const TD_EOP: u32 = 0x8000_0000;

thread_local! {
    static NOW: Cell<u64> = const { Cell::new(0) };
}

fn clock() -> u64 {
    NOW.with(|now| {
        let then = now.get() + 1_000;
        now.set(then);
        then
    })
}

fn advance(ns: u64) {
    NOW.with(|now| now.set(now.get() + ns));
}

fn alloc(pages: usize) -> Option<Dma> {
    Some(Dma::fake(pages * PAGE))
}

fn no_delay(_us: u32) {}

fn read_mem(phys: u64, len: usize) -> Vec<u8> {
    let mut out = std::vec![0u8; len];
    fake_memory(phys, &mut out);
    out
}

fn u32_at(phys: u64) -> u32 {
    u32::from_le_bytes(read_mem(phys, 4).try_into().unwrap())
}

fn u64_at(phys: u64) -> u64 {
    u64::from_le_bytes(read_mem(phys, 8).try_into().unwrap())
}

struct State {
    regs: Vec<u8>,
    ar816x: bool,
    /// The address the EEPROM holds, which a load puts in PAR0 and PAR1.
    eeprom: [u8; 6],
    phy: [u16; 32],
    debug: HashMap<u16, u16>,
    debug_address: u16,
    ext: HashMap<(u32, u16), u16>,
    /// The link the cable gives once the PHY negotiates: Mb/s and duplex.
    cable: Option<(u32, bool)>,
    negotiated: bool,
    negotiations: u32,
    rx_next: usize,
    next_return: usize,
    tx_index: usize,
    tx_producer: usize,
    sent: Vec<Vec<u8>>,
    /// Every write: register, size, value.
    writes: Vec<(usize, usize, u64)>,
    /// Every MDIO command.
    mdio: Vec<u32>,
    /// Every debug register write, in order.
    debug_writes: Vec<(u16, u16)>,
    resets: u32,
}

struct Fake {
    state: Mutex<State>,
}

impl Fake {
    fn new(ar816x: bool, eeprom: [u8; 6]) -> Arc<Fake> {
        let mut state = State {
            regs: std::vec![0; LEN],
            ar816x,
            eeprom,
            phy: [0; 32],
            debug: HashMap::new(),
            debug_address: 0,
            ext: HashMap::new(),
            cable: None,
            negotiated: false,
            negotiations: 0,
            rx_next: 0,
            next_return: 0,
            tx_index: 0,
            tx_producer: 0,
            sent: Vec::new(),
            writes: Vec::new(),
            mdio: Vec::new(),
            debug_writes: Vec::new(),
            resets: 0,
        };
        // An EEPROM is there, a 512-byte read request and 256-byte
        // payloads, L0s and L1 allowed with a 128-byte completion
        // boundary and extended sync on, and a 12 KiB receive FIFO.
        Fake::set(&mut state, reg::MASTER_CFG, 4, 0x8000_0000);
        Fake::set(&mut state, reg::TWSI_DEBUG, 4, 0x2000_0000);
        Fake::set(&mut state, EXPRESS + 0x08, 2, (2 << 12) | (1 << 5));
        Fake::set(&mut state, EXPRESS + 0x0c, 4, 0x0c00);
        Fake::set(&mut state, EXPRESS + 0x10, 2, 0x0003 | 0x0008 | 0x0080);
        Fake::set(&mut state, reg::SRAM_RX_FIFO_LEN, 4, 0x600);
        Fake::set(&mut state, reg::MISC, 4, 0x0000_1000);
        Arc::new(Fake { state: Mutex::new(state) })
    }

    fn get(state: &State, offset: usize, size: usize) -> u64 {
        let mut bytes = [0u8; 8];
        bytes[..size].copy_from_slice(&state.regs[offset..offset + size]);
        u64::from_le_bytes(bytes)
    }

    fn set(state: &mut State, offset: usize, size: usize, value: u64) {
        state.regs[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
    }

    fn or(state: &mut State, offset: usize, bits: u32) {
        let value = Fake::get(state, offset, 4) as u32 | bits;
        Fake::set(state, offset, 4, u64::from(value));
    }

    fn ring(state: &State, lo: usize) -> u64 {
        Fake::get(state, lo, 4) | (Fake::get(state, reg::RX_BASE_ADDR_HI, 4) << 32)
    }

    fn reset(state: &mut State) {
        Fake::set(state, reg::MBOX_RD0_PROD_IDX, 4, 0);
        Fake::set(state, reg::INTR_STATUS, 4, 0);
        Fake::set(state, reg::MAC_CFG, 4, 0);
        Fake::set(state, reg::RXQ_CFG, 4, 0);
        Fake::set(state, reg::TXQ_CFG, 4, 0);
        state.rx_next = 0;
        state.next_return = 0;
        state.tx_index = 0;
        state.tx_producer = 0;
        state.resets += 1;
    }

    fn phy_read(state: &State, phy: u32) -> u16 {
        let linked = state.negotiated && state.cable.is_some();
        match phy {
            // 10 and 100 Mb/s both ways, extended status, autonegotiation.
            0x01 => 0x7909 | if linked { 0x0004 } else { 0 },
            0x11 => match state.cable {
                Some((mbps, full)) if state.negotiated => {
                    let speed = match mbps {
                        1000 => 0x8000,
                        100 => 0x4000,
                        _ => 0,
                    };
                    0x0800 | 0x0400 | speed | if full { 0x2000 } else { 0 }
                }
                _ => 0,
            },
            0x1e => state.debug.get(&state.debug_address).copied().unwrap_or(0),
            _ => state.phy[phy as usize],
        }
    }

    fn phy_write(state: &mut State, phy: u32, value: u16) {
        match phy {
            0x1d => state.debug_address = value,
            0x1e => {
                state.debug.insert(state.debug_address, value);
                state.debug_writes.push((state.debug_address, value));
            }
            0x00 if value & 0x8000 != 0 => {
                state.phy[0] = value & !0x8000;
                if value & 0x1200 == 0x1200 {
                    state.negotiated = true;
                    state.negotiations += 1;
                }
            }
            _ => state.phy[phy as usize] = value,
        }
    }

    fn mdio(state: &mut State, command: u32) {
        state.mdio.push(command);
        if command & MDIO_EXECUTE == 0 {
            return;
        }
        let read = command & MDIO_READ != 0;
        let data = command as u16;
        let result = if command & MDIO_EXT != 0 {
            let select = Fake::get(state, reg::EXT_MDIO, 4) as u32;
            let key = ((select >> 16) & 0x1f, select as u16);
            if read {
                state.ext.get(&key).copied().unwrap_or(0)
            } else {
                state.ext.insert(key, data);
                0
            }
        } else {
            let phy = (command >> 16) & 0x1f;
            if read {
                Fake::phy_read(state, phy)
            } else {
                Fake::phy_write(state, phy, data);
                0
            }
        };
        let done = command & !(MDIO_EXECUTE | MDIO_BUSY | 0xffff) | u32::from(result);
        Fake::set(state, reg::MDIO, 4, u64::from(done));
    }

    fn transmit(state: &mut State) {
        let txq = Fake::get(state, reg::TXQ_CFG, 4) as u32;
        let mac = Fake::get(state, reg::MAC_CFG, 4) as u32;
        if txq & queue::TXQ_ENB == 0 || mac & mac::TX_ENB == 0 {
            return;
        }
        let base = Fake::get(state, reg::TDL_HEAD_ADDR_LO, 4)
            | (Fake::get(state, reg::TX_BASE_ADDR_HI, 4) << 32);
        while state.tx_index != state.tx_producer {
            let at = base + (state.tx_index * 16) as u64;
            let len = (u32_at(at) & 0x3fff) as usize;
            assert_eq!(u32_at(at + 4), TD_EOP, "one buffer a frame, no offloads");
            state.sent.push(read_mem(u64_at(at + 8), len));
            state.tx_index = (state.tx_index + 1) % RING;
            Fake::or(state, reg::INTR_STATUS, intr::TX_PKT0);
        }
    }

    /// Puts `frame` on the wire to the chip, with its frame check
    /// sequence and `errors` in its return descriptor, in as many buffers
    /// as it needs: whether the chip took it.
    fn deliver(&self, frame: &[u8], errors: u32) -> bool {
        let mut state = self.state.lock().unwrap();
        let rxq = Fake::get(&state, reg::RXQ_CFG, 4) as u32;
        let mac = Fake::get(&state, reg::MAC_CFG, 4) as u32;
        if rxq & queue::RXQ_QUEUE0_ENB == 0 || mac & mac::RX_ENB == 0 {
            return false;
        }
        let mut data = frame.to_vec();
        data.extend_from_slice(&[0xfc; 4]);
        let buffers = data.len().div_ceil(BUFFER);
        let producer = Fake::get(&state, reg::MBOX_RD0_PROD_IDX, 2) as usize;
        let receive = Fake::ring(&state, reg::RD0_HEAD_ADDR_LO);
        let returned = Fake::ring(&state, reg::RRD0_HEAD_ADDR_LO);
        let rrd = returned + (state.next_return * 16) as u64;
        if u32_at(rrd + 12) & RRD_VALID != 0 {
            return false;
        }
        // The chip stops one short of the producer.
        let mut index = state.rx_next;
        for _ in 0..buffers {
            if index == producer {
                return false;
            }
            index = (index + 1) % RING;
        }
        let first = state.rx_next;
        for (n, piece) in data.chunks(BUFFER).enumerate() {
            let buffer = u64_at(receive + (((first + n) % RING) * 8) as u64);
            fake_memory_write(buffer, piece);
        }
        let info = ((first as u32) << 20) | ((buffers as u32) << 16);
        fake_memory_write(rrd, &info.to_le_bytes());
        let status = RRD_VALID | errors | data.len() as u32;
        fake_memory_write(rrd + 12, &status.to_le_bytes());
        state.next_return = (state.next_return + 1) % RING;
        state.rx_next = (first + buffers) % RING;
        Fake::or(&mut state, reg::INTR_STATUS, intr::RX_PKT0);
        true
    }

    fn cable(&self, link: Option<(u32, bool)>) {
        self.state.lock().unwrap().cable = link;
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.state.lock().unwrap().sent)
    }

    fn reg(&self, offset: usize) -> u32 {
        Fake::get(&self.state.lock().unwrap(), offset, 4) as u32
    }

    fn raise(&self, bits: u32) {
        Fake::or(&mut self.state.lock().unwrap(), reg::INTR_STATUS, bits);
    }

    fn phy(&self, phy: u32) -> u16 {
        Fake::phy_read(&self.state.lock().unwrap(), phy)
    }

    fn debug(&self, at: u16) -> Option<u16> {
        self.state.lock().unwrap().debug.get(&at).copied()
    }

    fn debug_writes(&self, at: u16) -> Vec<u16> {
        let state = self.state.lock().unwrap();
        state.debug_writes.iter().filter(|&&(a, _)| a == at).map(|&(_, v)| v).collect()
    }

    fn ext(&self, device: u32, at: u16) -> Option<u16> {
        self.state.lock().unwrap().ext.get(&(device, at)).copied()
    }

    fn writes_to(&self, offset: usize) -> Vec<u64> {
        let state = self.state.lock().unwrap();
        state.writes.iter().filter(|&&(o, ..)| o == offset).map(|&(.., v)| v).collect()
    }

    fn mdio_commands(&self) -> Vec<u32> {
        core::mem::take(&mut self.state.lock().unwrap().mdio)
    }

    fn negotiations(&self) -> u32 {
        self.state.lock().unwrap().negotiations
    }
}

impl Registers for Fake {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let state = self.state.lock().unwrap();
        match (offset, size) {
            (reg::IDLE_STATUS, _) => 0,
            (reg::MBOX_TD_CONS_IDX, 4) => (state.tx_index as u64) << 16,
            (reg::MBOX_TD_PRI0_CONS_IDX, 2) => state.tx_index as u64,
            _ => Fake::get(&state, offset, size),
        }
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut state = self.state.lock().unwrap();
        state.writes.push((offset, size, value));
        let word = value as u32;
        match offset {
            reg::MASTER_CFG => {
                Fake::set(&mut state, offset, 4, u64::from(word & !1));
                if word & 1 != 0 {
                    Fake::reset(&mut state);
                }
            }
            reg::INTR_STATUS => {
                let status = Fake::get(&state, offset, 4) as u32 & !word;
                Fake::set(&mut state, offset, 4, u64::from(status));
            }
            reg::MDIO => Fake::mdio(&mut state, word),
            reg::TWSI_CFG if word & 0x0800 != 0 => {
                // TWSI on the AR813x, the station address loader on the
                // AR816x: the address loaded, the bit cleared.
                let eeprom = state.eeprom;
                Fake::set(
                    &mut state,
                    reg::PAR0,
                    4,
                    u64::from(u32::from_be_bytes([eeprom[2], eeprom[3], eeprom[4], eeprom[5]])),
                );
                Fake::set(
                    &mut state,
                    reg::PAR1,
                    4,
                    u64::from(u16::from_be_bytes([eeprom[0], eeprom[1]])),
                );
                Fake::set(&mut state, offset, 4, u64::from(word & !0x0800));
            }
            reg::MBOX_TD_PROD_IDX if size == 4 && !state.ar816x => {
                state.tx_producer = (word >> 16) as usize;
                Fake::transmit(&mut state);
            }
            reg::MBOX_TD_PRI0_PROD_IDX if size == 2 && state.ar816x => {
                state.tx_producer = word as usize;
                Fake::transmit(&mut state);
            }
            reg::TXQ_CFG | reg::MAC_CFG => {
                Fake::set(&mut state, offset, size, value);
                Fake::transmit(&mut state);
            }
            _ => Fake::set(&mut state, offset, size, value),
        }
    }
}

fn pci(revision: u8) -> Pci {
    Pci { revision, subsystem: (0x1028, 0x0001), express: Some(EXPRESS) }
}

fn started(id: u16, revision: u8, interrupt: Interrupt) -> (Arc<Fake>, Card) {
    let model = identify(VENDOR, id).unwrap();
    let fake = Fake::new(model.ar816x(), MAC);
    let regs = Mmio::fake(fake.clone(), LEN);
    let card =
        start(model, pci(revision), regs, interrupt, FALLBACK, &mut alloc, &mut no_delay, clock)
            .unwrap();
    (fake, card)
}

/// Plugs the cable in at `mbps`, full duplex, and lets the driver see it.
fn plug(fake: &Fake, card: &mut Card, mbps: u32) {
    fake.cable(Some((mbps, true)));
    advance(SECOND);
    let mut frame = [0u8; 2048];
    assert_eq!(card.receive(&mut frame), None);
}

fn frame(n: usize, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i + n) as u8).collect()
}

#[test]
fn identifies_atheros_and_killer_cards_by_their_pci_ids() {
    let ar8161 = identify(VENDOR, device::AR8161).unwrap();
    assert_eq!(ar8161.name, "Atheros AR8161");
    assert!(ar8161.ar816x() && ar8161.msi());
    assert_eq!(ar8161.article(), "an");
    let killer = identify(VENDOR, device::E2500).unwrap();
    assert_eq!((killer.name, killer.article()), ("Killer E2500", "a"));
    assert!(killer.ar816x());
    assert!(!identify(VENDOR, device::AR8131).unwrap().ar816x());
    assert_eq!(identify(VENDOR, 0x1026), None, "the L1, an older chip of its own");
    assert_eq!(identify(0x10ec, device::AR8161), None);
}

#[test]
fn start_sets_the_ar813x_up_as_freebsd_does() {
    let (fake, card) = started(device::AR8131, 0xc0, Interrupt::Msi);
    let info = card.info();
    assert_eq!((info.model, info.mac), ("Atheros AR8131", MAC));
    assert_eq!((info.receive_slots, info.transmit_slots), (255, 255));
    // The address programmed back, the rings and their sizes.
    assert_eq!(fake.reg(reg::PAR0), 0x7401_0203);
    assert_eq!(fake.reg(reg::PAR1), 0x0013);
    assert_ne!(fake.reg(reg::TDL_HEAD_ADDR_LO), 0);
    assert_ne!(fake.reg(reg::RD0_HEAD_ADDR_LO), 0);
    assert_ne!(fake.reg(reg::RRD0_HEAD_ADDR_LO), 0);
    for count in [reg::TD_RING_CNT, reg::RD_RING_CNT, reg::RRD_RING_CNT] {
        assert_eq!(fake.reg(count), 256);
    }
    assert_eq!(fake.reg(reg::RX_BUF_SIZE), 1536);
    assert_eq!(fake.reg(reg::MBOX_RD0_PROD_IDX), 255, "every buffer the chip's but one");
    assert_eq!(fake.writes_to(reg::RD1_HEAD_ADDR_LO), [0]);
    assert_eq!(fake.reg(reg::DMA_BLOCK), 1);
    assert_eq!(fake.reg(reg::FRAME_SIZE), 9 * 1024);
    // The MSI for frames and errors, nothing pending.
    assert_eq!(fake.reg(reg::INTR_MASK), intr::RAISED);
    assert_eq!(fake.reg(reg::INTR_STATUS), 0);
    // Bursts of 512 bytes read and 256 written, 128-byte completions,
    // the statistics block off.
    let dma = fake.reg(reg::DMA_CFG);
    assert_eq!((dma >> 4) & 7, 2);
    assert_eq!((dma >> 7) & 7, 1);
    assert_ne!(dma & queue::DMA_RCB_128, 0);
    assert_ne!(dma & queue::DMA_SMB_DIS, 0);
    assert_eq!(dma & (queue::DMA_CMB_ENB | queue::DMA_SMB_ENB), 0);
    assert_eq!(fake.reg(reg::TXQ_CFG) >> 16, 512);
    // XON at 80% of the FIFO and XOFF at 30%.
    assert_eq!(fake.reg(reg::RX_FIFO_PAUSE_THRESH), (0x600 * 3 / 10) | ((0x600 * 8 / 10) << 16));
    // Broadcasts and every multicast frame, CRC and padding, gigabit, and
    // the MAC still off: the link is not up.
    let mac = fake.reg(reg::MAC_CFG);
    let wanted = mac::BCAST | mac::ALLMULTI | mac::TX_CRC_ENB | mac::TX_AUTO_PAD | mac::SPEED_1000;
    assert_eq!(mac & wanted, wanted);
    assert_eq!(mac & (mac::TX_ENB | mac::RX_ENB | mac::PROMISC | mac::VLAN_TAG_STRIP), 0);
    assert_eq!((fake.reg(reg::MAR0), fake.reg(reg::MAR1)), (u32::MAX, u32::MAX));
    // The PHY offers every speed and negotiates.
    assert_eq!(fake.phy(0x04), 0x01e1);
    assert_eq!(fake.phy(0x09), 0x0300);
    assert_eq!(fake.negotiations(), 1);
    assert_eq!(card.link(), Link::default());
    // The power states the BIOS allowed are off until the link is up.
    assert_eq!(fake.reg(reg::PM_CFG) & 0x1008, 0);
}

#[test]
fn the_ar813x_phy_gets_the_vendors_values() {
    let (fake, _card) = started(device::AR8151, 0xc0, Interrupt::Msi);
    assert_eq!(fake.debug(0x12), Some(0x4c04));
    assert_eq!(fake.debug(0x05), Some(0x2c42));
    assert_eq!(fake.debug(0x36), Some(0xe12c));
    assert_eq!(fake.debug(0x04), Some(0x88bb));
    assert_eq!(fake.debug(0x00).map(|value| value & 0xff7f), Some(0x026f));
    // The AR8151's own value, then hibernation off, then atphy's zero.
    assert_eq!(fake.debug_writes(0x29), [0x929d, 0x129d, 0]);
    assert_eq!(fake.debug(0x0b).unwrap_or(0) & 0x8000, 0);
    // The PHY's MAC power down off, crossover and polarity automatic.
    assert_eq!(fake.phy(0x10), 0x0060 | 0x0800 | 0x0002);
}

#[test]
fn the_ar816x_is_set_up_its_own_way() {
    // An AR8161 of revision B0.
    let (fake, _card) = started(device::AR8161, 2 << 3, Interrupt::Msi);
    assert_eq!(fake.reg(reg::CLK_GATING_CFG), 0x3f);
    assert_eq!(fake.reg(reg::IDLE_DECISN_TIMER), 0x400);
    assert!(fake.writes_to(reg::RD1_HEAD_ADDR_LO).is_empty());
    // The receive producer written as 16 bits, the reset's workaround.
    assert_eq!(fake.writes_to(reg::MBOX_RD0_PROD_IDX).last(), Some(&255));
    // Its over-current protection restored, its SerDes clocks slowed.
    assert_eq!(fake.reg(reg::MISC) & 0x00e0_0000, 7 << 21);
    assert_eq!(fake.reg(reg::SERDES_LOCK) & 0x0006_0000, 0x0006_0000);
    // Four read channels, no write bursts, the queues' priorities.
    let dma = fake.reg(reg::DMA_CFG);
    assert_eq!(dma & 0x0c00_0000, queue::DMA_RD_CHNL_SEL_4);
    assert_eq!((dma >> 7) & 7, 0);
    assert_eq!(fake.reg(reg::WRR), 0x6000_0000 | 0x0404_0404);
    assert_eq!(fake.reg(reg::HQTD_CFG), 0x8000_0555);
    // XON from the FIFO's 12 KiB, less what FreeBSD keeps.
    assert_eq!(fake.reg(reg::RX_FIFO_PAUSE_THRESH), ((0x600 * 8 - 3212) / 8) | ((3212 / 8) << 16));
    // The PHY's hibernation and energy-efficient Ethernet off, and the
    // vendor's values.
    assert_eq!(fake.debug(0x0b), Some(0x2c40));
    assert_eq!(fake.debug(0x3b), Some(0x7078));
    assert_eq!(fake.debug(0x00), Some(0x02ef));
    assert_eq!(fake.ext(7, 0x3c), Some(0));
    assert_eq!(fake.ext(7, 0x8027), Some(0x8a05));
    assert_eq!(fake.reg(reg::LPI_CTL) & 1, 0);
    // L1 alone while the running chip's link is down.
    let pm = fake.reg(reg::PM_CFG);
    assert_eq!(pm & (0x1008 | 0x4000_0000), 0x0008 | 0x4000_0000);
    assert_eq!((pm >> 16) & 7, 4, "the L1 timer, shifted to its place");
}

#[test]
fn the_mac_is_turned_on_once_the_link_is_up() {
    let (fake, mut card) = started(device::AR8131, 0xc0, Interrupt::Msi);
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(card.link(), Link::default());
    // Plugged in: nothing until the driver looks again, a second on.
    fake.cable(Some((1000, true)));
    assert_eq!(card.receive(&mut buffer), None);
    assert!(!card.link().up);
    advance(SECOND);
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(card.link(), Link { up: true, mbps: 1000, full_duplex: true });
    let mac = fake.reg(reg::MAC_CFG);
    assert_eq!(mac & (mac::TX_ENB | mac::RX_ENB), mac::TX_ENB | mac::RX_ENB);
    assert_eq!(mac & mac::SPEED_MASK, mac::SPEED_1000);
    assert_ne!(mac & mac::FULL_DUPLEX, 0);
    assert_eq!(fake.reg(reg::RXQ_CFG) & queue::RXQ_ENB, queue::RXQ_QUEUE0_ENB);
    assert_ne!(fake.reg(reg::TXQ_CFG) & queue::TXQ_ENB, 0);
    // The AR8131's power states stay off with its link up, as FreeBSD
    // leaves them; its SerDes may sleep in L1.
    let pm = fake.reg(reg::PM_CFG);
    assert_eq!(pm & 0x1008, 0);
    assert_eq!(pm & 0xb0, 0xb0);
    // Down again at 100 Mb/s, half duplex: the MAC follows.
    fake.cable(Some((100, false)));
    fake.state.lock().unwrap().negotiated = true;
    advance(SECOND);
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(card.link(), Link { up: true, mbps: 100, full_duplex: false });
    let mac = fake.reg(reg::MAC_CFG);
    assert_eq!(mac & mac::SPEED_MASK, mac::SPEED_10_100);
    assert_eq!(mac & mac::FULL_DUPLEX, 0);
    fake.cable(None);
    advance(SECOND);
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(card.link(), Link::default());
    assert_eq!(fake.reg(reg::MAC_CFG) & (mac::TX_ENB | mac::RX_ENB), 0);
    assert_eq!(fake.reg(reg::TXQ_CFG) & queue::TXQ_ENB, 0);
}

#[test]
fn a_fast_chip_offers_no_gigabit_and_takes_no_gigabit_link() {
    let (fake, mut card) = started(device::AR8162, 0x08, Interrupt::Msi);
    assert_eq!(fake.phy(0x09), 0);
    assert_eq!(fake.reg(reg::MAC_CFG) & mac::SPEED_MASK, mac::SPEED_10_100);
    plug(&fake, &mut card, 1000);
    assert!(!card.link().up);
    plug(&fake, &mut card, 100);
    assert_eq!(card.link(), Link { up: true, mbps: 100, full_duplex: true });
}

#[test]
fn the_ar816x_reads_its_phy_slowly_once_its_link_is_up() {
    let (fake, mut card) = started(device::AR8171, 0x10, Interrupt::Msi);
    let _ = fake.mdio_commands();
    plug(&fake, &mut card, 1000);
    // Looked at with the fast clock while the link was down.
    assert!(fake.mdio_commands().iter().all(|command| command & MDIO_CLOCK == 0));
    advance(SECOND);
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), None);
    let commands = fake.mdio_commands();
    assert!(!commands.is_empty());
    assert!(commands.iter().all(|command| command & MDIO_CLOCK == MDIO_CLOCK));
}

#[test]
fn received_frames_lose_their_check_sequence() {
    let (fake, mut card) = started(device::AR8131, 0xc0, Interrupt::Msi);
    plug(&fake, &mut card, 1000);
    let sent = [frame(1, 60), frame(2, 1514), frame(3, 1518)];
    for frame in &sent {
        assert!(fake.deliver(frame, 0));
    }
    let mut buffer = [0u8; 2048];
    for frame in &sent {
        let len = card.receive(&mut buffer).unwrap();
        assert_eq!(&buffer[..len], &frame[..]);
    }
    assert_eq!(card.receive(&mut buffer), None);
    // The three buffers given back once nothing was left.
    assert_eq!(fake.reg(reg::MBOX_RD0_PROD_IDX), 2);
    assert_eq!(card.dropped(), 0);
}

#[test]
fn damaged_and_long_frames_are_dropped_and_their_buffers_given_back() {
    let (fake, mut card) = started(device::AR8131, 0xc0, Interrupt::Msi);
    plug(&fake, &mut card, 1000);
    assert!(fake.deliver(&frame(1, 100), RRD_ERR_SUM | RRD_ERR_CRC));
    // A jumbo frame, in two buffers.
    assert!(fake.deliver(&frame(2, 2000), 0));
    // A length that is not its protocol's is the stack's to find.
    assert!(fake.deliver(&frame(3, 100), RRD_ERR_SUM | RRD_ERR_LENGTH));
    assert!(fake.deliver(&frame(4, 10), 0));
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), Some(100));
    assert_eq!(&buffer[..100], &frame(3, 100)[..]);
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(card.dropped(), 3);
    assert_eq!(fake.reg(reg::MBOX_RD0_PROD_IDX), 4, "five buffers back");
}

#[test]
fn the_receive_ring_goes_round() {
    let (fake, mut card) = started(device::AR8161, 0x10, Interrupt::Polled);
    plug(&fake, &mut card, 1000);
    let mut buffer = [0u8; 2048];
    for n in 0..1000 {
        assert!(fake.deliver(&frame(n, 64 + n % 1400), 0), "frame {n}");
        assert_eq!(card.receive(&mut buffer), Some(64 + n % 1400));
        assert_eq!(&buffer[..64 + n % 1400], &frame(n, 64 + n % 1400)[..]);
    }
    assert_eq!(card.receive(&mut buffer), None);
    // A full ring holds the chip's frames back until buffers come back.
    let mut taken = 0;
    while fake.deliver(&frame(0, 64), 0) {
        taken += 1;
    }
    assert_eq!(taken, 255);
    while card.receive(&mut buffer).is_some() {}
    assert!(fake.deliver(&frame(0, 64), 0));
}

#[test]
fn frames_are_sent_once_flushed_and_wait_for_the_link() {
    let (fake, mut card) = started(device::AR8131, 0xc0, Interrupt::Msi);
    assert!(card.send(&frame(1, 42)));
    card.flush();
    assert!(fake.sent().is_empty(), "the queue is off until the link is up");
    plug(&fake, &mut card, 1000);
    assert_eq!(fake.sent(), [frame(1, 42)]);
    assert!(card.send(&frame(2, 1514)));
    assert!(card.send(&frame(3, 60)));
    assert!(fake.sent().is_empty());
    card.flush();
    assert_eq!(fake.sent(), [frame(2, 1514), frame(3, 60)]);
    assert!(!card.send(&[0u8; 13]));
    assert!(!card.send(&[0u8; 1519]));
}

#[test]
fn a_full_transmit_ring_takes_no_more_until_the_chip_sends() {
    let (fake, mut card) = started(device::AR8172, 0x10, Interrupt::Msi);
    for n in 0..255 {
        assert!(card.can_send());
        assert!(card.send(&frame(n, 64)));
    }
    assert!(!card.can_send());
    assert!(!card.send(&frame(0, 64)));
    card.flush();
    plug(&fake, &mut card, 100);
    assert_eq!(fake.sent().len(), 255);
    assert!(card.can_send());
    for n in 0..300 {
        assert!(card.send(&frame(n, 64)));
        card.flush();
    }
    assert_eq!(fake.sent().len(), 300);
}

#[test]
fn the_interrupt_is_acknowledged_once_nothing_is_left() {
    let (fake, mut card) = started(device::AR8131, 0xc0, Interrupt::Msi);
    plug(&fake, &mut card, 1000);
    assert!(fake.deliver(&frame(1, 100), 0));
    assert!(fake.deliver(&frame(2, 100), 0));
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), Some(100));
    assert_ne!(fake.reg(reg::INTR_STATUS) & intr::RX_PKT0, 0);
    assert_eq!(card.receive(&mut buffer), Some(100));
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(fake.reg(reg::INTR_STATUS), 0);
    // A failed DMA is left for failed() to see.
    fake.raise(intr::DMA_WR_TO_RST | intr::TX_PKT0);
    assert!(card.failed());
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(fake.reg(reg::INTR_STATUS), intr::DMA_WR_TO_RST);
    assert!(card.failed());
}

#[test]
fn a_polled_card_raises_nothing() {
    let (fake, mut card) = started(device::AR8131, 0xc0, Interrupt::Polled);
    assert_eq!(fake.reg(reg::INTR_MASK), 0);
    plug(&fake, &mut card, 1000);
    assert!(fake.deliver(&frame(1, 100), 0));
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), Some(100));
    assert!(!card.failed());
}

#[test]
fn the_phy_negotiates_again_after_a_while_without_a_link() {
    let (fake, mut card) = started(device::AR8131, 0xc0, Interrupt::Msi);
    let mut buffer = [0u8; 2048];
    for _ in 0..17 {
        assert_eq!(card.receive(&mut buffer), None);
        advance(SECOND);
    }
    assert_eq!(fake.negotiations(), 2);
    // Looked at once a second, not on every call.
    let _ = fake.mdio_commands();
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(card.receive(&mut buffer), None);
    assert!(fake.mdio_commands().len() <= 4);
}

#[test]
fn a_chip_without_an_address_gets_the_fallback() {
    let model = identify(VENDOR, device::E2400).unwrap();
    let fake = Fake::new(true, [0; 6]);
    let regs = Mmio::fake(fake.clone(), LEN);
    let card =
        start(model, pci(0x10), regs, Interrupt::Msi, FALLBACK, &mut alloc, &mut no_delay, clock)
            .unwrap();
    assert_eq!(card.info().mac, FALLBACK);
    assert_eq!(fake.reg(reg::PAR0), 0x0000_0001);
    assert_eq!(fake.reg(reg::PAR1), 0x0200);
}

#[test]
fn gone_cards_and_small_windows_are_refused() {
    let model = identify(VENDOR, device::AR8131).unwrap();
    let fake = Fake::new(false, MAC);
    Fake::set(&mut fake.state.lock().unwrap(), reg::MASTER_CFG, 4, u64::from(u32::MAX));
    let regs = Mmio::fake(fake.clone(), LEN);
    let err =
        start(model, pci(0), regs, Interrupt::Msi, FALLBACK, &mut alloc, &mut no_delay, clock);
    assert!(matches!(err, Err(Error::Device(_))));
    let small = Mmio::fake(Fake::new(false, MAC), 0x1000);
    let err =
        start(model, pci(0), small, Interrupt::Msi, FALLBACK, &mut alloc, &mut no_delay, clock);
    assert!(matches!(err, Err(Error::Registers)));
}

#[test]
fn stopping_turns_interrupts_queues_and_the_mac_off() {
    let (fake, mut card) = started(device::AR8131, 0xc0, Interrupt::Msi);
    plug(&fake, &mut card, 1000);
    card.stop();
    assert_eq!(fake.reg(reg::INTR_MASK), 0);
    assert_eq!(fake.reg(reg::MAC_CFG) & (mac::TX_ENB | mac::RX_ENB), 0);
    assert_eq!(fake.reg(reg::RXQ_CFG) & queue::RXQ_ENB, 0);
    assert_eq!(fake.reg(reg::TXQ_CFG) & queue::TXQ_ENB, 0);
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), None);
    assert!(!card.send(&frame(1, 64)));
    assert!(!card.can_send());
    let resets = fake.state.lock().unwrap().resets;
    card.stop();
    drop(card);
    assert_eq!(fake.state.lock().unwrap().resets, resets);
}

#[test]
fn the_ar8151_keeps_extended_sync_off_once_its_link_is_up() {
    let (fake, mut card) = started(device::AR8151_V2, 0xc0, Interrupt::Msi);
    plug(&fake, &mut card, 1000);
    let control = Fake::get(&fake.state.lock().unwrap(), EXPRESS + 0x10, 2) as u16;
    assert_eq!(control & 0x0080, 0);
    let pm = fake.reg(reg::PM_CFG);
    assert_ne!(pm & 0x2000, 0, "its clock switched in L1");
    assert_eq!((pm >> 16) & 0xf, 4, "the AR8151 v2's L1 timer at a gigabit");
}
