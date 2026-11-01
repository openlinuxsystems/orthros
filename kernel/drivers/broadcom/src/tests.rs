// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated chip: a configuration space with the
//! chip's IDs and its window into the chip's memory, registers that act
//! as FreeBSD's `bge(4)` expects them to, a bootcode that answers the
//! reset's handshake, a PHY behind the MDIO register with a cable the
//! tests plug in and pull out, an APE whose firmware hands out its locks
//! and takes the driver's events, and a wire whose frames the tests put
//! in and take out through the rings and the status block in (fake) DMA
//! memory. A clock moves a microsecond each time it is read, and as far
//! as a test moves it.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::pci::{Address, CONFIG_SIZE, Function};
use radix_hw::{Dma, Mmio, Registers, fake_memory, fake_memory_write};
use radix_nic::{Error, Interrupt, Link, Nic, PAGE};

use crate::reg::{self, ape, hcc, mac_mode, mi, mode_ctl, pci, rcb, rx_mode, sram, tx_mode};
use crate::{Card, Model, VENDOR, asic, chipid, device, identify, start};

const MAC: [u8; 6] = [0x00, 0x10, 0x18, 0x01, 0x02, 0x03];
const REGISTER_MAC: [u8; 6] = [0x00, 0x10, 0x18, 0x0a, 0x0b, 0x0c];
const FALLBACK: [u8; 6] = [0x02, 0, 0, 0, 0, 1];
/// Where the simulated function's MSI and PCI Express capabilities are.
const MSI_CAP: usize = 0x58;
const EXPRESS: usize = 0xac;
const SECOND: u64 = 1_000_000_000;

const STD_RING: usize = 512;
const RX_DESCRIPTOR: u64 = 32;
const TX_DESCRIPTOR: u64 = 16;
const BD_END: u32 = 0x0004;
const RX_ERROR: u32 = 0x0400;
const RX_JUMBO: u32 = 0x0020;
const STATUS_UPDATED: u32 = 0x0000_0001;
const STATUS_LINK_CHANGED: u32 = 0x0000_0002;
/// The bit the APE's firmware holds a lock with.
const FIRMWARE: u32 = 0x8000;

/// The PHY's registers the tests look at.
const BMCR: u16 = 0x00;
const ANAR: u16 = 0x04;
const GTCR: u16 = 0x09;
const EXTCTL: u16 = 0x10;
const DSP_RW_PORT: u16 = 0x15;
const DSP_ADDR: u16 = 0x17;
const AUXCTL: u16 = 0x18;
const IMR: u16 = 0x1b;

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

fn write32_at(phys: u64, value: u32) {
    fake_memory_write(phys, &value.to_le_bytes());
}

fn write16_at(phys: u64, value: u16) {
    fake_memory_write(phys, &value.to_le_bytes());
}

fn get(bytes: &[u8], offset: usize, size: usize) -> u64 {
    let mut word = [0u8; 8];
    word[..size].copy_from_slice(&bytes[offset..offset + size]);
    u64::from_le_bytes(word)
}

