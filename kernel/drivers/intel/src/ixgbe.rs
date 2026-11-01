// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The ixgbe family: Intel's 10 gigabit controllers, the 82599 (the X520
//! cards, with SFP+ cages or a backplane), and the X540 and X550, whose
//! PHYs are copper, 10GBASE-T (their datasheets).
//!
//! Their registers are not the e1000's: the interrupts are the extended
//! ones (EICR), queue 0 is at 0x1000 and 0x6000, the receiver and the
//! transmitter are turned on by RXCTRL and DMATXCTL, and the link is in
//! LINKS. Their descriptors are igb's advanced ones, with the frame's
//! errors in other bits.
//!
//! The driver stops the card's DMA and resets it (the X540's and X550's
//! with the PHY's semaphore the firmware shares held), tells the firmware
//! a driver has it, and takes the MAC address the card loaded into its
//! first receive address register. The link is the NVM's: the X540's and
//! X550's PHYs negotiate it by themselves, and the 82599 is asked to
//! negotiate it again as its NVM set it up, as iPXE's driver does, which
//! brings up direct-attach cables and most modules; the init sequences
//! some modules need from the NVM are not run yet. Queue pair 0 runs with
//! one buffer a descriptor, each written back once done. With an MSI the
//! card raises it for queue 0's frames and for link changes, at most
//! 20,000 times a second. FreeBSD's ix driver (BSD-3-Clause) was the
//! reference for the sequences.

use radix_hw::{Dma, Mmio};
use radix_nic::{Error, Link, wait_for};

use crate::rings::{Format, Offsets, Rings};
use crate::shared::{self, CTRL_EXT_DRV_LOAD};
use crate::{Card, Interrupt, Ops};

/// The registers, from the start of BAR 0.
mod reg {
    pub const CTRL: usize = 0x0_0000;
    pub const STATUS: usize = 0x0_0008;
    pub const CTRL_EXT: usize = 0x0_0018;
    pub const EICR: usize = 0x0_0800;
    pub const EITR0: usize = 0x0_0820;
    pub const EIMS: usize = 0x0_0880;
    pub const EIMC: usize = 0x0_0888;
    pub const GPIE: usize = 0x0_0898;
    pub const IVAR0: usize = 0x0_0900;
    pub const RXDCTL0: usize = 0x0_1028;
    pub const SRRCTL0: usize = 0x0_2100;
    pub const RDRXCTL: usize = 0x0_2f00;
    pub const RXPBSIZE0: usize = 0x0_3c00;
    pub const RXCTRL: usize = 0x0_3000;
    pub const HLREG0: usize = 0x0_4240;
    pub const AUTOC: usize = 0x0_42a0;
    pub const LINKS: usize = 0x0_42a4;
    pub const AUTOC2: usize = 0x0_42a8;
    pub const DMATXCTL: usize = 0x0_4a80;
    pub const FCTRL: usize = 0x0_5080;
    pub const MRQC: usize = 0x0_5818;
    pub const MPSAR_LO0: usize = 0x0_a600;
    pub const TXDCTL0: usize = 0x0_6028;
    pub const SECRXCTRL: usize = 0x0_8d00;
    pub const SECRXSTAT: usize = 0x0_8d04;
    pub const SWSM: usize = 0x1_0140;
    pub const SW_FW_SYNC: usize = 0x1_0160;
    /// The last register the driver touches, and its size.
    pub const END: usize = SW_FW_SYNC + 4;
}

/// Queue 0's ring registers.
const QUEUE_0: Offsets = Offsets {
    rdbal: 0x1000,
    rdbah: 0x1004,
    rdlen: 0x1008,
    rdh: 0x1010,
    rdt: 0x1018,
    tdbal: 0x6000,
    tdbah: 0x6004,
    tdlen: 0x6008,
    tdh: 0x6010,
    tdt: 0x6018,
};

