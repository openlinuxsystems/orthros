// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The e1000 and e1000e family: Intel's 8254x PCI and PCI-X gigabit
//! controllers (the 8254x Family of Gigabit Ethernet Controllers
//! Software Developer's Manual) and the 82571, 82572, 82573, 82574 and
//! 82583 on PCI Express (the PCIe GbE Controllers Open Source Software
//! Developer's Manual and the 82574 datasheet), copper ones only.
//!
//! The driver resets the card (stopping its DMA first on PCI Express),
//! applies the errata settings the 82571 family needs, powers the PHY
//! up and lets it negotiate the link's speed, reads the card's MAC
//! address and programs it as its one unicast address, and takes every
//! multicast frame, which IPv6 and mDNS need, and broadcasts. It drives
//! one queue pair of [`legacy`](crate::legacy) descriptors: the receive
//! buffers are [`SLOT`](radix_nic::SLOT) bytes, the frame check sequence
//! is stripped, and short frames are padded. With an MSI the card raises
//! it for frames received and sent and for link changes, at most 20,000
//! times a second; reading the interrupt cause register, which the
//! driver does once no frame is left to take, acknowledges it.

use radix_hw::{Dma, Mmio};
use radix_nic::{Error, Info, Link, Nic, usable_mac, wait_for};

use crate::Interrupt;
use crate::legacy::{self, Rings};

/// The registers, from the start of BAR 0.
mod reg {
    pub const CTRL: usize = 0x0000;
    pub const STATUS: usize = 0x0008;
    pub const EECD: usize = 0x0010;
    pub const EERD: usize = 0x0014;
    pub const CTRL_EXT: usize = 0x0018;
    pub const MDIC: usize = 0x0020;
    pub const ICR: usize = 0x00c0;
    pub const ITR: usize = 0x00c4;
    pub const IMS: usize = 0x00d0;
    pub const IMC: usize = 0x00d8;
    pub const RCTL: usize = 0x0100;
    pub const TCTL: usize = 0x0400;
    pub const TIPG: usize = 0x0410;
    pub const EXTCNF_CTRL: usize = 0x0f00;
    pub const PBA_ECC: usize = 0x1100;
    pub const RDTR: usize = 0x2820;
    pub const TIDV: usize = 0x3820;
    pub const TXDCTL0: usize = 0x3828;
    pub const TXDCTL1: usize = 0x3928;
    pub const TARC0: usize = 0x3840;
    pub const TARC1: usize = 0x3940;
    pub const RFCTL: usize = 0x5008;
    pub const MTA: usize = 0x5200;
    pub const RAL0: usize = 0x5400;
    pub const RAH0: usize = 0x5404;
    pub const GCR: usize = 0x5b00;
    pub const GCR2: usize = 0x5b64;
    /// The last register the driver touches, and its size.
    pub const END: usize = GCR2 + 4;
}

/// CTRL's bits.
mod ctrl {
    pub const LRST: u32 = 1 << 3;
    pub const GIO_MASTER_DISABLE: u32 = 1 << 2;
    pub const ASDE: u32 = 1 << 5;
    pub const SLU: u32 = 1 << 6;
    pub const ILOS: u32 = 1 << 7;
    pub const FRCSPD: u32 = 1 << 11;
    pub const FRCDPX: u32 = 1 << 12;
    pub const RST: u32 = 1 << 26;
    pub const RFCE: u32 = 1 << 27;
    pub const TFCE: u32 = 1 << 28;
    /// Cleared on the 82573, 82574 and 82583, as their errata say.
    pub const BIT_29: u32 = 1 << 29;
    pub const VME: u32 = 1 << 30;
    pub const PHY_RST: u32 = 1 << 31;
}

/// STATUS's bits.
mod status {
    pub const FD: u32 = 1 << 0;
    pub const LU: u32 = 1 << 1;
    pub const FUNC_1: u32 = 1 << 2;
    pub const SPEED_SHIFT: u32 = 6;
    pub const GIO_MASTER_ENABLE: u32 = 1 << 19;
}