fn put(bytes: &mut [u8], offset: usize, size: usize, value: u64) {
    bytes[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
}

/// The simulated chip, as a test sets it up.
#[derive(Clone, Copy)]
struct Setup {
    device: u16,
    /// What the product ID register its model names says.
    chipid: u32,
    /// What the high half of MISC_CTL says: 0xf000 when the chip's ID
    /// is in a product ID register.
    misc_id: u16,
    function: u8,
    subsystem: u16,
    /// Whether the APE's firmware runs, on a chip that has an APE.
    ape_running: bool,
    /// Whether the bootcode answers the reset's handshake.
    bootcode: bool,
    /// The address the bootcode left in the chip's memory.
    sram_mac: Option<[u8; 6]>,
    /// What the MAC's first address registers say.
    register_mac: [u8; 6],
    serdes: bool,
    fibre: bool,
    /// What the read DMA's length registers say.
    rdma_lengths: u32,
    express: bool,
}

fn setup(device: u16, chipid: u32) -> Setup {
    Setup {
        device,
        chipid,
        misc_id: 0xf000,
        function: 0,
        subsystem: 0x103c,
        ape_running: false,
        bootcode: true,
        sram_mac: Some(MAC),
        register_mac: REGISTER_MAC,
        serdes: false,
        fibre: false,
        rdma_lengths: 0x05ee_05ee,
        express: true,
    }
}

fn bcm5720() -> Setup {
    setup(device::BCM5720, 0x0572_0000)
}

fn bcm5761() -> Setup {
    setup(device::BCM5761, 0x0576_1100)
}

fn bcm57765() -> Setup {
    setup(device::BCM57765, 0x5778_5100)
}

fn bcm57780() -> Setup {
    setup(device::BCM57780, 0x5778_0001)
}

struct State {
    config: Vec<u8>,
    regs: Vec<u8>,
    ape: Vec<u8>,
    /// The chip's memory, by word.
    sram: HashMap<usize, u32>,
    misc_id: u32,
    bootcode: bool,
    /// Whether the chip has gone from the bus: it reads all ones.
    gone: bool,
    resets: u32,
    phy_address: u16,
    phy: [u16; 32],
    /// Every PHY write: register and value.
    phy_writes: Vec<(u16, u16)>,
    mdio: u32,
    /// The link the cable gives once the PHY negotiates: Mb/s and duplex.
    cable: Option<(u32, bool)>,
    negotiated: bool,
    negotiations: u32,
    /// The next standard descriptor the chip fills, the next return
    /// descriptor it writes, the next send descriptor it sends.
    std_next: usize,
    rx_produced: usize,
    tx_index: usize,
    /// Whether the chip sends nothing it is handed.
    tx_stuck: bool,
    sent: Vec<Vec<u8>>,
    tag: u32,
    /// Every write to the registers: offset and value.
    writes: Vec<(usize, u32)>,
    /// The APE's locks: who holds each.
    grants: [u32; 8],
    /// Every lock request: lock and bit; and whether the BCM5761's lock
    /// registers were used.
    requests: Vec<(usize, u32)>,
    old_locks: bool,
    /// Every event the APE's firmware took.
    events: Vec<u32>,
}

struct Fake {
    state: Mutex<State>,
}

impl Fake {
    fn new(setup: &Setup, model: &Model) -> Arc<Fake> {
        let mut config = std::vec![0u8; CONFIG_SIZE];
        put(&mut config, 0x00, 2, u64::from(VENDOR));
        put(&mut config, 0x02, 2, u64::from(setup.device));
        put(&mut config, 0x06, 2, 0x0010);
        put(&mut config, 0x2c, 2, u64::from(setup.subsystem));
        put(&mut config, 0x34, 1, MSI_CAP as u64);
        put(&mut config, MSI_CAP, 1, 0x05);
        if setup.express {
            put(&mut config, MSI_CAP + 1, 1, EXPRESS as u64);
            put(&mut config, EXPRESS, 1, 0x10);
            // 256-byte payloads; relaxed ordering and no snoop on, 512-byte
            // read requests, and errors left from before.
            put(&mut config, EXPRESS + 4, 4, 0x0001);
            put(&mut config, EXPRESS + 8, 2, 0x2810);
            put(&mut config, EXPRESS + 0xa, 2, 0x0009);
        }
        put(&mut config, model.product_id, 4, u64::from(setup.chipid));
        put(&mut config, pci::CMD, 2, 0x0016);
        put(&mut config, pci::CACHESZ, 4, 0x10);

        let mut regs = std::vec![0u8; reg::END];
        let [a, b, c, d, e, f] = setup.register_mac;
        put(&mut regs, reg::MAC_ADDR1_LO, 4, u64::from(u16::from_be_bytes([a, b])));
        put(&mut regs, reg::MAC_ADDR1_HI, 4, u64::from(u32::from_be_bytes([c, d, e, f])));
        // What a driver before this one left: the MAC in gigabit, the
        // MSI's one shot off.
        put(&mut regs, reg::MAC_MODE, 4, u64::from(mac_mode::PORTMODE_GMII));
        put(&mut regs, reg::MSI_MODE, 4, u64::from(reg::msi_mode::ONE_SHOT_DISABLE));
        for channel in 0..reg::rdma::CHANNELS {
            put(&mut regs, reg::RDMA_LENGTH + 4 * channel, 4, u64::from(setup.rdma_lengths));
        }
        if setup.serdes {
            put(&mut regs, reg::SGDIG_STS, 4, u64::from(reg::SGDIGSTS_IS_SERDES));
            put(&mut regs, reg::CPMU_PHY_STRAP, 4, u64::from(reg::CPMU_PHY_STRAP_IS_SERDES));
        }

        let mut ape = std::vec![0u8; 0x1_0000];
        if setup.ape_running {
            put(&mut ape, ape::SEG_SIG, 4, u64::from(ape::SEG_SIG_MAGIC));
            put(&mut ape, ape::FW_STATUS, 4, u64::from(ape::FW_STATUS_READY));
        }

        let mut sram = HashMap::new();
        if let Some([a, b, c, d, e, f]) = setup.sram_mac {
            let high = sram::MAC_ADDR_SIG << 16 | u32::from(u16::from_be_bytes([a, b]));
            sram.insert(sram::MAC_ADDR_HIGH_MB, high);
            sram.insert(sram::MAC_ADDR_LOW_MB, u32::from_be_bytes([c, d, e, f]));
        }
        if setup.fibre {
            sram.insert(sram::DATA_SIG, sram::DATA_SIG_MAGIC);
            sram.insert(sram::DATA_CFG, sram::MEDIA_FIBER);
        }
        let mut phy = [0u16; 32];
        phy[usize::from(BMCR)] = 0x1140;
        phy[usize::from(EXTCTL)] = 0x0102;
        let phy_address = if [0x5717, 0x5719, 0x5720].contains(&(setup.chipid >> 12)) {
            u16::from(setup.function) + 1
        } else {
            1
        };
        Arc::new(Fake {
            state: Mutex::new(State {
                config,
                regs,
                ape,
                sram,
                misc_id: u32::from(setup.misc_id),
                bootcode: setup.bootcode,
                gone: false,
                resets: 0,
                phy_address,
                phy,
                phy_writes: Vec::new(),
                mdio: 0,
                cable: None,
                negotiated: false,
                negotiations: 0,
                std_next: 0,
                rx_produced: 0,
                tx_index: 0,
                tx_stuck: false,
                sent: Vec::new(),
                tag: 0,
                writes: Vec::new(),
                grants: [0; 8],
                requests: Vec::new(),
                old_locks: false,
                events: Vec::new(),
            }),
        })
    }

    fn reg(state: &State, offset: usize) -> u32 {
        get(&state.regs, offset, 4) as u32
    }

    fn set_reg(state: &mut State, offset: usize, value: u32) {
        put(&mut state.regs, offset, 4, u64::from(value));
    }

    fn sram(state: &State, at: usize) -> u32 {
        state.sram.get(&at).copied().unwrap_or(0)
    }

    /// The physical address in the ring control block at `at` in the
    /// chip's memory, and its ring's size.
    fn rcb(state: &State, at: usize) -> (u64, usize) {
        let high = u64::from(Fake::sram(state, at + rcb::HOSTADDR_HI));
        let low = u64::from(Fake::sram(state, at + rcb::HOSTADDR_LO));
        (high << 32 | low, (Fake::sram(state, at + rcb::MAXLEN_FLAGS) >> 16) as usize)
    }

    fn host_address(state: &State, high: usize, low: usize) -> u64 {
        u64::from(Fake::reg(state, high)) << 32 | u64::from(Fake::reg(state, low))
    }

    /// A core reset: the state machines and mailboxes cleared, the MAC
    /// off, and the bootcode's answer to the driver's magic.
    fn reset(state: &mut State) {
        for at in [
            reg::MAC_MODE,
            reg::RX_MODE,
            reg::TX_MODE,
            reg::RBDI_MODE,
            reg::RXLP_MODE,
            reg::RDBDI_MODE,
            reg::RDC_MODE,
            reg::RBDC_MODE,
            reg::SRS_MODE,
            reg::SBDI_MODE,
            reg::SDI_MODE,
            reg::RDMA_MODE,
            reg::SDC_MODE,
            reg::SBDC_MODE,
            reg::HCC_MODE,
            reg::WDMA_MODE,
            reg::BMAN_MODE,
            reg::MBX_IRQ0_LO,
            reg::MBX_RX_STD_PROD_LO,
            reg::MBX_RX_CONS0_LO,
            reg::MBX_TX_HOST_PROD0_LO,
        ] {
            Fake::set_reg(state, at, 0);
        }
        state.std_next = 0;
        state.rx_produced = 0;
        state.tx_index = 0;
        state.tag = 0;
        state.resets += 1;
        if state.bootcode && Fake::sram(state, sram::FW_MB) == sram::FW_MB_MAGIC {
            state.sram.insert(sram::FW_MB, !sram::FW_MB_MAGIC);
        }
    }

    /// Writes the status block, if host coalescing runs, with `bits` in
    /// its status word and a new tag.
    fn update_status(state: &mut State, bits: u32) {
        if Fake::reg(state, reg::HCC_MODE) & hcc::ENABLE == 0 {
            return;
        }
        let at = Fake::host_address(state, reg::HCC_STATUSBLK_ADDR_HI, reg::HCC_STATUSBLK_ADDR_LO);
        state.tag = (state.tag + 1) & 0xff;
        write32_at(at, u32_at(at) | STATUS_UPDATED | bits);
        write32_at(at + 4, state.tag);
        write16_at(at + 16, state.rx_produced as u16);
        write16_at(at + 18, state.tx_index as u16);
    }

    fn linked(state: &State) -> bool {
        state.negotiated && state.cable.is_some()
    }

    fn phy_read(state: &State, at: u16) -> u16 {
        match at {
            // 10 and 100 Mb/s both ways, extended status, autonegotiation,
            // and once linked, negotiation complete and the link.
            0x01 => 0x7949 | if Fake::linked(state) { 0x0024 } else { 0 },
            0x19 => match state.cable {
                Some((mbps, full)) if state.negotiated => {
                    let resolved = match (mbps, full) {
                        (1000, true) => 0x0700,
                        (1000, false) => 0x0600,
                        (100, true) => 0x0500,
                        (100, false) => 0x0300,
                        (_, true) => 0x0200,
                        (_, false) => 0x0100,
                    };
                    0x0004 | resolved
                }
                _ => 0,
            },
            _ => state.phy[usize::from(at)],
        }
    }

    fn phy_write(state: &mut State, at: u16, value: u16) {
        state.phy_writes.push((at, value));
        if at == BMCR && value & 0x8000 != 0 {
            state.phy[0] = 0x1140;
            state.negotiated = false;
            return;
        }
        if at == BMCR && value & 0x1200 == 0x1200 {
            state.negotiated = true;
            state.negotiations += 1;
        }
        state.phy[usize::from(at)] = value;
    }

    fn mdio(state: &mut State, command: u32) {
        state.mdio += 1;
        let phy = ((command >> 21) & 0x1f) as u16;
        let at = ((command >> 16) & 0x1f) as u16;
        let result = if phy != state.phy_address {
            mi::COMM_READFAIL
        } else if command & mi::CMD_READ != 0 {
            u32::from(Fake::phy_read(state, at))
        } else {
            Fake::phy_write(state, at, command as u16);
            0
        };
        Fake::set_reg(state, reg::MI_COMM, command & !(mi::COMM_BUSY | 0xffff) | result);
    }

    /// Sends what the driver handed the chip, if its transmitter runs.
    fn transmit(state: &mut State) {
        if state.tx_stuck || Fake::reg(state, reg::TX_MODE) & tx_mode::ENABLE == 0 {
            return;
        }
        let (base, count) = Fake::rcb(state, sram::SEND_RING_RCB);
        let producer = Fake::reg(state, reg::MBX_TX_HOST_PROD0_LO) as usize;
        while state.tx_index != producer {
            let at = base + state.tx_index as u64 * TX_DESCRIPTOR;
            let address = u64::from(u32_at(at)) << 32 | u64::from(u32_at(at + 4));
            let word = u32_at(at + 8);
            assert_eq!(word & 0xffff, BD_END, "one buffer a frame, no offloads");
            assert_eq!(u32_at(at + 12), 0, "no VLAN tag, no segmentation");
            state.sent.push(read_mem(address, (word >> 16) as usize));
            state.tx_index = (state.tx_index + 1) % count;
        }
        Fake::update_status(state, 0);
    }

    /// Puts `frame` on the wire to the chip, with its frame check
    /// sequence and `flags` in its return descriptor: whether the chip
    /// took it.
    fn deliver(&self, frame: &[u8], flags: u32) -> bool {
        let mut state = self.state.lock().unwrap();
        if Fake::reg(&state, reg::RX_MODE) & rx_mode::ENABLE == 0 {
            return false;
        }
        // The chip stops at the producer the driver last wrote.
        let producer = Fake::reg(&state, reg::MBX_RX_STD_PROD_LO) as usize;
        if state.std_next == producer {
            return false;
        }
        let (returned, count) = Fake::rcb(&state, sram::RX_RETURN_RING_RCB);
        let consumed = Fake::reg(&state, reg::MBX_RX_CONS0_LO) as usize;
        if (state.rx_produced + 1) % count == consumed {
            return false;
        }
        let standard =
            Fake::host_address(&state, reg::RX_STD_RCB_HADDR_HI, reg::RX_STD_RCB_HADDR_LO);
        let at = standard + state.std_next as u64 * RX_DESCRIPTOR;
        let high = u32_at(at);
        let low = u32_at(at + 4);
        let len_index = u32_at(at + 8);
        assert_eq!(u32_at(at + 12), BD_END);
        let mut data = frame.to_vec();
        data.extend_from_slice(&[0xfc; 4]);
        assert!(data.len() <= (len_index & 0xffff) as usize, "the buffer holds the frame");
        fake_memory_write(u64::from(high) << 32 | u64::from(low), &data);
        let rrd = returned + state.rx_produced as u64 * RX_DESCRIPTOR;
        write32_at(rrd, high);
        write32_at(rrd + 4, low);
        write32_at(rrd + 8, (len_index & 0xffff_0000) | data.len() as u32);
        write32_at(rrd + 12, BD_END | flags);
        state.rx_produced = (state.rx_produced + 1) % count;
        state.std_next = (state.std_next + 1) % STD_RING;
        Fake::update_status(&mut state, 0);
        true
    }

    /// Plugs the cable in or pulls it out: the PHY tells the MAC, which
    /// says so in its status register and the status block.
    fn cable(&self, link: Option<(u32, bool)>) {
        let mut state = self.state.lock().unwrap();
        state.cable = link;
        let sts = Fake::reg(&state, reg::MAC_STS) | reg::mac_sts::LINK_CHANGED;
        Fake::set_reg(&mut state, reg::MAC_STS, sts);
        Fake::update_status(&mut state, STATUS_LINK_CHANGED);
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.state.lock().unwrap().sent)
    }

    fn reg32(&self, offset: usize) -> u32 {
        Fake::reg(&self.state.lock().unwrap(), offset)
    }

    fn set32(&self, offset: usize, value: u32) {
        Fake::set_reg(&mut self.state.lock().unwrap(), offset, value);
    }

    fn config32(&self, offset: usize) -> u32 {
        get(&self.state.lock().unwrap().config, offset, 4) as u32
    }

    fn config16(&self, offset: usize) -> u16 {
        get(&self.state.lock().unwrap().config, offset, 2) as u16
    }

    fn ape32(&self, offset: usize) -> u32 {
        get(&self.state.lock().unwrap().ape, offset, 4) as u32
    }

    fn sram32(&self, at: usize) -> u32 {
        Fake::sram(&self.state.lock().unwrap(), at)
    }

    fn rcb_word(&self, at: usize, word: usize) -> u32 {
        self.sram32(at + word)
    }

    fn writes_to(&self, offset: usize) -> Vec<u32> {
        let state = self.state.lock().unwrap();
        state.writes.iter().filter(|&&(o, _)| o == offset).map(|&(_, v)| v).collect()
    }

    fn phy(&self, at: u16) -> u16 {
        Fake::phy_read(&self.state.lock().unwrap(), at)
    }

    fn phy_writes_to(&self, at: u16) -> Vec<u16> {
        let state = self.state.lock().unwrap();
        state.phy_writes.iter().filter(|&&(a, _)| a == at).map(|&(_, v)| v).collect()
    }

    fn mdio_commands(&self) -> u32 {
        core::mem::take(&mut self.state.lock().unwrap().mdio)
    }

    fn negotiations(&self) -> u32 {
        self.state.lock().unwrap().negotiations
    }

    fn resets(&self) -> u32 {
        self.state.lock().unwrap().resets
    }

    fn status_block(&self) -> u64 {
        let state = self.state.lock().unwrap();
        Fake::host_address(&state, reg::HCC_STATUSBLK_ADDR_HI, reg::HCC_STATUSBLK_ADDR_LO)
    }

    fn tag(&self) -> u32 {
        self.state.lock().unwrap().tag
    }

    fn stick(&self, stuck: bool) {
        let mut state = self.state.lock().unwrap();
        state.tx_stuck = stuck;
        Fake::transmit(&mut state);
    }

    fn unplug_from_bus(&self) {
        self.state.lock().unwrap().gone = true;
    }

    fn hold(&self, lock: usize) {
        self.state.lock().unwrap().grants[lock] = FIRMWARE;
    }

    fn grants(&self) -> [u32; 8] {
        self.state.lock().unwrap().grants
    }

    fn requests(&self) -> Vec<(usize, u32)> {
        self.state.lock().unwrap().requests.clone()
    }

    fn events(&self) -> Vec<u32> {
        self.state.lock().unwrap().events.clone()
    }
}