/// CTRL's bits: the MAC's reset, and the link's with it.
const CTRL_RST: u32 = 1 << 26;
const CTRL_LNK_RST: u32 = 1 << 3;
/// STATUS's LAN ID, which port of the card this is.
const STATUS_LAN_ID_SHIFT: u32 = 2;
/// RXCTRL's receive enable.
const RXEN: u32 = 1 << 0;
/// HLREG0's bits: add the frame check sequence to frames sent, strip it
/// from frames received, and pad short frames.
const HLREG0_TXCRCEN: u32 = 1 << 0;
const HLREG0_RXCRCSTRP: u32 = 1 << 1;
const HLREG0_JUMBOEN: u32 = 1 << 2;
const HLREG0_TXPADEN: u32 = 1 << 10;
/// RDRXCTL's bits: strip the frame check sequence, which HLREG0 must say
/// too; the first-packet size of coalescing, cleared; and two bits the
/// datasheet says must be set.
const RDRXCTL_CRCSTRIP: u32 = 1 << 1;
const RDRXCTL_RSCFRSTSIZE: u32 = 0x1f << 17;
const RDRXCTL_RSCACKC: u32 = 1 << 25;
const RDRXCTL_FCOE_WRFIX: u32 = 1 << 26;
/// FCTRL's bits: take broadcasts and every multicast frame.
const FCTRL_MPE: u32 = 1 << 8;
const FCTRL_BAM: u32 = 1 << 10;
/// DMATXCTL's transmit enable.
const DMATXCTL_TE: u32 = 1 << 0;
/// SECRXCTRL's bit that holds the receive path, and SECRXSTAT's that
/// says it is held.
const SECRX_DIS: u32 = 1 << 1;
const SECRX_READY: u32 = 1 << 0;
/// AUTOC's restart of autonegotiation, and AUTOC2's bits that keep the
/// link down.
const AUTOC_AN_RESTART: u32 = 1 << 12;
const AUTOC2_LINK_DISABLE: u32 = 0x7 << 28;
/// LINKS's bits: up, its speed, and the X550's other speeds.
const LINKS_UP: u32 = 1 << 30;
const LINKS_SPEED_SHIFT: u32 = 28;
const LINKS_NON_STD: u32 = 1 << 27;
/// SWSM's semaphore between drivers, and SW_FW_SYNC's between software
/// and the firmware, with each port's PHY bit; the firmware's are 5 bits
/// higher.
const SWSM_SMBI: u32 = 1 << 0;
const SW_FW_REGSMP: u32 = 1 << 31;
const PHY_SEMAPHORES: [u32; 2] = [0x02, 0x04];
/// The receive packet buffer the X540 and X550 are given, in KiB.
const RX_PACKET_BUFFER_KB: u32 = 384;

/// SRRCTL: advanced descriptors of one buffer of 2 KiB, with the header
/// size Linux gives it.
const SRRCTL: u32 = 0x0200_0000 | (4 << 8) | 2;
/// RXDCTL's and TXDCTL's queue enable bit and thresholds: TXDCTL's
/// prefetch 32 and host 1, and each descriptor written back once done.
const QUEUE_ENABLE: u32 = 1 << 25;
const RXDCTL: u32 = QUEUE_ENABLE;
const TXDCTL: u32 = QUEUE_ENABLE | 32 | (1 << 8) | (1 << 16);
/// The interrupt's causes: queue 0's (vector 0's bit, where IVAR0 puts
/// both its directions) and the link's.
const CAUSES: u32 = (1 << 0) | (1 << 20);
const IVAR0: u32 = 0x8080;
/// At most 20,000 interrupts a second: EITR's interval of 50 us, in 2 us
/// from bit 3, with its counter left alone when it is written.
const EITR: u32 = (25 << 3) | (1 << 31);

/// How long things take.
const MASTER_US: u32 = 80_000;
const RESET_US: u32 = 10;
const SEMAPHORE_US: u32 = 10_000;
const QUEUE_US: u32 = 10_000;
const STEP_US: u32 = 10;

/// The descriptors in each ring.
const RECEIVE: usize = 256;
const TRANSMIT: usize = 256;

/// Which controller a card is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mac {
    /// The 82599, on X520 cards.
    I82599,
    /// The X540.
    X540,
    /// The X550.
    X550,
}

impl Mac {
    /// Whether its PHY is the firmware's too, held by the semaphore
    /// around a reset.
    fn shares_phy(self) -> bool {
        self != Mac::I82599
    }
}

/// The cards of the family the driver knows, by device ID.
pub fn identify(device: u16) -> Option<(&'static str, Mac)> {
    Some(match device {
        0x10fb | 0x154d => ("82599ES", Mac::I82599),
        0x1557 => ("82599EN", Mac::I82599),
        0x10f7 => ("82599 KX4", Mac::I82599),
        0x1528 => ("X540-T2", Mac::X540),
        0x1560 => ("X540-T1", Mac::X540),
        0x1563 => ("X550-T2", Mac::X550),
        0x15d1 => ("X550-T1", Mac::X550),
        _ => return None,
    })
}