/// RCTL's bits.
mod rctl {
    pub const EN: u32 = 1 << 1;
    pub const MPE: u32 = 1 << 4;
    pub const BAM: u32 = 1 << 15;
    /// Strips the frame check sequence. Buffers of 2048 bytes are BSIZE 0.
    pub const SECRC: u32 = 1 << 26;
}

/// TCTL's bits.
mod tctl {
    pub const EN: u32 = 1 << 1;
    pub const PSP: u32 = 1 << 3;
    pub const CT: u32 = 0x0f << 4;
    pub const COLD: u32 = 63 << 12;
    pub const RTLC: u32 = 1 << 24;
    pub const MULR: u32 = 1 << 28;
}

/// The interrupt causes it raises: transmit descriptor written back, link
/// status change, receive sequence error, receive descriptor minimum
/// threshold, receiver overrun and receive timer.
const CAUSES: u32 = (1 << 0) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7);
/// At most 20,000 interrupts a second: ITR counts 256 ns.
const ITR: u32 = 1_000_000_000 / (20_000 * 256);
/// The interpacket gap for copper: IPGT 8, IPGR1 8, IPGR2 6.
const TIPG: u32 = 8 | (8 << 10) | (6 << 20);

/// MDIC's bits, for the PHY's registers.
mod mdic {
    pub const PHY_SHIFT: u32 = 21;
    pub const REG_SHIFT: u32 = 16;
    pub const OP_WRITE: u32 = 1 << 26;
    pub const OP_READ: u32 = 2 << 26;
    pub const READY: u32 = 1 << 28;
    pub const ERROR: u32 = 1 << 30;
}

/// The PHY's control register (IEEE 802.3 clause 22) and its bits.
const PHY_CONTROL: u32 = 0;
const PHY_RESTART_AUTONEG: u16 = 1 << 9;
const PHY_POWER_DOWN: u16 = 1 << 11;
const PHY_AUTONEG: u16 = 1 << 12;
/// The internal PHY's address on MDIO.
const PHY_ADDRESS: u32 = 1;

/// How long things take.
const RESET_US: u32 = 10_000;
const MASTER_US: u32 = 80_000;
const AUTO_READ_US: u32 = 10_000;
const NVM_US: u32 = 10_000;
const MDIC_US: u32 = 10_000;
const SEMAPHORE_US: u32 = 100_000;
const STEP_US: u32 = 10;

/// The descriptors in each ring.
const RECEIVE: usize = 256;
const TRANSMIT: usize = 256;

/// Which controller a card is: what decides its quirks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mac {
    /// The 82540EM and 82540EP, which QEMU and VirtualBox emulate.
    I82540,
    /// The 82541EI, ER and GI.
    I82541,
    /// The 82545EM and GM, which VMware emulates.
    I82545,
    /// The dual-port 82546EB and GB.
    I82546,
    /// The 82547EI and GI.
    I82547,
    /// The 82571EB and PT, on PCI Express.
    I82571,
    /// The 82572EI.
    I82572,
    /// The 82573E and L.
    I82573,
    /// The 82574L, which QEMU emulates as its e1000e.
    I82574,
    /// The 82583V.
    I82583,
}

impl Mac {
    /// Whether it is on PCI Express: the 82571 and later.
    pub fn pcie(self) -> bool {
        self >= Mac::I82571
    }

    /// Where EERD has its done bit and its address: the 82540, 82545 and
    /// 82546 at bit 4 and from bit 8; the 82541, 82547 and those on PCI
    /// Express at bit 1 and from bit 2.
    fn eerd(self) -> (u32, u32) {
        match self {
            Mac::I82540 | Mac::I82545 | Mac::I82546 => (1 << 4, 8),
            _ => (1 << 1, 2),
        }
    }

    /// Whether its MAC and PHY share MDIO through EXTCNF_CTRL's
    /// software ownership bit.
    fn mdio_semaphore(self) -> bool {
        matches!(self, Mac::I82573 | Mac::I82574 | Mac::I82583)
    }
}