/// The function's configuration space.
struct Config(Arc<Fake>);
/// The chip's registers, its first BAR.
struct Bar(Arc<Fake>);
/// The APE's registers, its third BAR.
struct ApeBar(Arc<Fake>);

impl Registers for Config {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let state = self.0.state.lock().unwrap();
        match (offset, size) {
            (pci::MEMWIN_DATA, 4) => {
                let base = get(&state.config, pci::MEMWIN_BASEADDR, 4) as usize;
                u64::from(Fake::sram(&state, base))
            }
            (pci::MISC_CTL, 4) => u64::from(state.misc_id << 16) | get(&state.config, offset, 2),
            _ => get(&state.config, offset, size),
        }
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut state = self.0.state.lock().unwrap();
        match (offset, size) {
            (pci::MEMWIN_DATA, 4) => {
                let base = get(&state.config, pci::MEMWIN_BASEADDR, 4) as usize;
                state.sram.insert(base, value as u32);
            }
            (pci::MISC_CTL, 4) => put(&mut state.config, offset, 2, value & 0xffff),
            // Its errors are cleared by writing them.
            (at, 2) if at == EXPRESS + 0xa => {
                let errors = get(&state.config, at, 2) & !value;
                put(&mut state.config, at, 2, errors);
            }
            _ => put(&mut state.config, offset, size, value),
        }
    }
}

impl Registers for Bar {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let state = self.0.state.lock().unwrap();
        if state.gone {
            return u64::MAX >> (64 - 8 * size);
        }
        if offset >= reg::MEMWIN_START {
            let base = get(&state.config, pci::MEMWIN_BASEADDR, 4) as usize;
            return u64::from(Fake::sram(&state, base + offset - reg::MEMWIN_START));
        }
        get(&state.regs, offset, size)
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut state = self.0.state.lock().unwrap();
        if state.gone {
            return;
        }
        assert_eq!(size, 4, "the registers are written a word at a time");
        let word = value as u32;
        state.writes.push((offset, word));
        if offset >= reg::MEMWIN_START {
            let base = get(&state.config, pci::MEMWIN_BASEADDR, 4) as usize;
            state.sram.insert(base + offset - reg::MEMWIN_START, word);
            return;
        }
        match offset {
            reg::MI_COMM => Fake::mdio(&mut state, word),
            reg::MAC_STS => {
                let sts = Fake::reg(&state, offset) & !(word & reg::mac_sts::CHANGES);
                Fake::set_reg(&mut state, offset, sts);
            }
            reg::NVRAM_SWARB => {
                let granted =
                    if word & reg::NVRAMSWARB_SET1 != 0 { reg::NVRAMSWARB_GNT1 } else { 0 };
                Fake::set_reg(&mut state, offset, word | granted);
            }
            reg::MISC_CFG => {
                Fake::set_reg(&mut state, offset, word & !reg::misc_cfg::RESET_CORE_CLOCKS);
                if word & reg::misc_cfg::RESET_CORE_CLOCKS != 0 {
                    Fake::reset(&mut state);
                }
            }
            reg::HCC_MODE => {
                Fake::set_reg(&mut state, offset, word & !hcc::COAL_NOW);
                if word & hcc::COAL_NOW != 0 {
                    Fake::update_status(&mut state, 0);
                }
            }
            reg::MBX_TX_HOST_PROD0_LO => {
                Fake::set_reg(&mut state, offset, word);
                Fake::transmit(&mut state);
            }
            _ => Fake::set_reg(&mut state, offset, word),
        }
    }
}