/// Resets the card `mac`, a `model`, and sets it running: see
/// [`crate::start`].
pub fn start(
    model: &'static str,
    mac: Mac,
    regs: Mmio,
    interrupt: Interrupt,
    fallback_mac: [u8; 6],
    alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    delay: &mut dyn FnMut(u32),
) -> Result<Card, Error> {
    if regs.len() < reg::END {
        return Err(Error::Registers);
    }
    if regs.read32(reg::STATUS) == u32::MAX {
        return Err(Error::Device("its registers read as all ones"));
    }
    reset(&regs, mac, delay)?;
    shared::update(&regs, reg::CTRL_EXT, CTRL_EXT_DRV_LOAD, 0);
    if mac.shares_phy() {
        regs.write32(reg::RXPBSIZE0, RX_PACKET_BUFFER_KB << 10);
    }
    let address = shared::registers_mac(&regs).unwrap_or(fallback_mac);
    shared::set_address(&regs, address);
    // The address belongs to pool 0, the one there is.
    regs.write32(reg::MPSAR_LO0, 1);
    // Frames of up to 1518 bytes, their check sequences added and
    // stripped, short ones padded; every multicast frame and broadcasts;
    // one queue.
    shared::update(
        &regs,
        reg::HLREG0,
        HLREG0_TXCRCEN | HLREG0_RXCRCSTRP | HLREG0_TXPADEN,
        HLREG0_JUMBOEN,
    );
    shared::update(
        &regs,
        reg::RDRXCTL,
        RDRXCTL_CRCSTRIP | RDRXCTL_RSCACKC | RDRXCTL_FCOE_WRFIX,
        RDRXCTL_RSCFRSTSIZE,
    );
    regs.write32(reg::FCTRL, FCTRL_BAM | FCTRL_MPE);
    regs.write32(reg::MRQC, 0);
    // The queues off while their rings are set up, then on: the
    // transmitter's DMA first, as the datasheet has it.
    regs.write32(reg::RXDCTL0, 0);
    regs.write32(reg::TXDCTL0, 0);
    let mut rings = Rings::new(regs.clone(), QUEUE_0, Format::Ixgbe, RECEIVE, TRANSMIT, alloc)?;
    regs.write32(reg::SRRCTL0, SRRCTL);
    shared::update(&regs, reg::DMATXCTL, DMATXCTL_TE, 0);
    regs.write32(reg::RXDCTL0, RXDCTL);
    regs.write32(reg::TXDCTL0, TXDCTL);
    let mut on = |at| wait_for(delay, QUEUE_US, STEP_US, || regs.read32(at) & QUEUE_ENABLE != 0);
    if !on(reg::RXDCTL0) || !on(reg::TXDCTL0) {
        (mac.ops().halt)(&regs);
        return Err(Error::Timeout("turning its queues on"));
    }
    rings.hand_over();
    if interrupt == Interrupt::Msi {
        regs.write32(reg::GPIE, 0);
        regs.write32(reg::IVAR0, IVAR0);
        regs.write32(reg::EITR0, EITR);
    }
    // The receiver on, with the security block's receive path held
    // meanwhile.
    shared::update(&regs, reg::SECRXCTRL, SECRX_DIS, 0);
    let _ = wait_for(delay, QUEUE_US, STEP_US, || regs.read32(reg::SECRXSTAT) & SECRX_READY != 0);
    shared::update(&regs, reg::RXCTRL, RXEN, 0);
    shared::update(&regs, reg::SECRXCTRL, 0, SECRX_DIS);
    if mac == Mac::I82599 {
        // The link as the NVM set it up, negotiated again.
        shared::update(&regs, reg::AUTOC, AUTOC_AN_RESTART, 0);
    }
    let _ = regs.read32(reg::EICR);
    if interrupt == Interrupt::Msi {
        regs.write32(reg::EIMS, CAUSES);
    }
    Ok(Card::new(model, address, regs, rings, interrupt, mac.ops()))
}

static OPS: Ops = Ops { icr: reg::EICR, link, halt };
static OPS_SHARED_PHY: Ops = Ops { icr: reg::EICR, link, halt: halt_shared_phy };

impl Mac {
    fn ops(self) -> &'static Ops {
        if self.shares_phy() { &OPS_SHARED_PHY } else { &OPS }
    }
}

/// The link, as LINKS says: always full duplex.
fn link(regs: &Mmio) -> Link {
    let links = regs.read32(reg::LINKS);
    let up = links & LINKS_UP != 0 && links != u32::MAX;
    let other = links & LINKS_NON_STD != 0;
    let mbps = match ((links >> LINKS_SPEED_SHIFT) & 3, other) {
        (3, _) => 10_000,
        (2, false) => 1_000,
        (2, true) => 2_500,
        (1, false) => 100,
        (1, true) => 5_000,
        _ => 10,
    };
    Link { up, mbps: if up { mbps } else { 0 }, full_duplex: up }
}

/// Stops the card: interrupts, receiver and queues off, the card given
/// back to its firmware, then a reset, which stops its DMA.
fn halt(regs: &Mmio) {
    quiet(regs);
    shared::update(regs, reg::CTRL, CTRL_RST, 0);
}

/// Stops an X540 or X550, its reset made with the PHY's semaphore held
/// as at the start, but without waiting for it: a card being stopped is
/// not waited on, and the semaphore is only looked at a few hundred times.
fn halt_shared_phy(regs: &Mmio) {
    quiet(regs);
    let mut no_wait = |_| {};
    let phy = phy_semaphore(regs);
    let held = take_semaphore(regs, phy, &mut no_wait);
    shared::update(regs, reg::CTRL, CTRL_RST, 0);
    if held {
        give_semaphore(regs, phy, &mut no_wait);
    }
}