/// The copper cards of the family, by device ID.
pub fn identify(device: u16) -> Option<(&'static str, Mac)> {
    Some(match device {
        0x100e | 0x1015 => ("82540EM", Mac::I82540),
        0x1016 | 0x1017 | 0x101e => ("82540EP", Mac::I82540),
        0x1013 | 0x1018 => ("82541EI", Mac::I82541),
        0x1014 | 0x1078 => ("82541ER", Mac::I82541),
        0x1076 | 0x1077 | 0x107c => ("82541GI", Mac::I82541),
        0x100f => ("82545EM", Mac::I82545),
        0x1026 => ("82545GM", Mac::I82545),
        0x1010 | 0x101d => ("82546EB", Mac::I82546),
        0x1079 | 0x1099 | 0x10b5 => ("82546GB", Mac::I82546),
        0x1019 | 0x101a => ("82547EI", Mac::I82547),
        0x1075 => ("82547GI", Mac::I82547),
        0x105e | 0x10a4 | 0x10bc => ("82571EB", Mac::I82571),
        0x10d5 => ("82571PT", Mac::I82571),
        0x107d | 0x10b9 => ("82572EI", Mac::I82572),
        0x108b | 0x108c => ("82573E", Mac::I82573),
        0x109a => ("82573L", Mac::I82573),
        0x10d3 => ("82574L", Mac::I82574),
        0x10f6 => ("82574LA", Mac::I82574),
        0x150c => ("82583V", Mac::I82583),
        _ => return None,
    })
}

/// A card of the family, running.
pub struct Em {
    regs: Mmio,
    mac: Mac,
    rings: Rings,
    info: Info,
    interrupt: Interrupt,
    /// Whether it was stopped.
    stopped: bool,
}

impl Em {
    /// Resets the card and sets it running: see [`crate::start`].
    pub fn start(
        model: &'static str,
        mac: Mac,
        regs: Mmio,
        interrupt: Interrupt,
        fallback_mac: [u8; 6],
        alloc: &mut dyn FnMut(usize) -> Option<Dma>,
        delay: &mut dyn FnMut(u32),
    ) -> Result<Em, Error> {
        if regs.len() < reg::END {
            return Err(Error::Registers);
        }
        if regs.read32(reg::STATUS) == u32::MAX {
            return Err(Error::Device("its registers read as all ones"));
        }
        reset(&regs, mac, delay)?;
        if mac.pcie() {
            errata(&regs, mac);
        }
        // The link: let the PHY negotiate, and the MAC take its speed.
        let mut control = regs.read32(reg::CTRL);
        control |= ctrl::SLU | ctrl::ASDE;
        control &= !(ctrl::LRST
            | ctrl::PHY_RST
            | ctrl::ILOS
            | ctrl::FRCSPD
            | ctrl::FRCDPX
            | ctrl::VME
            | ctrl::RFCE
            | ctrl::TFCE);
        regs.write32(reg::CTRL, control);
        power_up_phy(&regs, mac, delay);
        let address = read_mac(&regs, mac, delay).unwrap_or(fallback_mac);
        // Its one unicast address, the others off, and no multicast
        // filter: it takes every multicast frame.
        let low = u32::from_le_bytes([address[0], address[1], address[2], address[3]]);
        let high = u32::from(u16::from_le_bytes([address[4], address[5]])) | (1 << 31);
        regs.write32(reg::RAL0, low);
        regs.write32(reg::RAH0, high);
        for entry in 1..16 {
            regs.write32(reg::RAL0 + 8 * entry, 0);
            regs.write32(reg::RAH0 + 8 * entry, 0);
        }
        for word in 0..128 {
            regs.write32(reg::MTA + 4 * word, 0);
        }
        let rings = Rings::new(regs.clone(), legacy::EM, RECEIVE, TRANSMIT, alloc)?;
        // No delays on receive or transmit interrupts: the throttle
        // spaces them.
        regs.write32(reg::RDTR, 0);
        regs.write32(reg::TIDV, 0);
        regs.write32(reg::ITR, ITR);
        regs.write32(reg::TIPG, TIPG);
        let mut transmit = tctl::EN | tctl::PSP | tctl::CT | tctl::COLD | tctl::RTLC;
        if mac.pcie() {
            transmit |= tctl::MULR;
        }
        regs.write32(reg::TCTL, transmit);
        regs.write32(reg::RCTL, rctl::EN | rctl::BAM | rctl::MPE | rctl::SECRC);
        let _ = regs.read32(reg::ICR);
        if interrupt == Interrupt::Msi {
            regs.write32(reg::IMS, CAUSES);
        }
        let (receive_slots, transmit_slots) = rings.sizes();
        let info = Info {
            model,
            mac: address,
            // One receive descriptor is always the card's tail.
            receive_slots: (receive_slots - 1) as u16,
            transmit_slots: (transmit_slots - 1) as u16,
        };
        Ok(Em { regs, mac, rings, info, interrupt, stopped: false })
    }