/// Which lock a write to the APE's lock registers at `offset` is for:
/// the lock, whether it requests or releases, and whether the
/// registers are the BCM5761's.
fn ape_lock(offset: usize) -> Option<(usize, bool, bool)> {
    let regions = [
        (ape::LOCK_REQ, true, true),
        (ape::LOCK_GRANT, false, true),
        (ape::PER_LOCK_REQ, true, false),
        (ape::PER_LOCK_GRANT, false, false),
    ];
    regions.into_iter().find_map(|(base, request, old)| {
        (base..base + 32).contains(&offset).then(|| ((offset - base) / 4, request, old))
    })
}

impl Registers for ApeBar {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let state = self.0.state.lock().unwrap();
        match ape_lock(offset) {
            Some((lock, false, _)) => u64::from(state.grants[lock]),
            _ => get(&state.ape, offset, size),
        }
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut state = self.0.state.lock().unwrap();
        let word = value as u32;
        match ape_lock(offset) {
            Some((lock, true, old)) => {
                state.requests.push((lock, word));
                state.old_locks |= old;
                if state.grants[lock] == 0 {
                    state.grants[lock] = word;
                }
            }
            Some((lock, false, _)) => state.grants[lock] &= !word,
            None if offset == ape::EVENT && word & ape::EVENT_1 != 0 => {
                // The firmware takes the event.
                let status = get(&state.ape, ape::EVENT_STATUS, 4) as u32;
                state.events.push(status & !ape::EVENT_STATUS_EVENT_PENDING);
                let taken = u64::from(status & !ape::EVENT_STATUS_EVENT_PENDING);
                put(&mut state.ape, ape::EVENT_STATUS, 4, taken);
            }
            None => put(&mut state.ape, offset, size, value),
        }
    }
}

struct Parts {
    fake: Arc<Fake>,
    model: Model,
    function: Function,
    regs: Mmio,
    ape: Option<Mmio>,
}

fn parts(setup: &Setup) -> Parts {
    let model = identify(VENDOR, setup.device).unwrap();
    let fake = Fake::new(setup, &model);
    let address = Address { segment: 0, bus: 2, device: 0, function: setup.function };
    let config = Mmio::fake(Arc::new(Config(fake.clone())), CONFIG_SIZE);
    let function = Function::probe(address, config).unwrap();
    let regs = Mmio::fake(Arc::new(Bar(fake.clone())), reg::END);
    let ape = model.ape().then(|| Mmio::fake(Arc::new(ApeBar(fake.clone())), 0x1_0000));
    Parts { fake, model, function, regs, ape }
}

fn try_start(setup: &Setup, interrupt: Interrupt) -> (Arc<Fake>, Result<Card, Error>) {
    let Parts { fake, model, function, regs, ape } = parts(setup);
    let card =
        start(model, &function, regs, ape, interrupt, FALLBACK, &mut alloc, &mut no_delay, clock);
    (fake, card)
}

fn started(setup: &Setup, interrupt: Interrupt) -> (Arc<Fake>, Card) {
    let (fake, card) = try_start(setup, interrupt);
    (fake, card.unwrap())
}

/// Plugs the cable in at `mbps`, full duplex or not, and lets the
/// driver see it.
fn plug(fake: &Fake, card: &mut Card, mbps: u32, full: bool) {
    fake.cable(Some((mbps, full)));
    advance(SECOND);
    let mut frame = [0u8; 2048];
    assert_eq!(card.receive(&mut frame), None);
}

fn frame(n: usize, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + n) as u8).collect()
}

fn error(card: Result<Card, Error>) -> Error {
    card.err().unwrap()
}

#[test]
fn identifies_broadcoms_cards_by_their_pci_ids() {
    let bcm5720 = identify(VENDOR, device::BCM5720).unwrap();
    assert_eq!(bcm5720.name, "Broadcom BCM5720");
    assert!(bcm5720.msi() && bcm5720.ape());
    assert_eq!(bcm5720.article(), "a");
    assert_eq!(identify(VENDOR, device::BCM5717C).unwrap().name, "Broadcom BCM5717");
    assert!(identify(VENDOR, device::BCM5761E).unwrap().ape());
    assert!(!identify(VENDOR, device::BCM57765).unwrap().ape());
    assert!(!identify(VENDOR, device::BCM57780).unwrap().ape());
    assert_eq!(identify(VENDOR, 0x1677), None, "the BCM5751, an older chip");
    assert_eq!(identify(VENDOR, 0x1639), None, "the BCM5709, which is bce(4)'s");
    assert_eq!(identify(0x1028, device::BCM5720), None);
}

#[test]
fn the_chip_is_read_from_the_register_its_family_uses() {
    for (setup, asicrev) in [
        (bcm5720(), asic::BCM5720),
        (setup(device::BCM5719, 0x0571_9001), asic::BCM5719),
        (setup(device::BCM5717, 0x0571_7100), asic::BCM5717),
        (bcm5761(), asic::BCM5761),
        (setup(device::BCM5762, 0x0576_2100), asic::BCM5762),
        (bcm57765(), asic::BCM57765),
        (setup(device::BCM57766, 0x5776_6001), asic::BCM57766),
        (bcm57780(), asic::BCM57780),
    ] {
        let (_fake, card) = started(&setup, Interrupt::Msi);
        assert_eq!(card.chip.chipid, setup.chipid);
        assert_eq!(card.chip.asicrev, asicrev);
    }
    // The BCM5717's C0 is a BCM5720, whatever its register says.
    let (_fake, card) = started(&setup(device::BCM5717C, 0x0571_7200), Interrupt::Msi);
    assert_eq!((card.chip.chipid, card.chip.asicrev), (chipid::BCM5720_A0, asic::BCM5720));
}

#[test]
fn chips_this_driver_does_not_know_are_refused() {
    // A BCM5718 that says it is a BCM5714, and one whose old register
    // names a BCM5700.
    let (_fake, card) = try_start(&setup(device::BCM5718, 0x0571_4000), Interrupt::Msi);
    assert_eq!(error(card), Error::Unsupported("a chip this driver does not know"));
    let mut old = setup(device::BCM5718, 0x0571_7000);
    old.misc_id = 0x7100;
    let (_fake, card) = try_start(&old, Interrupt::Msi);
    assert_eq!(error(card), Error::Unsupported("a chip this driver does not know"));
}

#[test]
fn fibre_serdes_and_plain_pci_are_refused() {
    let mut serdes = bcm5720();
    serdes.serdes = true;
    let (_fake, card) = try_start(&serdes, Interrupt::Msi);
    assert_eq!(error(card), Error::Unsupported("copper: its port is a SerDes"));
    // The BCM5717's A0 says it in its strap register.
    let mut a0 = setup(device::BCM5717, chipid::BCM5717_A0);
    a0.serdes = true;
    let (_fake, card) = try_start(&a0, Interrupt::Msi);
    assert_eq!(error(card), Error::Unsupported("copper: its port is a SerDes"));
    // A SerDes elsewhere than the BCM5717 family is not looked for.
    let mut desktop = bcm5761();
    desktop.serdes = true;
    assert!(try_start(&desktop, Interrupt::Msi).1.is_ok());

    let mut fibre = bcm5761();
    fibre.fibre = true;
    let (_fake, card) = try_start(&fibre, Interrupt::Msi);
    assert_eq!(error(card), Error::Unsupported("copper: its port is fibre"));

    let mut legacy = bcm57780();
    legacy.express = false;
    let (_fake, card) = try_start(&legacy, Interrupt::Msi);
    assert_eq!(error(card), Error::Unsupported("PCI Express"));
}