/// Turns the card's interrupts, receiver, queues and transmit DMA off,
/// and gives it back to its firmware.
fn quiet(regs: &Mmio) {
    regs.write32(reg::EIMC, u32::MAX);
    shared::update(regs, reg::RXCTRL, 0, RXEN);
    regs.write32(reg::RXDCTL0, 0);
    regs.write32(reg::TXDCTL0, 0);
    shared::update(regs, reg::DMATXCTL, 0, DMATXCTL_TE);
    shared::update(regs, reg::CTRL_EXT, 0, CTRL_EXT_DRV_LOAD);
}

/// The semaphore bit of this port's PHY.
fn phy_semaphore(regs: &Mmio) -> u32 {
    PHY_SEMAPHORES[((regs.read32(reg::STATUS) >> STATUS_LAN_ID_SHIFT) & 1) as usize]
}

/// Stops the card and resets it, as FreeBSD's driver does: its
/// receiver, interrupts and queues off, its DMA stopped, then the MAC
/// reset, and the link with it if the link is down; the reset clears
/// itself.
fn reset(regs: &Mmio, mac: Mac, delay: &mut dyn FnMut(u32)) -> Result<(), Error> {
    shared::update(regs, reg::RXCTRL, 0, RXEN);
    regs.write32(reg::EIMC, u32::MAX);
    let _ = regs.read32(reg::EICR);
    regs.write32(reg::RXDCTL0, 0);
    regs.write32(reg::TXDCTL0, 0);
    delay(2_000);
    let _ = shared::stop_dma(regs, MASTER_US, delay);
    let phy = phy_semaphore(regs);
    let held = mac.shares_phy() && take_semaphore(regs, phy, delay);
    let link_reset = mac == Mac::I82599 && regs.read32(reg::LINKS) & LINKS_UP == 0;
    let bits = if link_reset { CTRL_LNK_RST } else { CTRL_RST };
    shared::update(regs, reg::CTRL, bits, 0);
    if held {
        give_semaphore(regs, phy, delay);
    }
    let reset = CTRL_RST | CTRL_LNK_RST;
    let done = wait_for(delay, RESET_US, 1, || regs.read32(reg::CTRL) & reset == 0);
    delay(if mac.shares_phy() { 100_000 } else { 50_000 });
    if !done {
        return Err(Error::Timeout("resetting"));
    }
    if mac == Mac::I82599 {
        shared::update(regs, reg::AUTOC2, 0, AUTOC2_LINK_DISABLE);
    }
    regs.write32(reg::EIMC, u32::MAX);
    let _ = regs.read32(reg::EICR);
    Ok(())
}

/// Takes the semaphores that guard SW_FW_SYNC: software's, which reading
/// SWSM sets, then the register's own, which reading SW_FW_SYNC sets.
fn take_hardware_semaphore(regs: &Mmio, delay: &mut dyn FnMut(u32)) -> bool {
    if !wait_for(delay, SEMAPHORE_US, 50, || regs.read32(reg::SWSM) & SWSM_SMBI == 0) {
        return false;
    }
    let had =
        wait_for(delay, SEMAPHORE_US, 50, || regs.read32(reg::SW_FW_SYNC) & SW_FW_REGSMP == 0);
    if !had {
        shared::update(regs, reg::SWSM, 0, SWSM_SMBI);
    }
    had
}

fn give_hardware_semaphore(regs: &Mmio) {
    shared::update(regs, reg::SW_FW_SYNC, 0, SW_FW_REGSMP);
    shared::update(regs, reg::SWSM, 0, SWSM_SMBI);
}

/// Takes `mask`'s resource from the firmware: whether it was had.
fn take_semaphore(regs: &Mmio, mask: u32, delay: &mut dyn FnMut(u32)) -> bool {
    for _ in 0..200 {
        if !take_hardware_semaphore(regs, delay) {
            return false;
        }
        let sync = regs.read32(reg::SW_FW_SYNC);
        if sync & (mask | (mask << 5)) == 0 {
            regs.write32(reg::SW_FW_SYNC, sync | mask);
            give_hardware_semaphore(regs);
            return true;
        }
        give_hardware_semaphore(regs);
        delay(5_000);
    }
    false
}

/// Gives `mask`'s resource back to the firmware.
fn give_semaphore(regs: &Mmio, mask: u32, delay: &mut dyn FnMut(u32)) {
    let had = take_hardware_semaphore(regs, delay);
    shared::update(regs, reg::SW_FW_SYNC, 0, mask);
    if had {
        give_hardware_semaphore(regs);
    }
}