    /// Which controller it is.
    pub fn mac(&self) -> Mac {
        self.mac
    }

    /// Stops the card: interrupts off, then a reset, which stops its DMA.
    fn halt(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.regs.write32(reg::IMC, u32::MAX);
        self.regs.write32(reg::RCTL, 0);
        self.regs.write32(reg::TCTL, 0);
        let control = self.regs.read32(reg::CTRL);
        self.regs.write32(reg::CTRL, control | ctrl::RST);
    }
}

impl Drop for Em {
    fn drop(&mut self) {
        self.halt();
    }
}

impl Nic for Em {
    fn info(&self) -> Info {
        self.info
    }

    fn link(&self) -> Link {
        let status = self.regs.read32(reg::STATUS);
        let up = status & status::LU != 0 && status != u32::MAX;
        let mbps = match (status >> status::SPEED_SHIFT) & 3 {
            0 => 10,
            1 => 100,
            _ => 1000,
        };
        Link { up, mbps: if up { mbps } else { 0 }, full_duplex: up && status & status::FD != 0 }
    }

    fn failed(&self) -> bool {
        // A card that is gone reads as all ones.
        self.regs.read32(reg::STATUS) == u32::MAX
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        if self.stopped {
            return None;
        }
        if let Some(len) = self.rings.receive(frame) {
            return Some(len);
        }
        if self.interrupt == Interrupt::Msi {
            // Acknowledge the interrupt, then look again for a frame that
            // came in between.
            let _ = self.regs.read32(reg::ICR);
            if self.rings.received() {
                return self.rings.receive(frame);
            }
        }
        None
    }

    fn can_send(&mut self) -> bool {
        !self.stopped && self.rings.can_send()
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        !self.stopped && self.rings.send(frame)
    }

    fn flush(&mut self) {
        if !self.stopped {
            self.rings.flush();
        }
    }

    fn dropped(&self) -> u64 {
        self.rings.dropped()
    }

    fn stop(&mut self) {
        self.halt();
    }
}