#[test]
fn a_chip_that_is_gone_or_mapped_too_small_is_refused() {
    let Parts { fake, model, function, regs: _, ape } = parts(&bcm5720());
    let small = Mmio::fake(Arc::new(Bar(fake.clone())), 0x8000);
    let card = start(
        model,
        &function,
        small,
        ape,
        Interrupt::Msi,
        FALLBACK,
        &mut alloc,
        &mut no_delay,
        clock,
    );
    assert_eq!(error(card), Error::Registers);

    let Parts { fake, model, function, regs, ape } = parts(&bcm5720());
    fake.unplug_from_bus();
    let card = start(
        model,
        &function,
        regs,
        ape,
        Interrupt::Msi,
        FALLBACK,
        &mut alloc,
        &mut no_delay,
        clock,
    );
    assert_eq!(error(card), Error::Device("its registers read as all ones"));
}

#[test]
fn the_address_comes_from_the_bootcode_then_the_registers() {
    let (_fake, card) = started(&bcm5720(), Interrupt::Msi);
    assert_eq!(card.info().mac, MAC);
    let mut registers = bcm5720();
    registers.sram_mac = None;
    let (fake, card) = started(&registers, Interrupt::Msi);
    assert_eq!(card.info().mac, REGISTER_MAC);
    // Programmed back as it was read.
    assert_eq!(fake.reg32(reg::MAC_ADDR1_LO), 0x0010);
    assert_eq!(fake.reg32(reg::MAC_ADDR1_HI), 0x180a_0b0c);
    let mut none = bcm5720();
    none.sram_mac = None;
    none.register_mac = [0; 6];
    let (fake, card) = started(&none, Interrupt::Msi);
    assert_eq!(card.info().mac, FALLBACK);
    assert_eq!(fake.reg32(reg::MAC_ADDR1_LO), 0x0200);
    assert_eq!(fake.reg32(reg::MAC_ADDR1_HI), 0x0000_0001);
    // A multicast address is no card's.
    let mut multicast = bcm5720();
    multicast.sram_mac = Some([0x01, 0, 0x5e, 0, 0, 1]);
    multicast.register_mac = [0; 6];
    assert_eq!(started(&multicast, Interrupt::Msi).1.info().mac, FALLBACK);
}

#[test]
fn start_resets_twice_and_waits_for_the_bootcode() {
    let (fake, card) = started(&bcm5720(), Interrupt::Msi);
    assert_eq!(fake.resets(), 2, "once to read the address, once to run");
    assert_eq!(fake.sram32(sram::FW_MB), !sram::FW_MB_MAGIC, "the bootcode answered");
    assert_eq!(fake.writes_to(reg::FASTBOOT_PC), [0, 0]);
    assert!(fake.writes_to(reg::NVRAM_SWARB).iter().all(|&w| w == reg::NVRAMSWARB_SET1));
    let info = card.info();
    assert_eq!(info.model, "Broadcom BCM5720");
    assert_eq!((info.receive_slots, info.transmit_slots), (256, 255));
    // A bootcode that never answers is waited for, then the chip is used
    // all the same, as FreeBSD's is.
    let mut silent = bcm5720();
    silent.bootcode = false;
    let (fake, card) = started(&silent, Interrupt::Msi);
    assert_eq!(fake.sram32(sram::FW_MB), sram::FW_MB_MAGIC);
    assert_eq!(card.info().mac, MAC);
}

#[test]
fn the_pci_express_state_is_set_for_each_chip() {
    // The 5719's and 5720's reads are kept to 2048 bytes; relaxed
    // ordering and no snoop off, the errors cleared.
    let (fake, _card) = started(&bcm5720(), Interrupt::Msi);
    assert_eq!(fake.config16(EXPRESS + 8), 0x4000);
    assert_eq!(fake.config16(EXPRESS + 0xa), 0);
    let (fake, _card) = started(&bcm5761(), Interrupt::Msi);
    assert_eq!(fake.config16(EXPRESS + 8), 0x5000);
    // Memory write and invalidate off; the cache line size kept.
    assert_eq!(fake.config32(pci::CMD) & pci::CMD_MWIEN, 0);
    assert_eq!(fake.config32(pci::CMD) & 0x0006, 0x0006);
    assert_eq!(fake.config32(pci::CACHESZ), 0x10);
    // Word swapping, indirect access, tagged status only on the 5717
    // family with an MSI.
    let misc = fake.config32(pci::MISC_CTL);
    let wanted = pci::MISC_CTL_ENDIAN_WORDSWAP | pci::MISC_CTL_INDIRECT_ACCESS;
    assert_eq!(misc & wanted, wanted);
    assert_eq!(misc & pci::MISC_CTL_MASK_PCI_INTR, 0, "unmasked for the MSI");
    assert_eq!(misc & pci::MISC_CTL_TAGGED_STATUS, 0);
    let (fake, _card) = started(&bcm5720(), Interrupt::Msi);
    assert_ne!(fake.config32(pci::MISC_CTL) & pci::MISC_CTL_TAGGED_STATUS, 0);
    let (fake, _card) = started(&bcm5720(), Interrupt::Polled);
    assert_eq!(fake.config32(pci::MISC_CTL) & pci::MISC_CTL_TAGGED_STATUS, 0);
}

#[test]
fn dma_is_set_up_as_freebsd_does() {
    let (fake, _card) = started(&bcm5720(), Interrupt::Msi);
    let dma = fake.config32(pci::DMA_RW_CTL);
    let wanted = pci::dma_rw_ctl_rd_cmd(6)
        | pci::dma_rw_ctl_wr_cmd(7)
        | pci::dma_rw_ctl_wr_wat(7)
        | pci::DMA_RW_CTL_TAGGED_STATUS_WA;
    assert_eq!(dma, wanted & !pci::DMA_RW_CTL_DIS_CACHE_ALIGNMENT);
    let (fake, _card) = started(&bcm5761(), Interrupt::Msi);
    let dma = fake.config32(pci::DMA_RW_CTL);
    assert_eq!(dma & pci::DMA_RW_CTL_TAGGED_STATUS_WA, 0);
    // The byte order, the send descriptors in host memory, and the
    // statistics and status blocks in the chip's memory cleared.
    let mode = fake.reg32(reg::MODE_CTL);
    let wanted = mode_ctl::SWAP | mode_ctl::HOST_SEND_BDS | mode_ctl::MAC_ATTN_INTR;
    assert_eq!(mode & wanted, wanted);
    assert_ne!(mode & mode_ctl::STACKUP, 0, "the firmware told the driver runs");
    assert_eq!(fake.sram32(sram::STATS_BLOCK), 0);
    // The 57765 family's fast training fix.
    let (fake, _card) = started(&bcm57765(), Interrupt::Msi);
    assert_ne!(fake.reg32(reg::CPMU_PADRNG_CTL) & reg::CPMU_PADRNG_CTL_RDIV2, 0);
}