/// Resets the card, its interrupts off. On PCI Express, its DMA is
/// stopped first, and the reset is done with MDIO held, as the 82573,
/// 82574 and 82583 share it with their firmware.
fn reset(regs: &Mmio, mac: Mac, delay: &mut dyn FnMut(u32)) -> Result<(), Error> {
    regs.write32(reg::IMC, u32::MAX);
    if mac.pcie() {
        let control = regs.read32(reg::CTRL);
        regs.write32(reg::CTRL, control | ctrl::GIO_MASTER_DISABLE);
        if !wait_for(delay, MASTER_US, 100, || {
            regs.read32(reg::STATUS) & status::GIO_MASTER_ENABLE == 0
        }) {
            return Err(Error::Timeout("stopping its DMA before a reset"));
        }
    }
    regs.write32(reg::RCTL, 0);
    regs.write32(reg::TCTL, tctl::PSP);
    let _ = regs.read32(reg::STATUS);
    delay(10_000);
    let held = mac.mdio_semaphore() && take_mdio(regs, delay);
    let control = regs.read32(reg::CTRL);
    regs.write32(reg::CTRL, control | ctrl::RST);
    // The reset takes a few microseconds; the card must not be touched
    // meanwhile.
    delay(20);
    let reset_done = wait_for(delay, RESET_US, STEP_US, || regs.read32(reg::CTRL) & ctrl::RST == 0);
    if held {
        give_mdio(regs);
    }
    if !reset_done {
        return Err(Error::Timeout("a reset"));
    }
    if mac.pcie() {
        // It reads its NVM again after the reset.
        if !wait_for(delay, AUTO_READ_US, STEP_US, || regs.read32(reg::EECD) & (1 << 9) != 0) {
            return Err(Error::Timeout("reading its NVM after a reset"));
        }
        match mac {
            Mac::I82571 | Mac::I82572 => {
                // Let go of the NVM, which the reset may leave requested.
                let eecd = regs.read32(reg::EECD);
                regs.write32(reg::EECD, eecd & !((1 << 6) | (1 << 7)));
            }
            _ => delay(25_000),
        }
    } else {
        delay(5_000);
    }
    regs.write32(reg::IMC, u32::MAX);
    let _ = regs.read32(reg::ICR);
    Ok(())
}

/// The settings the 82571 family's specification updates ask for, as
/// FreeBSD's e1000_initialize_hw_bits_82571 applies them.
fn errata(regs: &Mmio, mac: Mac) {
    let set = |at: usize, on: u32, off: u32| {
        let value = regs.read32(at);
        regs.write32(at, (value & !off) | on);
    };
    set(reg::TXDCTL0, 1 << 22, 0);
    set(reg::TXDCTL1, 1 << 22, 0);
    match mac {
        Mac::I82571 | Mac::I82572 => {
            set(reg::TARC0, (1 << 23) | (1 << 24) | (1 << 25) | (1 << 26), 0xf << 27);
            // The transmitter is set up with MULR, so bit 28 is off.
            set(reg::TARC1, (1 << 22) | (1 << 24) | (1 << 25) | (1 << 26), (1 << 28) | (3 << 29));
            // DMA dynamic clock gating off.
            set(reg::CTRL_EXT, 0, 1 << 19);
        }
        Mac::I82574 | Mac::I82583 => set(reg::TARC0, 1 << 26, 0xf << 27),
        _ => set(reg::TARC0, 0, 0xf << 27),
    }
    if mac.mdio_semaphore() {
        set(reg::CTRL, 0, ctrl::BIT_29);
        set(reg::CTRL_EXT, 1 << 22, 1 << 23);
    }
    if mac == Mac::I82571 {
        set(reg::PBA_ECC, 1, 0);
    }
    if mac <= Mac::I82573 {
        // Malformed IPv6 extension headers can hang the receiver.
        set(reg::RFCTL, (1 << 16) | (1 << 17), 0);
    }
    if matches!(mac, Mac::I82574 | Mac::I82583) {
        set(reg::GCR, 1 << 22, 0);
        set(reg::GCR2, 1, 0);
    }
}

/// Takes MDIO from the card's firmware: whether it was had.
fn take_mdio(regs: &Mmio, delay: &mut dyn FnMut(u32)) -> bool {
    const OWNED: u32 = 1 << 5;
    wait_for(delay, SEMAPHORE_US, 1_000, || {
        let value = regs.read32(reg::EXTCNF_CTRL);
        regs.write32(reg::EXTCNF_CTRL, value | OWNED);
        regs.read32(reg::EXTCNF_CTRL) & OWNED != 0
    })
}

fn give_mdio(regs: &Mmio) {
    let value = regs.read32(reg::EXTCNF_CTRL);
    regs.write32(reg::EXTCNF_CTRL, value & !(1 << 5));
}