#[test]
fn the_rings_are_where_each_family_wants_them() {
    let (fake, _card) = started(&bcm5720(), Interrupt::Msi);
    assert_eq!(fake.reg32(reg::RX_STD_RCB_NICADDR), sram::STD_RX_RINGS_5717);
    assert_eq!(fake.reg32(reg::RX_STD_RCB_MAXLEN_FLAGS), 512 << 16 | 1536 << 2);
    assert_ne!(fake.reg32(reg::RX_STD_RCB_HADDR_LO), 0);
    assert_eq!(fake.reg32(reg::MBX_RX_STD_PROD_LO), 255, "256 buffers, one held back");
    // The jumbo ring disabled.
    let jumbo = fake.reg32(reg::RX_JUMBO_RCB_MAXLEN_FLAGS);
    assert_ne!(jumbo & rcb::FLAG_RING_DISABLED, 0);
    // One send ring in host memory, the others disabled.
    let send = sram::SEND_RING_RCB;
    assert_eq!(fake.rcb_word(send, rcb::MAXLEN_FLAGS), 512 << 16);
    assert_eq!(fake.rcb_word(send, rcb::NICADDR), sram::SEND_RING);
    assert_ne!(fake.rcb_word(send, rcb::HOSTADDR_LO), 0);
    for ring in 1..4 {
        let flags = fake.rcb_word(send + ring * rcb::SIZE, rcb::MAXLEN_FLAGS);
        assert_eq!(flags, rcb::FLAG_RING_DISABLED);
    }
    // The return rings: the first of 1024 descriptors, the other 15
    // disabled.
    let returned = sram::RX_RETURN_RING_RCB;
    assert_eq!(fake.rcb_word(returned, rcb::MAXLEN_FLAGS), 1024 << 16);
    for ring in 1..16 {
        let flags = fake.rcb_word(returned + ring * rcb::SIZE, rcb::MAXLEN_FLAGS);
        assert_eq!(flags, rcb::FLAG_RING_DISABLED);
    }
    // The older chips' rings.
    let (fake, _card) = started(&bcm5761(), Interrupt::Msi);
    assert_eq!(fake.reg32(reg::RX_STD_RCB_NICADDR), sram::STD_RX_RINGS);
    assert_eq!(fake.reg32(reg::RX_STD_RCB_MAXLEN_FLAGS), 512 << 16);
    assert_eq!(fake.rcb_word(returned, rcb::MAXLEN_FLAGS), 512 << 16);
    assert_eq!(fake.writes_to(reg::RX_JUMBO_RCB_MAXLEN_FLAGS), []);
    // The 57765 family's four return rings.
    let (fake, _card) = started(&bcm57765(), Interrupt::Msi);
    assert_eq!(fake.rcb_word(returned, rcb::MAXLEN_FLAGS), 1024 << 16);
    assert_eq!(fake.rcb_word(returned + 3 * rcb::SIZE, rcb::MAXLEN_FLAGS), rcb::FLAG_RING_DISABLED);
    assert_eq!(fake.writes_to(reg::MBX_RX_CONS0_LO + 4 * 8), []);
}

#[test]
fn the_mac_runs_with_every_multicast_frame_and_no_promiscuity() {
    let (fake, card) = started(&bcm5720(), Interrupt::Msi);
    assert_eq!(fake.reg32(reg::RX_MTU), 1522);
    for word in 0..4 {
        assert_eq!(fake.reg32(reg::MAR0 + 4 * word), u32::MAX);
    }
    let rx = fake.reg32(reg::RX_MODE);
    assert_eq!(
        rx & (rx_mode::ENABLE | rx_mode::IPV6_ENABLE),
        rx_mode::ENABLE | rx_mode::IPV6_ENABLE
    );
    assert_eq!(rx & (rx_mode::RX_PROMISC | rx_mode::FLOWCTL_ENABLE), 0);
    let tx = fake.reg32(reg::TX_MODE);
    assert_eq!(tx & tx_mode::ENABLE, tx_mode::ENABLE);
    assert_ne!(tx & tx_mode::MBUF_LOCKUP_FIX, 0);
    // The state machines on, the status block in host memory and
    // written once.
    for block in [reg::RDC_MODE, reg::RBDI_MODE, reg::SDI_MODE, reg::RDMA_MODE, reg::WDMA_MODE] {
        assert_ne!(fake.reg32(block) & reg::ENABLE, 0, "{block:#x}");
    }
    assert_ne!(fake.reg32(reg::HCC_MODE) & hcc::ENABLE, 0);
    assert_ne!(fake.status_block(), 0);
    assert_eq!(fake.tag(), 1);
    assert_eq!(fake.reg32(reg::MAC_EVT_ENB), reg::EVTENB_LINK_CHANGED);
    // The PHY negotiates every speed it has, without pause frames.
    assert_eq!(fake.negotiations(), 1);
    assert_eq!(fake.phy(ANAR), 0x01e1);
    assert_eq!(fake.phy(GTCR), 0x0300);
    assert_eq!(fake.phy(IMR), 0xff00);
    assert_eq!(card.link(), Link::default());
}

#[test]
fn the_5719_s_buffer_manager_and_the_5717_s_read_dma_get_their_fixes() {
    let (fake, _card) = started(&setup(device::BCM5719, 0x0571_9001), Interrupt::Msi);
    assert_ne!(fake.reg32(reg::BMAN_MODE) & reg::bman::NO_TX_UNDERRUN, 0);
    let (fake, _card) = started(&setup(device::BCM5717, 0x0571_7100), Interrupt::Msi);
    assert_ne!(fake.reg32(reg::RDMA_MODE) & reg::rdma::MULT_DMA_RD_DIS, 0);
    assert_eq!(fake.reg32(reg::BMAN_MODE) & reg::bman::NO_TX_UNDERRUN, 0);
    // The 5719's A0 segments nothing, and its read DMA keeps a margin.
    let (fake, _card) = started(&setup(device::BCM5719, chipid::BCM5719_A0), Interrupt::Msi);
    assert_eq!(fake.reg32(reg::SDI_MODE) & reg::SDI_HW_LSO_PRE_DMA, 0);
    assert_eq!(fake.reg32(reg::RDMA_MODE) & reg::rdma::TSO4_ENABLE, 0);
    let control = fake.reg32(reg::RDMA_RSRVCTRL);
    assert_ne!(control & reg::rdma::RSRVCTRL_TXMRGN_320B, 0);
    assert_eq!(fake.writes_to(reg::RX_JUMBO_RCB_MAXLEN_FLAGS), [], "it has no jumbo ring");
    // The BCM5761's send data completion waits.
    let (fake, _card) = started(&bcm5761(), Interrupt::Msi);
    assert_ne!(fake.reg32(reg::SDC_MODE) & reg::SDC_CDELAY, 0);
}

#[test]
fn the_5720_s_read_dma_fix_is_on_until_frames_go_out() {
    let mut miscounting = bcm5720();
    miscounting.rdma_lengths = 0x0600_0600;
    let (fake, mut card) = started(&miscounting, Interrupt::Msi);
    let fix = reg::rdma::TX_LENGTH_WA_5720;
    assert_ne!(fake.reg32(reg::RDMA_LSO_CRPTEN_CTRL) & fix, 0);
    plug(&fake, &mut card, 1000, true);
    let mut buffer = [0u8; 2048];
    for n in 0..4 {
        assert!(card.send(&frame(n, 100)));
        card.flush();
        assert_eq!(card.receive(&mut buffer), None);
        assert_ne!(fake.reg32(reg::RDMA_LSO_CRPTEN_CTRL) & fix, 0, "{n} sent");
    }
    assert!(card.send(&frame(4, 100)));
    card.flush();
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(fake.reg32(reg::RDMA_LSO_CRPTEN_CTRL) & fix, 0);
    // A chip that counts right never has it.
    let (fake, _card) = started(&bcm5720(), Interrupt::Msi);
    assert_eq!(fake.reg32(reg::RDMA_LSO_CRPTEN_CTRL) & fix, 0);
}

#[test]
fn the_link_sets_the_mac_s_port_and_duplex() {
    let (fake, mut card) = started(&bcm5720(), Interrupt::Msi);
    assert!(!card.can_send() && !card.send(&frame(0, 64)), "nothing is sent without a link");
    plug(&fake, &mut card, 1000, true);
    assert_eq!(card.link(), Link { up: true, mbps: 1000, full_duplex: true });
    let mac = fake.reg32(reg::MAC_MODE);
    assert_eq!(mac & (mac_mode::PORTMODE | mac_mode::HALF_DUPLEX), mac_mode::PORTMODE_GMII);
    assert!(card.can_send());
    // Pulled out and plugged into a hub.
    fake.cable(None);
    advance(SECOND);
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(card.link(), Link::default());
    assert!(!card.can_send());
    plug(&fake, &mut card, 100, false);
    assert_eq!(card.link(), Link { up: true, mbps: 100, full_duplex: false });
    let mac = fake.reg32(reg::MAC_MODE);
    let wanted = mac_mode::PORTMODE_MII | mac_mode::HALF_DUPLEX;
    assert_eq!(mac & (mac_mode::PORTMODE | mac_mode::HALF_DUPLEX), wanted);
    assert_eq!(fake.reg32(reg::TX_MODE) & tx_mode::FLOWCTL_ENABLE, 0);
    assert_eq!(fake.reg32(reg::MAC_STS) & reg::mac_sts::LINK_CHANGED, 0, "acknowledged");
}

#[test]
fn a_link_that_stays_up_is_not_asked_about() {
    let (fake, mut card) = started(&bcm5720(), Interrupt::Msi);
    plug(&fake, &mut card, 1000, true);
    fake.mdio_commands();
    let mut buffer = [0u8; 2048];
    for _ in 0..3 {
        advance(SECOND);
        assert_eq!(card.receive(&mut buffer), None);
    }
    assert_eq!(fake.mdio_commands(), 0);
    // The status block's word alone says it changed.
    fake.cable(None);
    fake.set32(reg::MAC_STS, 0);
    advance(SECOND);
    assert_eq!(card.receive(&mut buffer), None);
    assert!(!card.link().up);
}

#[test]
fn the_link_is_looked_at_once_a_second_and_renegotiated_after_a_while() {
    let (fake, mut card) = started(&bcm5720(), Interrupt::Msi);
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), None);
    let looked = fake.mdio_commands();
    assert_ne!(looked, 0);
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(fake.mdio_commands(), 0, "not again within the second");
    for _ in 1..16 {
        advance(SECOND);
        assert_eq!(card.receive(&mut buffer), None);
    }
    assert_eq!(fake.negotiations(), 1);
    advance(SECOND);
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(fake.negotiations(), 2, "17 looks without a link");
}

#[test]
fn frames_are_received_through_the_return_ring_and_given_back() {
    let (fake, mut card) = started(&bcm5720(), Interrupt::Msi);
    let mut buffer = [0u8; 2048];
    let mut n = 0;
    // Enough rounds that both rings and the slots wrap.
    for round in 0..6 {
        let count = 200;
        for i in 0..count {
            assert!(fake.deliver(&frame(n + i, 60 + i), 0), "round {round}, frame {i}");
        }
        for i in 0..count {
            let len = card.receive(&mut buffer).unwrap();
            assert_eq!(&buffer[..len], &frame(n + i, 60 + i)[..], "round {round}, frame {i}");
        }
        assert_eq!(card.receive(&mut buffer), None);
        n += count;
        // Every buffer given back, every return descriptor read.
        let state = fake.state.lock().unwrap();
        assert_eq!(Fake::reg(&state, reg::MBX_RX_CONS0_LO) as usize, state.rx_produced);
        let producer = Fake::reg(&state, reg::MBX_RX_STD_PROD_LO) as usize;
        assert_eq!((producer + 1) % STD_RING, (state.std_next + 256) % STD_RING);
    }
    assert_eq!(card.dropped(), 0);
}

#[test]
fn the_chip_holds_frames_once_its_buffers_run_out() {
    let (fake, mut card) = started(&bcm5720(), Interrupt::Msi);
    for i in 0..255 {
        assert!(fake.deliver(&frame(i, 100), 0), "frame {i}");
    }
    assert!(!fake.deliver(&frame(255, 100), 0), "the one buffer held back");
    let mut buffer = [0u8; 2048];
    for i in 0..255 {
        assert_eq!(card.receive(&mut buffer), Some(100), "frame {i}");
    }
    assert_eq!(card.receive(&mut buffer), None);
    assert!(fake.deliver(&frame(0, 100), 0));
}

#[test]
fn bad_frames_are_dropped_and_their_buffers_given_back() {
    let (fake, mut card) = started(&bcm5761(), Interrupt::Msi);
    assert!(fake.deliver(&frame(0, 100), RX_ERROR));
    assert!(fake.deliver(&frame(1, 100), RX_JUMBO));
    assert!(fake.deliver(&frame(2, 10), 0), "shorter than a header");
    assert!(fake.deliver(&frame(3, 1514), 0));
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), Some(1514));
    assert_eq!(&buffer[..1514], &frame(3, 1514)[..]);
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(card.dropped(), 3);
    assert_eq!(fake.reg32(reg::MBX_RX_STD_PROD_LO), 259);
}

#[test]
fn a_return_index_past_the_ring_breaks_the_card() {
    let (fake, mut card) = started(&bcm5761(), Interrupt::Msi);
    assert!(!card.failed());
    write16_at(fake.status_block() + 16, 512);
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), None);
    assert!(card.failed());
}

#[test]
fn frames_are_sent_padded_through_the_send_ring() {
    let (fake, mut card) = started(&bcm5720(), Interrupt::Msi);
    plug(&fake, &mut card, 1000, true);
    assert!(card.send(&frame(0, 42)));
    assert!(card.send(&frame(1, 1514)));
    assert!(!card.send(&frame(2, 1519)), "longer than Ethernet's longest");
    assert!(!card.send(&frame(2, 13)), "shorter than a header");
    assert!(fake.sent().is_empty(), "nothing goes out until the flush");
    card.flush();
    let sent = fake.sent();
    assert_eq!(sent.len(), 2);
    let mut padded = frame(0, 42);
    padded.resize(60, 0);
    assert_eq!(sent[0], padded);
    assert_eq!(sent[1], frame(1, 1514));
    assert_eq!(fake.writes_to(reg::MBX_TX_HOST_PROD0_LO).last(), Some(&2));
    card.flush();
    assert_eq!(fake.writes_to(reg::MBX_TX_HOST_PROD0_LO).last(), Some(&2));
}

#[test]
fn at_most_255_frames_wait_and_the_ring_wraps() {
    let (fake, mut card) = started(&bcm5720(), Interrupt::Msi);
    plug(&fake, &mut card, 1000, true);
    fake.stick(true);
    for n in 0..255 {
        assert!(card.send(&frame(n, 64)), "frame {n}");
    }
    assert!(!card.can_send());
    assert!(!card.send(&frame(255, 64)));
    card.flush();
    fake.stick(false);
    assert!(card.can_send());
    assert_eq!(fake.sent().len(), 255);
    // Around the 512 descriptors, and the 256 slots, a few times.
    let mut n = 0;
    for _ in 0..8 {
        for i in 0..100 {
            assert!(card.send(&frame(n + i, 64 + i)));
        }
        card.flush();
        let sent = fake.sent();
        assert_eq!(sent.len(), 100);
        for (i, out) in sent.iter().enumerate() {
            assert_eq!(out, &frame(n + i, 64 + i));
        }
        n += 100;
    }
}

#[test]
fn frames_that_never_go_out_fail_the_card_after_five_seconds() {
    let (fake, mut card) = started(&bcm5720(), Interrupt::Msi);
    plug(&fake, &mut card, 1000, true);
    let mut buffer = [0u8; 2048];
    // Frames going out are fine however long it runs.
    for n in 0..3 {
        assert!(card.send(&frame(n, 64)));
        card.flush();
        advance(3 * SECOND);
        assert_eq!(card.receive(&mut buffer), None);
        assert!(!card.failed());
    }
    fake.stick(true);
    assert!(card.send(&frame(3, 64)));
    card.flush();
    assert_eq!(card.receive(&mut buffer), None);
    advance(4 * SECOND);
    assert_eq!(card.receive(&mut buffer), None);
    assert!(!card.failed());
    advance(SECOND);
    assert!(card.failed());
}

#[test]
fn tagged_status_acknowledges_the_msi_by_the_tag() {
    let (fake, mut card) = started(&bcm5720(), Interrupt::Msi);
    assert!(fake.deliver(&frame(0, 100), 0));
    let tag = fake.tag();
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), Some(100));
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(fake.writes_to(reg::MBX_IRQ0_LO).last(), Some(&(tag << 24)));
    // One-shot MSI on, the line interrupt unmasked.
    let msi = fake.reg32(reg::MSI_MODE);
    assert_eq!(
        msi & (reg::msi_mode::ENABLE | reg::msi_mode::ONE_SHOT_DISABLE),
        reg::msi_mode::ENABLE
    );
    assert_eq!(fake.config32(pci::MISC_CTL) & pci::MISC_CTL_MASK_PCI_INTR, 0);
}