/// Reads PHY register `register`.
fn phy_read(regs: &Mmio, register: u32, delay: &mut dyn FnMut(u32)) -> Option<u16> {
    regs.write32(
        reg::MDIC,
        (register << mdic::REG_SHIFT) | (PHY_ADDRESS << mdic::PHY_SHIFT) | mdic::OP_READ,
    );
    let mut value = 0;
    let done = wait_for(delay, MDIC_US, 50, || {
        value = regs.read32(reg::MDIC);
        value & mdic::READY != 0
    });
    (done && value & mdic::ERROR == 0).then_some(value as u16)
}

/// Writes PHY register `register`: whether the card took it.
fn phy_write(regs: &Mmio, register: u32, data: u16, delay: &mut dyn FnMut(u32)) -> bool {
    regs.write32(
        reg::MDIC,
        u32::from(data)
            | (register << mdic::REG_SHIFT)
            | (PHY_ADDRESS << mdic::PHY_SHIFT)
            | mdic::OP_WRITE,
    );
    let mut value = 0;
    wait_for(delay, MDIC_US, 50, || {
        value = regs.read32(reg::MDIC);
        value & mdic::READY != 0
    }) && value & mdic::ERROR == 0
}

/// Powers the PHY up, if a reset or the firmware left it down, and has
/// it negotiate the link. A PHY that does not answer is left as it is:
/// the link may still come up.
fn power_up_phy(regs: &Mmio, mac: Mac, delay: &mut dyn FnMut(u32)) {
    let held = mac.mdio_semaphore() && take_mdio(regs, delay);
    if (held || !mac.mdio_semaphore())
        && let Some(control) = phy_read(regs, PHY_CONTROL, delay)
        && (control & PHY_POWER_DOWN != 0 || control & PHY_AUTONEG == 0)
    {
        let control = (control & !PHY_POWER_DOWN) | PHY_AUTONEG | PHY_RESTART_AUTONEG;
        let _ = phy_write(regs, PHY_CONTROL, control, delay);
    }
    if held {
        give_mdio(regs);
    }
}

/// Reads word `word` of the card's NVM through EERD.
fn nvm_word(regs: &Mmio, mac: Mac, word: u32, delay: &mut dyn FnMut(u32)) -> Option<u16> {
    let (done, shift) = mac.eerd();
    regs.write32(reg::EERD, (word << shift) | 1);
    let mut value = 0;
    wait_for(delay, NVM_US, STEP_US, || {
        value = regs.read32(reg::EERD);
        value & done != 0
    })
    .then_some((value >> 16) as u16)
}

/// The card's MAC address: on the 8254x from its NVM's first three words
/// (emulated 8254x cards do not always load it into RAL0 and RAH0), with
/// the second port of a dual-port card one more; on PCI Express from
/// RAL0 and RAH0, which the card loaded from its NVM. Each falls back on
/// the other; `None` when neither has an address.
fn read_mac(regs: &Mmio, mac: Mac, delay: &mut dyn FnMut(u32)) -> Option<[u8; 6]> {
    let from_registers = || {
        let low = regs.read32(reg::RAL0).to_le_bytes();
        let high = regs.read32(reg::RAH0).to_le_bytes();
        let address = [low[0], low[1], low[2], low[3], high[0], high[1]];
        usable_mac(address).then_some(address)
    };
    let mut from_nvm = || {
        let mut address = [0u8; 6];
        for word in 0..3 {
            let value = nvm_word(regs, mac, word, delay)?.to_le_bytes();
            address[2 * word as usize] = value[0];
            address[2 * word as usize + 1] = value[1];
        }
        if mac == Mac::I82546 && regs.read32(reg::STATUS) & status::FUNC_1 != 0 {
            address[5] ^= 1;
        }
        usable_mac(address).then_some(address)
    };
    if mac.pcie() { from_registers().or_else(from_nvm) } else { from_nvm().or_else(from_registers) }
}