#[test]
fn untagged_and_polled_cards_acknowledge_their_own_ways() {
    let (fake, mut card) = started(&bcm5761(), Interrupt::Msi);
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(fake.writes_to(reg::MBX_IRQ0_LO).last(), Some(&0));
    let (fake, mut card) = started(&bcm5720(), Interrupt::Polled);
    let before = fake.writes_to(reg::MBX_IRQ0_LO);
    assert_eq!(before.last(), Some(&1), "the interrupt masked");
    assert_ne!(fake.config32(pci::MISC_CTL) & pci::MISC_CTL_MASK_PCI_INTR, 0);
    assert!(fake.deliver(&frame(0, 100), 0));
    assert_eq!(card.receive(&mut buffer), Some(100));
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(fake.writes_to(reg::MBX_IRQ0_LO), before, "a polled card is not acknowledged");
}

#[test]
fn each_function_of_the_5717_family_has_its_own_phy() {
    let mut second = bcm5720();
    second.function = 1;
    let (fake, mut card) = started(&second, Interrupt::Msi);
    assert_eq!(card.chip.phy_address, 2);
    assert_eq!(fake.negotiations(), 1);
    plug(&fake, &mut card, 1000, true);
    assert!(card.link().up);
    let (_fake, card) = started(&bcm5761(), Interrupt::Msi);
    assert_eq!(card.chip.phy_address, 1);
}

#[test]
fn the_desktop_chips_phys_get_freebsd_s_fixes() {
    let (fake, _card) = started(&bcm5761(), Interrupt::Msi);
    assert_eq!(fake.phy_writes_to(AUXCTL), [0x0c00, 0x0400, 0x7007, 0xf017]);
    assert_eq!(fake.phy_writes_to(DSP_ADDR), [0x000a]);
    assert_eq!(fake.phy_writes_to(DSP_RW_PORT), [0x010b]);
    assert_eq!(fake.phy(EXTCTL), 0x0102, "not Dell's board");
    let mut dell = bcm57780();
    dell.subsystem = 0x1028;
    let (fake, _card) = started(&dell, Interrupt::Msi);
    assert_eq!(fake.phy_writes_to(AUXCTL), [0x7007, 0xf017]);
    assert_eq!(fake.phy_writes_to(DSP_ADDR), []);
    assert_eq!(fake.phy(EXTCTL), 0x0100, "Dell's link LED");
    let (fake, _card) = started(&bcm5720(), Interrupt::Msi);
    assert_eq!(fake.phy_writes_to(AUXCTL), []);
}

#[test]
fn the_ape_s_firmware_is_told_and_its_locks_are_taken_and_given_back() {
    let mut managed = bcm5720();
    managed.ape_running = true;
    let (fake, mut card) = started(&managed, Interrupt::Msi);
    assert!(card.chip.on_ape());
    assert_eq!(fake.config32(pci::PCISTATE) & pci::PCISTATE_ALLOW_APE, pci::PCISTATE_ALLOW_APE);
    let change = ape::EVENT_STATUS_DRIVER_EVNT | ape::EVENT_STATUS_STATE_CHNGE;
    assert_eq!(
        fake.events(),
        [ape::EVENT_STATUS_STATE_UNLOAD | change, ape::EVENT_STATUS_STATE_START | change]
    );
    assert_eq!(fake.ape32(ape::HOST_DRVR_STATE), ape::HOST_DRVR_STATE_START);
    assert_eq!(fake.ape32(ape::HOST_SEG_SIG), ape::HOST_SEG_SIG_MAGIC);
    assert_eq!(fake.ape32(ape::HOST_INIT_COUNT), 0);
    assert_eq!(fake.ape32(ape::HOST_BEHAVIOR), ape::HOST_BEHAV_NO_PHYLOCK);
    // The MAC keeps the APE's frames moving.
    assert_eq!(fake.reg32(reg::MAC_MODE) & mac_mode::APE, mac_mode::APE);
    // The GRC lock around each reset, the PHY's around each MDIO access,
    // the memory's around each event: all given back.
    let requests = fake.requests();
    assert_eq!(requests.iter().filter(|&&(lock, _)| lock == ape::LOCK_GRC).count(), 2);
    assert!(requests.iter().any(|&(lock, bit)| (lock, bit) == (ape::LOCK_PHY0, 0x1000)));
    assert_eq!(requests.iter().filter(|&&(lock, _)| lock == ape::LOCK_MEM).count(), 2);
    assert_eq!(fake.grants(), [0; 8]);
    assert!(!fake.state.lock().unwrap().old_locks);
    card.stop();
    assert_eq!(fake.events().len(), 3);
    assert_eq!(fake.ape32(ape::HOST_DRVR_STATE), ape::HOST_DRVR_STATE_UNLOAD);
    assert_eq!(fake.grants(), [0; 8]);
}

#[test]
fn the_5761_s_ape_locks_are_its_own_and_function_1_s_bits_are_too() {
    let mut desktop = bcm5761();
    desktop.ape_running = true;
    let (fake, _card) = started(&desktop, Interrupt::Msi);
    assert!(fake.state.lock().unwrap().old_locks);
    assert!(fake.requests().iter().all(|&(lock, _)| lock != ape::LOCK_GPIO));
    let mut second = bcm5720();
    second.function = 1;
    second.ape_running = true;
    let (fake, _card) = started(&second, Interrupt::Msi);
    let requests = fake.requests();
    assert!(requests.contains(&(ape::LOCK_GRC, 1 << 1)));
    assert!(requests.contains(&(ape::LOCK_PHY1, 0x1000)));
    assert_eq!(fake.grants(), [0; 8]);
}

#[test]
fn without_the_ape_s_firmware_no_lock_is_asked_for() {
    let (fake, card) = started(&bcm5720(), Interrupt::Msi);
    assert!(card.chip.ape.is_some() && !card.chip.on_ape());
    assert!(fake.requests().is_empty());
    assert!(fake.events().is_empty());
    assert_eq!(fake.reg32(reg::MAC_MODE) & mac_mode::APE, 0);
}

#[test]
fn a_phy_lock_the_firmware_keeps_leaves_the_phy_alone() {
    let mut managed = bcm5720();
    managed.ape_running = true;
    let Parts { fake, model, function, regs, ape } = parts(&managed);
    fake.hold(ape::LOCK_PHY0);
    let mut card = start(
        model,
        &function,
        regs,
        ape,
        Interrupt::Msi,
        FALLBACK,
        &mut alloc,
        &mut no_delay,
        clock,
    )
    .unwrap();
    assert_eq!(fake.mdio_commands(), 0);
    assert_eq!(fake.negotiations(), 0);
    assert_eq!(fake.grants()[ape::LOCK_PHY0], FIRMWARE, "the request withdrawn");
    fake.cable(Some((1000, true)));
    advance(SECOND);
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), None);
    assert!(!card.link().up);
}

#[test]
fn stop_stops_every_state_machine_and_resets_the_chip() {
    let (fake, mut card) = started(&bcm5720(), Interrupt::Msi);
    plug(&fake, &mut card, 1000, true);
    card.stop();
    assert_eq!(fake.resets(), 3);
    for block in [reg::RX_MODE, reg::RDMA_MODE, reg::WDMA_MODE, reg::HCC_MODE, reg::SDI_MODE] {
        let writes = fake.writes_to(block);
        assert_eq!(writes.last().map(|w| w & reg::ENABLE), Some(0), "{block:#x}");
    }
    assert_ne!(fake.config32(pci::MISC_CTL) & pci::MISC_CTL_MASK_PCI_INTR, 0);
    assert_eq!(fake.reg32(reg::MODE_CTL) & mode_ctl::STACKUP, 0, "the firmware told");
    assert!(!card.can_send() && !card.send(&frame(0, 64)));
    assert!(!fake.deliver(&frame(0, 64), 0));
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), None);
    card.stop();
    assert_eq!(fake.resets(), 3, "once");
    drop(card);
    assert_eq!(fake.resets(), 3);
}

#[test]
fn a_chip_that_falls_off_the_bus_fails_and_stops_at_once() {
    let (fake, mut card) = started(&bcm5720(), Interrupt::Msi);
    fake.unplug_from_bus();
    assert!(card.failed());
    card.stop();
    assert_eq!(fake.resets(), 2);
}

#[test]
fn dropping_a_card_stops_it() {
    let (fake, card) = started(&bcm57780(), Interrupt::Msi);
    drop(card);
    assert_eq!(fake.resets(), 3);
    assert_eq!(fake.reg32(reg::RX_MODE) & rx_mode::ENABLE, 0);
}
