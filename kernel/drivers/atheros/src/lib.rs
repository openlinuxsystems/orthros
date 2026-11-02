// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Atheros' gigabit and fast Ethernet controllers (ADR 0197), on many
//! laptops and desktop boards from 2009 to 2018: the AR8131 and AR8132
//! (L1C and L2C) and the AR8151 and AR8152 (L1D and L2C v2), and the
//! AR8161, AR8162, AR8171 and AR8172 with the Killer E2200, E2400 and
//! E2500 that are built on them. FreeBSD's `alc(4)` drives them all;
//! Linux's `atl1c` the first family and `alx` the second.
//!
//! The two families share their rings and most registers, and differ in
//! how their PHY is reached and reset, their PCI Express power states and
//! a few registers each family alone has ([`Chip::ar816x`]). The driver
//! resets the PHY with the vendor's values, as FreeBSD's does, resets
//! the chip, reads the MAC address it loads from its EEPROM, and drives
//! it with one ring of 256 descriptors to send, one of 256 buffers to
//! receive into and one of 256 return descriptors where the chip says
//! what it received ([`rings`]). The receive buffers are 1536 bytes, so
//! a longer frame comes in several, and is dropped; there are no
//! offloads, no VLAN tag stripping, and the card takes every multicast
//! frame, which IPv6 and mDNS need, and broadcasts.
//!
//! The chip does not configure its MAC for the link its PHY negotiated:
//! the driver looks at the PHY once a second, from [`Nic::receive`],
//! which the kernel calls at least that often, and sets the MAC's speed
//! and duplex and turns it on when the link comes up, as FreeBSD's
//! does from its tick. With an MSI the card raises it for frames received
//! and sent and for errors; writing the causes back to its interrupt
//! status register, which the driver does once no frame is left to take,
//! acknowledges them.
//!
//! The driver is written from FreeBSD's `alc(4)` and `atphy(4)`
//! (BSD-2-Clause, Pyun YongHyeon) as the reference for the registers,
//! the families and their quirks (credits/freebsd-network-drivers.md);
//! no code is taken from them. It loads no firmware: the chips have none.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod phy;
mod rings;

use radix_hw::{Dma, Mmio};
pub use radix_nic::Interrupt;
use radix_nic::{Error, Info, Link, Nic, usable_mac, wait_for};

use crate::rings::Rings;

/// Atheros' PCI vendor ID.
pub const VENDOR: u16 = 0x1969;

/// The PCI device IDs.
pub mod device {
    /// The AR8131, L1C: gigabit.
    pub const AR8131: u16 = 0x1063;
    /// The AR8132, L2C: 10 and 100 Mb/s.
    pub const AR8132: u16 = 0x1062;
    /// The AR8151 v1.0, L1D: gigabit.
    pub const AR8151: u16 = 0x1073;
    /// The AR8151 v2.0.
    pub const AR8151_V2: u16 = 0x1083;
    /// The AR8152 v1.1, L2C v2: 10 and 100 Mb/s.
    pub const AR8152_B: u16 = 0x2060;
    /// The AR8152 v2.0.
    pub const AR8152_B2: u16 = 0x2062;
    /// The AR8161: gigabit.
    pub const AR8161: u16 = 0x1091;
    /// The AR8162: 10 and 100 Mb/s.
    pub const AR8162: u16 = 0x1090;
    /// The AR8171: gigabit.
    pub const AR8171: u16 = 0x10a1;
    /// The AR8172: 10 and 100 Mb/s.
    pub const AR8172: u16 = 0x10a0;
    /// The Killer E2200, built on the AR8161.
    pub const E2200: u16 = 0xe091;
    /// The Killer E2400.
    pub const E2400: u16 = 0xe0a1;
    /// The Killer E2500.
    pub const E2500: u16 = 0xe0b1;
}

/// The registers, from the start of the chip's memory BAR, which also
/// mirrors its PCI configuration space from offset 0.
mod reg {
    pub const PEX_UNC_ERR_SEV: usize = 0x10c;
    pub const EEPROM_LD: usize = 0x204;
    /// TWSI's configuration on the AR813x, its station address loader on
    /// the AR816x: the same register.
    pub const TWSI_CFG: usize = 0x218;
    pub const PCIE_PHYMISC: usize = 0x1000;
    pub const PCIE_PHYMISC2: usize = 0x1004;
    pub const PDLL_TRNS1: usize = 0x1104;
    pub const TWSI_DEBUG: usize = 0x1108;
    pub const OPT_CFG: usize = 0x12f0;
    pub const PM_CFG: usize = 0x12f8;
    pub const LTSSM_ID_CFG: usize = 0x12fc;
    pub const MASTER_CFG: usize = 0x1400;
    pub const IM_TIMER: usize = 0x1408;
    pub const GPHY_CFG: usize = 0x140c;
    pub const IDLE_STATUS: usize = 0x1410;
    pub const MDIO: usize = 0x1414;
    pub const SERDES_LOCK: usize = 0x1424;
    pub const LPI_CTL: usize = 0x1440;
    pub const EXT_MDIO: usize = 0x1448;
    pub const IDLE_DECISN_TIMER: usize = 0x1474;
    pub const MAC_CFG: usize = 0x1480;
    pub const IPG_IFG_CFG: usize = 0x1484;
    pub const PAR0: usize = 0x1488;
    pub const PAR1: usize = 0x148c;
    pub const MAR0: usize = 0x1490;
    pub const MAR1: usize = 0x1494;
    pub const HDPX_CFG: usize = 0x1498;
    pub const FRAME_SIZE: usize = 0x149c;
    pub const WOL_CFG: usize = 0x14a0;
    pub const RSS_IDT_TABLE0: usize = 0x14e0;
    pub const SRAM_RD0_ADDR: usize = 0x1500;
    pub const SRAM_TD_ADDR: usize = 0x1518;
    pub const SRAM_RX_FIFO_ADDR: usize = 0x1520;
    pub const SRAM_RX_FIFO_LEN: usize = 0x1524;
    pub const SRAM_TX_FIFO_ADDR: usize = 0x1528;
    pub const SRAM_TX_FIFO_LEN: usize = 0x152c;
    pub const DMA_BLOCK: usize = 0x1534;
    pub const RX_BASE_ADDR_HI: usize = 0x1540;
    pub const TX_BASE_ADDR_HI: usize = 0x1544;
    pub const SMB_BASE_ADDR_HI: usize = 0x1548;
    pub const SMB_BASE_ADDR_LO: usize = 0x154c;
    pub const RD0_HEAD_ADDR_LO: usize = 0x1550;
    pub const RD1_HEAD_ADDR_LO: usize = 0x1554;
    pub const RD2_HEAD_ADDR_LO: usize = 0x1558;
    pub const RD3_HEAD_ADDR_LO: usize = 0x155c;
    pub const RD_RING_CNT: usize = 0x1560;
    pub const RX_BUF_SIZE: usize = 0x1564;
    pub const RRD0_HEAD_ADDR_LO: usize = 0x1568;
    pub const RRD1_HEAD_ADDR_LO: usize = 0x156c;
    pub const RRD2_HEAD_ADDR_LO: usize = 0x1570;
    pub const RRD3_HEAD_ADDR_LO: usize = 0x1574;
    pub const RRD_RING_CNT: usize = 0x1578;
    pub const TDH_HEAD_ADDR_LO: usize = 0x157c;
    pub const TDL_HEAD_ADDR_LO: usize = 0x1580;
    pub const TD_RING_CNT: usize = 0x1584;
    pub const CMB_BASE_ADDR_LO: usize = 0x1588;
    pub const TXQ_CFG: usize = 0x1590;
    pub const TSO_OFFLOAD_THRESH: usize = 0x1594;
    pub const TXF_WATER_MARK: usize = 0x1598;
    pub const RXQ_CFG: usize = 0x15a0;
    pub const RX_RD_FREE_THRESH: usize = 0x15a4;
    pub const RX_FIFO_PAUSE_THRESH: usize = 0x15a8;
    pub const RD_DMA_CFG: usize = 0x15ac;
    pub const RSS_CPU: usize = 0x15b8;
    pub const DMA_CFG: usize = 0x15c0;
    pub const SMB_STAT_TIMER: usize = 0x15c4;
    pub const CMB_TD_THRESH: usize = 0x15c8;
    pub const CMB_TX_TIMER: usize = 0x15cc;
    pub const MBOX_RD0_PROD_IDX: usize = 0x15e0;
    /// The transmit producer: the AR813x's normal queue in the top 16
    /// bits of the word at 0x15f0, the AR816x's in the half at 0x15f2.
    pub const MBOX_TD_PROD_IDX: usize = 0x15f0;
    pub const MBOX_TD_PRI0_PROD_IDX: usize = 0x15f2;
    /// The transmit consumer, laid out as the producer.
    pub const MBOX_TD_CONS_IDX: usize = 0x15f4;
    pub const MBOX_TD_PRI0_CONS_IDX: usize = 0x15f6;
    pub const INTR_STATUS: usize = 0x1600;
    pub const INTR_MASK: usize = 0x1604;
    pub const INTR_RETRIG_TIMER: usize = 0x1608;
    pub const HDS_CFG: usize = 0x160c;
    pub const CLK_GATING_CFG: usize = 0x1814;
    pub const WRR: usize = 0x1938;
    pub const HQTD_CFG: usize = 0x193c;
    pub const MISC: usize = 0x19c0;
    pub const MISC2: usize = 0x19c8;
    pub const MISC3: usize = 0x19cc;
    /// The last register the driver touches, and its size.
    pub const END: usize = MISC3 + 4;
}

/// The master configuration's bits.
mod master {
    pub const RESET: u32 = 0x0000_0001;
    pub const WAKEN_25M: u32 = 0x0000_0020;
    pub const OOB_DIS_OFF: u32 = 0x0000_0040;
    pub const SA_TIMER_ENB: u32 = 0x0000_0080;
    pub const IM_TX_TIMER_ENB: u32 = 0x0000_0400;
    pub const IM_RX_TIMER_ENB: u32 = 0x0000_0800;
    pub const CLK_SEL_DIS: u32 = 0x0000_1000;
    pub const OTP_SEL: u32 = 0x8000_0000;
}

/// The idle status's bits: each part of the chip is idle.
mod idle {
    pub const RXMAC: u32 = 0x01;
    pub const TXMAC: u32 = 0x02;
    pub const RXQ: u32 = 0x04;
    pub const TXQ: u32 = 0x08;
}

/// The MAC configuration's bits.
mod mac {
    pub const TX_ENB: u32 = 0x0000_0001;
    pub const RX_ENB: u32 = 0x0000_0002;
    pub const TX_FC: u32 = 0x0000_0004;
    pub const RX_FC: u32 = 0x0000_0008;
    pub const FULL_DUPLEX: u32 = 0x0000_0020;
    pub const TX_CRC_ENB: u32 = 0x0000_0040;
    pub const TX_AUTO_PAD: u32 = 0x0000_0080;
    /// The preamble's length, 7 bytes.
    pub const PREAMBLE: u32 = 7 << 10;
    pub const VLAN_TAG_STRIP: u32 = 0x0000_4000;
    pub const PROMISC: u32 = 0x0000_8000;
    pub const SPEED_MASK: u32 = 0x0030_0000;
    pub const SPEED_10_100: u32 = 0x0010_0000;
    pub const SPEED_1000: u32 = 0x0020_0000;
    pub const ALLMULTI: u32 = 0x0200_0000;
    pub const BCAST: u32 = 0x0400_0000;
    pub const HASH_ALG_CRC32: u32 = 0x2000_0000;
    pub const SPEED_MODE_SW: u32 = 0x4000_0000;
}

/// The queues' and DMA's configuration bits.
mod queue {
    pub const TXQ_TD_BURST: u32 = 5;
    pub const TXQ_IP_OPTION_ENB: u32 = 0x0000_0010;
    pub const TXQ_ENB: u32 = 0x0000_0020;
    pub const TXQ_ENHANCED_MODE: u32 = 0x0000_0040;
    pub const TXQ_8023_ENB: u32 = 0x0000_0080;
    pub const TXQ_FIFO_BURST_SHIFT: u32 = 16;
    pub const TXQ_FIFO_BURST_MASK: u32 = 0xffff_0000;
    pub const RXQ_ASPM_THROUGHPUT_LIMIT_100M: u32 = 0x0000_0003;
    pub const RXQ_QUEUE0_ENB: u32 = 0x8000_0000;
    /// All four of the AR813x's receive queues.
    pub const RXQ_ENB: u32 = RXQ_QUEUE0_ENB | 0x10 | 0x20 | 0x40;
    pub const RXQ_RD_BURST: u32 = 8 << 20;
    pub const RXQ_816X_IDT_TBL_SIZE: u32 = 0x100 << 8;
    pub const DMA_OUT_ORDER: u32 = 0x0000_0004;
    pub const DMA_RCB_128: u32 = 0x0000_0008;
    pub const DMA_RD_REQ_PRI: u32 = 0x0000_0400;
    pub const DMA_RD_DELAY_CNT: u32 = 15 << 11;
    pub const DMA_WR_DELAY_CNT: u32 = 4 << 16;
    pub const DMA_CMB_ENB: u32 = 0x0010_0000;
    pub const DMA_SMB_ENB: u32 = 0x0020_0000;
    pub const DMA_SMB_DIS: u32 = 0x0100_0000;
    pub const DMA_RD_CHNL_SEL_2: u32 = 0x0400_0000;
    pub const DMA_RD_CHNL_SEL_4: u32 = 0x0c00_0000;
    pub const DMA_RD_BURST_SHIFT: u32 = 4;
    pub const DMA_WR_BURST_SHIFT: u32 = 7;
    pub const HQTD_BURST_ENB: u32 = 0x8000_0000;
    pub const WRR_PRI_RESTRICT_NONE: u32 = 0x6000_0000;
    pub const WRR_PRI_DEFAULT: u32 = 4;
    pub const RX_RD_FREE_THRESH: u32 = 16 | (8 << 6);
    pub const RX_FIFO_PAUSE_816X_RSVD: u32 = 3212;
    pub const TSO_OFFLOAD_ERRLGPKT_DROP_ENB: u32 = 0x0000_0800;
}

/// The interrupt status and mask registers' bits.
mod intr {
    pub const RX_FIFO_OFLOW: u32 = 0x0000_0008;
    pub const RD0_UNDERRUN: u32 = 0x0000_0010;
    pub const TX_FIFO_UNDERRUN: u32 = 0x0000_0100;
    pub const DMA_RD_TO_RST: u32 = 0x0000_0200;
    pub const DMA_WR_TO_RST: u32 = 0x0000_0400;
    pub const TXQ_TO_RST: u32 = 0x0000_4000;
    pub const TX_PKT0: u32 = 0x0000_8000;
    pub const RX_PKT0: u32 = 0x0001_0000;
    /// Writing it holds every interrupt off.
    pub const DIS_INT: u32 = 0x8000_0000;
    /// The chip's DMA or transmit queue failed: it does nothing more
    /// until it is set up again.
    pub const FATAL: u32 = DMA_RD_TO_RST | DMA_WR_TO_RST | TXQ_TO_RST;
    /// What the MSI is raised for, as FreeBSD's ALC_INTRS.
    pub const RAISED: u32 =
        FATAL | RX_PKT0 | TX_PKT0 | RX_FIFO_OFLOW | RD0_UNDERRUN | TX_FIFO_UNDERRUN;
}

/// The power management configuration's bits.
mod pm {
    pub const ASPM_L1_ENB: u32 = 0x0000_0008;
    pub const SERDES_L1_ENB: u32 = 0x0000_0010;
    pub const SERDES_PLL_L1_ENB: u32 = 0x0000_0020;
    pub const SERDES_PD_EX_L1: u32 = 0x0000_0040;
    pub const SERDES_BUDS_RX_L1_ENB: u32 = 0x0000_0080;
    pub const RX_L1_AFTER_L0S: u32 = 0x0000_0800;
    pub const ASPM_L0S_ENB: u32 = 0x0000_1000;
    pub const CLK_SWH_L1: u32 = 0x0000_2000;
    pub const PCIE_RECV: u32 = 0x0000_8000;
    pub const L1_ENTRY_TIMER_SHIFT: u32 = 16;
    pub const L1_ENTRY_TIMER_MASK: u32 = 0x000f_0000;
    pub const L1_ENTRY_TIMER_816X_MASK: u32 = 0x0007_0000;
    pub const TX_L1_AFTER_L0S: u32 = 0x0008_0000;
    pub const PM_REQ_TIMER_SHIFT: u32 = 20;
    pub const PM_REQ_TIMER_MASK: u32 = 0x00f0_0000;
    pub const LCKDET_TIMER_SHIFT: u32 = 24;
    pub const LCKDET_TIMER_MASK: u32 = 0x0f00_0000;
    pub const EN_BUFS_RX_L0S: u32 = 0x1000_0000;
    pub const SA_DLY_ENB: u32 = 0x2000_0000;
    pub const MAC_ASPM_CHK: u32 = 0x4000_0000;
    pub const HOTRST: u32 = 0x8000_0000;
    pub const L1_ENTRY_TIMER_DEFAULT: u32 = 1;
    pub const L1_ENTRY_TIMER_816X_DEFAULT: u32 = 4;
    pub const LCKDET_TIMER_DEFAULT: u32 = 12;
    pub const PM_REQ_TIMER_DEFAULT: u32 = 12;
    pub const PM_REQ_TIMER_816X_DEFAULT: u32 = 15;
}

/// The PCI Express capability's registers and bits, as the chip mirrors
/// them.
mod express {
    pub const DEVICE_CTL: usize = 0x08;
    pub const LINK_CAP: usize = 0x0c;
    pub const LINK_CTL: usize = 0x10;
    pub const MAX_PAYLOAD: u16 = 0x00e0;
    pub const MAX_READ_REQUEST: u16 = 0x7000;
    pub const LINK_CAP_ASPM: u16 = 0x0c00;
    pub const LINK_CTL_ASPM_L0S: u16 = 0x0001;
    pub const LINK_CTL_ASPM_L1: u16 = 0x0002;
    pub const LINK_CTL_RCB: u16 = 0x0008;
    pub const LINK_CTL_EXTENDED_SYNC: u16 = 0x0080;
    /// The last byte of the capability the driver reads.
    pub const END: usize = LINK_CTL + 2;
}

/// The other registers' bits the driver sets.
const PEX_UNC_ERR_SEV_DLP: u32 = 0x0000_0010;
const PEX_UNC_ERR_SEV_FCP: u32 = 0x0000_2000;
const LTSSM_ID_WRO_ENB: u32 = 0x0000_1000;
const PCIE_PHYMISC_FORCE_RCV_DET: u32 = 0x0000_0004;
const PCIE_PHYMISC2_SERDES_MASK: u32 = 0x000f_0000;
const PCIE_PHYMISC2_SERDES_AR8152_B_V10: u32 = (3 << 16) | (3 << 18);
const PDLL_TRNS1_D3PLLOFF_ENB: u32 = 0x0000_0800;
const SERDES_CLK_SLOWDOWN: u32 = 0x0002_0000 | 0x0004_0000;
const MISC_INTNLOSC_OPEN: u32 = 0x0000_0008;
const MISC_ISO_ENB: u32 = 0x0000_1000;
const MISC_PSW_OCP_MASK: u32 = 0x00e0_0000;
const MISC_PSW_OCP_DEFAULT: u32 = 7 << 21;
const MISC2_CALB_START: u32 = 0x0000_0001;
const MISC3_25M_NOTO_INTNL: u32 = 0x0000_0001;
const MISC3_25M_BY_SW: u32 = 0x0000_0002;
const CLK_GATING_ALL: u32 = 0x3f;
const IDLE_DECISN_TIMER_DEFAULT_1MS: u32 = 0x400;
const DMA_BLOCK_LOAD: u32 = 0x0000_0001;
const IPG_IFG_DEFAULT: u32 = 0x60 | (0x50 << 8) | (0x40 << 16) | (0x60 << 24);
const HDPX_DEFAULT: u32 = 0x37 | (0x0f << 12) | 0x0001_0000 | (0x0a << 20) | (0x07 << 24);
const SRAM_RX_FIFO_LEN_MASK: u32 = 0x0fff;
const RX_FIFO_PAUSE_LO_MASK: u32 = 0x0000_0fff;
const RX_FIFO_PAUSE_HI_MASK: u32 = 0x0fff_0000;
const LPI_CTL_ENB: u32 = 0x0000_0001;
const OPT_CFG_CLK_ENB: u32 = 0x0000_0002;
const TWSI_CFG_SW_LD_START: u32 = 0x0000_0800;
const TWSI_DEBUG_DEV_EXIST: u32 = 0x2000_0000;
const SLD_START: u32 = 0x0000_0800;
const SLD_PROGRESS: u32 = 0x0000_1000;
const EEPROM_LD_START: u32 = 0x0000_0001;
const EEPROM_LD_PROGRESS: u32 = 0x0000_0020;
const EEPROM_LD_EXISTS: u32 = 0x0000_0200 | 0x0000_0400;

/// The bytes of a receive buffer the chip is told, the most it takes; a
/// longer frame comes in several.
const RX_BUF_SIZE: u32 = 1536;
/// The AR816x's revisions, from its PCI revision.
const AR816X_REV_A1: u8 = 1;
const AR816X_REV_B0: u8 = 2;
/// The PCI revision of the AR8152 B v1.0.
const AR8152_B_V10: u8 = 0xc0;
/// The DMA bursts the PCI Express maximums allow, by their code.
const DMA_BURSTS: [u32; 8] = [128, 256, 512, 1024, 2048, 4096, 0, 0];
/// The longest burst the chip is given, 1024 bytes.
const DMA_BURST_MAX: u16 = 3;

/// How long things take, in microseconds.
const RESET_US: u32 = 1_000;
const IDLE_US: u32 = 10_000;
const LOAD_US: u32 = 100_000;
/// How often the PHY is looked at for the link, in nanoseconds, and how
/// many looks without a link the driver waits before it has the PHY
/// negotiate again, as FreeBSD's `mii(4)` does for a gigabit PHY.
const LINK_EVERY_NS: u64 = 1_000_000_000;
const RENEGOTIATE_AFTER: u32 = 17;

/// The descriptors in each ring.
const RING: usize = 256;

/// The family-wide quirks of a model.
mod flag {
    /// The AR816x family: the AR8161, AR8162, AR8171 and AR8172 and the
    /// Killers.
    pub const AR816X: u8 = 0x01;
    /// 10 and 100 Mb/s only.
    pub const FAST: u8 = 0x02;
    /// The AR8151 and AR8152's power saving.
    pub const APS: u8 = 0x04;
    /// A Killer.
    pub const E2X00: u8 = 0x08;
}

/// A card the driver knows, from its PCI IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    /// Its maker and name: "Atheros AR8161", "Killer E2400".
    pub name: &'static str,
    /// Its PCI device ID.
    pub device: u16,
    flags: u8,
    /// The longest frame the chip takes, which it is told so that it
    /// drops none shorter.
    max_frame: u32,
}

impl Model {
    /// Whether it is of the AR816x family.
    pub fn ar816x(&self) -> bool {
        self.flags & flag::AR816X != 0
    }

    /// The article its name takes: "an Atheros", "a Killer".
    pub fn article(&self) -> &'static str {
        if self.name.starts_with('A') { "an" } else { "a" }
    }

    /// Whether the kernel should give it an MSI: every one is on PCI
    /// Express, and its MSI works.
    pub fn msi(&self) -> bool {
        true
    }

    fn is(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

const KIB: u32 = 1024;

/// The cards, as FreeBSD's `alc(4)` lists them.
const CARDS: &[Model] = &[
    Model { name: "Atheros AR8131", device: device::AR8131, flags: 0, max_frame: 9 * KIB },
    Model { name: "Atheros AR8132", device: device::AR8132, flags: flag::FAST, max_frame: 9 * KIB },
    Model { name: "Atheros AR8151", device: device::AR8151, flags: flag::APS, max_frame: 6 * KIB },
    Model {
        name: "Atheros AR8151 v2",
        device: device::AR8151_V2,
        flags: flag::APS,
        max_frame: 6 * KIB,
    },
    Model {
        name: "Atheros AR8152",
        device: device::AR8152_B,
        flags: flag::FAST | flag::APS,
        max_frame: 6 * KIB,
    },
    Model {
        name: "Atheros AR8152 v2",
        device: device::AR8152_B2,
        flags: flag::FAST | flag::APS,
        max_frame: 6 * KIB,
    },
    Model {
        name: "Atheros AR8161",
        device: device::AR8161,
        flags: flag::AR816X,
        max_frame: 9 * KIB,
    },
    Model {
        name: "Atheros AR8162",
        device: device::AR8162,
        flags: flag::AR816X | flag::FAST,
        max_frame: 9 * KIB,
    },
    Model {
        name: "Atheros AR8171",
        device: device::AR8171,
        flags: flag::AR816X,
        max_frame: 9 * KIB,
    },
    Model {
        name: "Atheros AR8172",
        device: device::AR8172,
        flags: flag::AR816X | flag::FAST,
        max_frame: 9 * KIB,
    },
    Model {
        name: "Killer E2200",
        device: device::E2200,
        flags: flag::AR816X | flag::E2X00,
        max_frame: 9 * KIB,
    },
    Model {
        name: "Killer E2400",
        device: device::E2400,
        flags: flag::AR816X | flag::E2X00,
        max_frame: 9 * KIB,
    },
    Model {
        name: "Killer E2500",
        device: device::E2500,
        flags: flag::AR816X | flag::E2X00,
        max_frame: 9 * KIB,
    },
];

/// The card with these PCI IDs, if the driver knows it.
pub fn identify(vendor: u16, device: u16) -> Option<Model> {
    if vendor != VENDOR {
        return None;
    }
    CARDS.iter().find(|model| model.device == device).copied()
}

/// What the kernel reads of the card's PCI configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pci {
    /// Its revision.
    pub revision: u8,
    /// Its subsystem vendor and subsystem IDs.
    pub subsystem: (u16, u16),
    /// The offset of its PCI Express capability, whose registers the chip
    /// mirrors in its own; `None` leaves the PCI Express settings as they
    /// are.
    pub express: Option<usize>,
}

/// The chip, as the driver sets it up: its model and what its revision
/// and its bus make of it.
#[derive(Clone, Copy, Debug)]
struct Chip {
    model: Model,
    revision: u8,
    /// The early AR8161 on Atheros' own board, which needs its link
    /// worked around.
    link_war: bool,
    /// The PCI Express capability's offset.
    express: Option<usize>,
    /// The DMA bursts, by their codes, and the read completion boundary.
    read_burst: u16,
    write_burst: u16,
    rcb: u32,
    /// Whether the BIOS let the link enter L0s and L1.
    l0s: bool,
    l1: bool,
}

impl Chip {
    fn ar816x(&self) -> bool {
        self.model.ar816x()
    }

    fn is(&self, flag: u8) -> bool {
        self.model.is(flag)
    }

    fn device(&self) -> u16 {
        self.model.device
    }

    /// The AR816x's revision: A0, A1, B0, C0.
    fn rev_816x(&self) -> u8 {
        self.revision >> 3
    }

    /// An AR816x of revision A with the revision's odd bit, which needs
    /// its clock and its power states handled around a reset.
    fn early_816x(&self) -> bool {
        self.rev_816x() <= AR816X_REV_A1 && self.revision & 1 != 0
    }

    /// The models whose MAC is told the multicast hash and the speed by
    /// the driver.
    fn speed_by_software(&self) -> bool {
        self.ar816x()
            || matches!(self.device(), device::AR8151 | device::AR8151_V2 | device::AR8152_B2)
    }
}

/// Resets the card of `model`, whose registers are `regs` (its memory
/// BAR), and sets it running, with interrupts as `interrupt` says.
/// `fallback_mac` is its address if it has none that can be read;
/// `alloc` gives zeroed DMA memory in pages, below 4 GiB, so that the
/// rings that share the high half of their addresses share it; `delay`
/// waits the given number of microseconds; and `clock` is the time in
/// nanoseconds, by which the running card waits and looks at its link.
#[expect(clippy::too_many_arguments, reason = "what the kernel knows of the card")]
pub fn start(
    model: Model,
    pci: Pci,
    regs: Mmio,
    interrupt: Interrupt,
    fallback_mac: [u8; 6],
    alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    delay: &mut dyn FnMut(u32),
    clock: fn() -> u64,
) -> Result<Card, Error> {
    if regs.len() < reg::END || pci.express.is_some_and(|at| regs.len() < at + express::END) {
        return Err(Error::Registers);
    }
    if regs.read32(reg::MASTER_CFG) == u32::MAX {
        return Err(Error::Device("its registers read as all ones"));
    }
    let mut chip = Chip {
        model,
        revision: pci.revision,
        link_war: matches!(
            model.device,
            device::AR8161 | device::E2200 | device::E2400 | device::E2500
        ) && pci.subsystem == (VENDOR, 0x0091)
            && pci.revision == 0,
        express: pci.express,
        read_burst: 0,
        write_burst: 0,
        rcb: 0,
        l0s: false,
        l1: false,
    };
    if let Some(at) = chip.express {
        let control = regs.read16(at + express::DEVICE_CTL);
        chip.read_burst = ((control & express::MAX_READ_REQUEST) >> 12).min(DMA_BURST_MAX);
        chip.write_burst = ((control & express::MAX_PAYLOAD) >> 5).min(DMA_BURST_MAX);
        if chip.is(flag::E2X00 | flag::AR816X) {
            chip.write_burst = 0;
        }
        init_pcie(&regs, &mut chip, delay);
    }
    phy::reset(&regs, &chip, delay);
    stop_mac(&regs, chip.ar816x(), delay);
    reset(&regs, &chip, delay)?;
    let mac = read_mac(&regs, &chip, delay).unwrap_or(fallback_mac);
    // Set up from here as FreeBSD's alc_init_locked() does: stopped and
    // reset again, the rings, then the registers in their order.
    halt(&regs, &chip, delay);
    reset(&regs, &chip, delay)?;
    let rings = Rings::new(regs.clone(), &chip, RING, alloc)?;
    if chip.ar816x() {
        regs.write32(reg::CLK_GATING_CFG, CLK_GATING_ALL);
        if chip.rev_816x() >= AR816X_REV_B0 {
            regs.write32(reg::IDLE_DECISN_TIMER, IDLE_DECISN_TIMER_DEFAULT_1MS);
        }
    } else {
        regs.write32(reg::CLK_GATING_CFG, 0);
    }
    set_mac(&regs, mac);
    // Wake on LAN off, its status cleared by the read.
    let _ = regs.read32(reg::WOL_CFG);
    regs.write32(reg::WOL_CFG, 0);
    program_rings(&regs, &chip, &rings);
    regs.write32(reg::DMA_BLOCK, DMA_BLOCK_LOAD);
    set_up_registers(&regs, &chip);
    let mask = if interrupt == Interrupt::Msi { intr::RAISED } else { 0 };
    regs.write32(reg::INTR_MASK, mask);
    regs.write32(reg::INTR_STATUS, u32::MAX);
    regs.write32(reg::INTR_STATUS, 0);
    phy::start(&regs, &chip, delay);
    if chip.ar816x() {
        // Running, its link down: L1 allowed, as FreeBSD's first look at
        // the link leaves it.
        aspm_816x(&regs, &chip, false, false, true);
    }
    Ok(Card::new(chip, mac, regs, rings, interrupt, clock))
}

/// Has the chip report PCI Express errors as FreeBSD's does, reads the
/// power states the BIOS allowed and turns them off on the AR813x until
/// the link is up, and sets the AR816x's clocks.
fn init_pcie(regs: &Mmio, chip: &mut Chip, delay: &mut dyn FnMut(u32)) {
    let Some(at) = chip.express else { return };
    let severity = regs.read32(reg::PEX_UNC_ERR_SEV);
    regs.write32(reg::PEX_UNC_ERR_SEV, severity & !(PEX_UNC_ERR_SEV_DLP | PEX_UNC_ERR_SEV_FCP));
    if chip.ar816x() {
        let pll = regs.read32(reg::PDLL_TRNS1);
        regs.write32(reg::PDLL_TRNS1, pll & !PDLL_TRNS1_D3PLLOFF_ENB);
        let master = regs.read32(reg::MASTER_CFG);
        let wanted = if chip.early_816x() {
            master | master::WAKEN_25M | master::CLK_SEL_DIS
        } else {
            (master | master::WAKEN_25M) & !master::CLK_SEL_DIS
        };
        if wanted != master {
            regs.write32(reg::MASTER_CFG, wanted);
        }
        aspm_816x(regs, chip, false, true, false);
    } else {
        let ltssm = regs.read32(reg::LTSSM_ID_CFG);
        regs.write32(reg::LTSSM_ID_CFG, ltssm & !LTSSM_ID_WRO_ENB);
        let misc = regs.read32(reg::PCIE_PHYMISC);
        regs.write32(reg::PCIE_PHYMISC, misc | PCIE_PHYMISC_FORCE_RCV_DET);
        if chip.device() == device::AR8152_B && chip.revision == AR8152_B_V10 {
            let misc2 = regs.read32(reg::PCIE_PHYMISC2) & !PCIE_PHYMISC2_SERDES_MASK;
            regs.write32(reg::PCIE_PHYMISC2, misc2 | PCIE_PHYMISC2_SERDES_AR8152_B_V10);
        }
        if regs.read16(at + express::LINK_CAP) & express::LINK_CAP_ASPM != 0 {
            let control = regs.read16(at + express::LINK_CTL);
            if control & express::LINK_CTL_RCB != 0 {
                chip.rcb = queue::DMA_RCB_128;
            }
            chip.l0s = control & express::LINK_CTL_ASPM_L0S != 0;
            chip.l1 = control & express::LINK_CTL_ASPM_L1 != 0;
            // The vendor's: the power states off.
            let pm = regs.read32(reg::PM_CFG)
                & !(pm::L1_ENTRY_TIMER_MASK
                    | pm::CLK_SWH_L1
                    | pm::ASPM_L0S_ENB
                    | pm::ASPM_L1_ENB
                    | pm::MAC_ASPM_CHK
                    | pm::SERDES_PD_EX_L1);
            regs.write32(
                reg::PM_CFG,
                pm | pm::SERDES_BUDS_RX_L1_ENB | pm::SERDES_PLL_L1_ENB | pm::SERDES_L1_ENB,
            );
        }
    }
    delay(0);
}

/// Sets the AR813x's power states for a link up at `mbps`, as FreeBSD's
/// alc_aspm_813x() does; it leaves them alone while the link is down.
fn aspm_813x(regs: &Mmio, chip: &Chip, mbps: u32) {
    let aps = chip.is(flag::APS);
    let mut pm = regs.read32(reg::PM_CFG);
    let mut link_control = match chip.express {
        Some(at) if aps => regs.read16(at + express::LINK_CTL),
        _ => 0,
    };
    pm &= !(pm::SERDES_PD_EX_L1 | pm::L1_ENTRY_TIMER_MASK | pm::LCKDET_TIMER_MASK);
    pm |= pm::MAC_ASPM_CHK | (pm::LCKDET_TIMER_DEFAULT << pm::LCKDET_TIMER_SHIFT);
    pm &= !(pm::ASPM_L1_ENB | pm::ASPM_L0S_ENB);
    if aps {
        // Extended sync off, but on the AR8152 B v1.0.
        link_control &= !express::LINK_CTL_EXTENDED_SYNC;
        if chip.device() == device::AR8152_B && chip.revision == AR8152_B_V10 {
            link_control |= express::LINK_CTL_EXTENDED_SYNC;
        }
        if let Some(at) = chip.express {
            regs.write16(at + express::LINK_CTL, link_control);
        }
        pm &= !(pm::EN_BUFS_RX_L0S | pm::SA_DLY_ENB | pm::HOTRST);
        pm |= pm::L1_ENTRY_TIMER_DEFAULT << pm::L1_ENTRY_TIMER_SHIFT;
        pm &= !pm::PM_REQ_TIMER_MASK;
        pm |= pm::PM_REQ_TIMER_DEFAULT << pm::PM_REQ_TIMER_SHIFT;
        pm |= pm::SERDES_PD_EX_L1 | pm::PCIE_RECV;
    }
    if chip.l0s {
        pm |= pm::ASPM_L0S_ENB;
    }
    if chip.l1 {
        pm |= pm::ASPM_L1_ENB;
    }
    if aps {
        if chip.device() == device::AR8152_B {
            pm &= !pm::ASPM_L0S_ENB;
        }
        pm &= !(pm::SERDES_L1_ENB | pm::SERDES_PLL_L1_ENB | pm::SERDES_BUDS_RX_L1_ENB);
        pm |= pm::CLK_SWH_L1;
        if mbps >= 100 {
            let timer = match chip.device() {
                device::AR8152_B => 7,
                device::AR8152_B2 | device::AR8151_V2 => 4,
                _ => 15,
            };
            pm = (pm & !pm::L1_ENTRY_TIMER_MASK) | (timer << pm::L1_ENTRY_TIMER_SHIFT);
        }
    } else {
        pm |= pm::SERDES_L1_ENB | pm::SERDES_PLL_L1_ENB | pm::SERDES_BUDS_RX_L1_ENB;
        pm &= !(pm::CLK_SWH_L1 | pm::ASPM_L1_ENB | pm::ASPM_L0S_ENB);
    }
    regs.write32(reg::PM_CFG, pm);
}

/// Sets the AR816x's power states as FreeBSD's alc_aspm_816x() does:
/// both allowed while the link is up or the chip is being set up, L1
/// alone while a running chip's link is down, none once it is stopped.
/// FreeBSD sets the timers without their shifts; they are shifted here,
/// as the vendor's driver sets them.
fn aspm_816x(regs: &Mmio, chip: &Chip, link_up: bool, init: bool, running: bool) {
    let mut pm = regs.read32(reg::PM_CFG);
    pm &= !(pm::L1_ENTRY_TIMER_816X_MASK | pm::PM_REQ_TIMER_MASK | pm::LCKDET_TIMER_MASK);
    pm |= (pm::L1_ENTRY_TIMER_816X_DEFAULT << pm::L1_ENTRY_TIMER_SHIFT)
        | (pm::PM_REQ_TIMER_816X_DEFAULT << pm::PM_REQ_TIMER_SHIFT)
        | (pm::LCKDET_TIMER_DEFAULT << pm::LCKDET_TIMER_SHIFT);
    pm |= pm::SERDES_PD_EX_L1 | pm::CLK_SWH_L1 | pm::PCIE_RECV;
    pm &= !(pm::RX_L1_AFTER_L0S
        | pm::TX_L1_AFTER_L0S
        | pm::ASPM_L1_ENB
        | pm::ASPM_L0S_ENB
        | pm::SERDES_L1_ENB
        | pm::SERDES_PLL_L1_ENB
        | pm::SERDES_BUDS_RX_L1_ENB
        | pm::SA_DLY_ENB
        | pm::MAC_ASPM_CHK
        | pm::HOTRST);
    if chip.early_816x() {
        pm |= pm::SERDES_L1_ENB | pm::SERDES_PLL_L1_ENB;
    }
    if link_up || init {
        pm |= pm::ASPM_L0S_ENB | pm::ASPM_L1_ENB | pm::MAC_ASPM_CHK;
    } else if running {
        pm |= pm::ASPM_L1_ENB | pm::MAC_ASPM_CHK;
    }
    regs.write32(reg::PM_CFG, pm);
}

/// Restarts the AR816x's internal oscillator after a reset, as its
/// revision needs.
fn osc_reset(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) {
    let misc3 = regs.read32(reg::MISC3) & !MISC3_25M_BY_SW;
    regs.write32(reg::MISC3, misc3 | MISC3_25M_NOTO_INTNL);
    let mut misc = regs.read32(reg::MISC);
    if chip.rev_816x() >= AR816X_REV_B0 {
        // The over-current protection's default, which a reset can lose.
        misc = (misc & !MISC_PSW_OCP_MASK) | MISC_PSW_OCP_DEFAULT;
        misc &= !MISC_INTNLOSC_OPEN;
        regs.write32(reg::MISC, misc);
        regs.write32(reg::MISC, misc | MISC_INTNLOSC_OPEN);
        let misc2 = regs.read32(reg::MISC2) & !MISC2_CALB_START;
        regs.write32(reg::MISC2, misc2);
        regs.write32(reg::MISC2, misc2 | MISC2_CALB_START);
    } else {
        misc &= !MISC_INTNLOSC_OPEN;
        if chip.rev_816x() <= AR816X_REV_A1 {
            misc &= !MISC_ISO_ENB;
        }
        regs.write32(reg::MISC, misc | MISC_INTNLOSC_OPEN);
        regs.write32(reg::MISC, misc);
    }
    delay(20);
}

/// Resets the chip, as FreeBSD's alc_reset() does.
fn reset(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) -> Result<(), Error> {
    let mut pm = 0;
    if chip.ar816x() {
        // The reset's workaround: the receive producer set, which the
        // reset clears.
        regs.write32(reg::MBOX_RD0_PROD_IDX, 1);
        if chip.early_816x() {
            pm = regs.read32(reg::PM_CFG);
            if pm & (pm::ASPM_L0S_ENB | pm::ASPM_L1_ENB) != 0 {
                regs.write32(reg::PM_CFG, pm & !(pm::ASPM_L0S_ENB | pm::ASPM_L1_ENB));
            }
        }
    }
    let master = regs.read32(reg::MASTER_CFG);
    regs.write32(reg::MASTER_CFG, master | master::OOB_DIS_OFF | master::RESET);
    if chip.ar816x() {
        wait_for(delay, RESET_US, 10, || regs.read32(reg::MBOX_RD0_PROD_IDX) == 0);
    }
    if !wait_for(delay, RESET_US, 10, || regs.read32(reg::MASTER_CFG) & master::RESET == 0) {
        return Err(Error::Timeout("reset"));
    }
    let busy = idle::RXMAC | idle::TXMAC | idle::RXQ | idle::TXQ;
    wait_for(delay, RESET_US, 10, || regs.read32(reg::IDLE_STATUS) & busy == 0);
    if chip.ar816x() {
        if chip.early_816x() {
            let master = regs.read32(reg::MASTER_CFG);
            regs.write32(reg::MASTER_CFG, master | master::CLK_SEL_DIS);
            if pm & (pm::ASPM_L0S_ENB | pm::ASPM_L1_ENB) != 0 {
                regs.write32(reg::PM_CFG, pm);
            }
        }
        osc_reset(regs, chip, delay);
        let misc3 = regs.read32(reg::MISC3) & !MISC3_25M_BY_SW;
        regs.write32(reg::MISC3, misc3 | MISC3_25M_NOTO_INTNL);
        let mut misc = regs.read32(reg::MISC) & !MISC_INTNLOSC_OPEN;
        if chip.rev_816x() <= AR816X_REV_A1 {
            misc &= !MISC_ISO_ENB;
        }
        regs.write32(reg::MISC, misc);
        delay(20);
    }
    if chip.ar816x() || matches!(chip.device(), device::AR8152_B | device::AR8151_V2) {
        let lock = regs.read32(reg::SERDES_LOCK);
        regs.write32(reg::SERDES_LOCK, lock | SERDES_CLK_SLOWDOWN);
    }
    Ok(())
}

/// Has the chip load its MAC address again from its EEPROM, or its
/// flash on the AR816x, and reads it: if it is one a card can have.
fn read_mac(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) -> Option<[u8; 6]> {
    if chip.ar816x() {
        load_mac_816x(regs, delay);
    } else {
        load_mac_813x(regs, chip, delay);
    }
    let low = regs.read32(reg::PAR0).to_be_bytes();
    let high = regs.read32(reg::PAR1).to_be_bytes();
    let mac = [high[2], high[3], low[0], low[1], low[2], low[3]];
    usable_mac(mac).then_some(mac)
}

/// The AR813x's: TWSI reloads the EEPROM, with its clock or its PHY's
/// hibernation held off as the model needs.
fn load_mac_813x(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) {
    let has_eeprom = regs.read32(reg::MASTER_CFG) & master::OTP_SEL != 0
        && regs.read32(reg::TWSI_DEBUG) & TWSI_DEBUG_DEV_EXIST != 0;
    if !has_eeprom {
        return;
    }
    let mut option = regs.read32(reg::OPT_CFG);
    let clocked = matches!(chip.device(), device::AR8131 | device::AR8132);
    let hibernating = matches!(
        chip.device(),
        device::AR8151 | device::AR8151_V2 | device::AR8152_B | device::AR8152_B2
    );
    if clocked && option & OPT_CFG_CLK_ENB == 0 {
        option |= OPT_CFG_CLK_ENB;
        regs.write32(reg::OPT_CFG, option);
        let _ = regs.read32(reg::OPT_CFG);
        delay(1_000);
    }
    if hibernating {
        phy::hibernation_for_eeprom(regs, chip, true, delay);
    }
    let ltssm = regs.read32(reg::LTSSM_ID_CFG);
    regs.write32(reg::LTSSM_ID_CFG, ltssm & !LTSSM_ID_WRO_ENB);
    regs.write32(reg::WOL_CFG, 0);
    let _ = regs.read32(reg::WOL_CFG);
    let twsi = regs.read32(reg::TWSI_CFG);
    regs.write32(reg::TWSI_CFG, twsi | TWSI_CFG_SW_LD_START);
    wait_for(delay, LOAD_US, 1_000, || regs.read32(reg::TWSI_CFG) & TWSI_CFG_SW_LD_START == 0);
    if clocked && option & OPT_CFG_CLK_ENB != 0 {
        regs.write32(reg::OPT_CFG, option & !OPT_CFG_CLK_ENB);
        let _ = regs.read32(reg::OPT_CFG);
        delay(1_000);
    }
    if hibernating {
        phy::hibernation_for_eeprom(regs, chip, false, delay);
    }
}

/// The AR816x's: the station address loader, else its EEPROM or flash.
fn load_mac_816x(regs: &Mmio, delay: &mut dyn FnMut(u32)) {
    let mut reloaded = false;
    let mut sld = 0;
    let idle = wait_for(delay, LOAD_US, 1_000, || {
        sld = regs.read32(reg::TWSI_CFG);
        sld & (SLD_PROGRESS | SLD_START) == 0
    });
    if idle {
        regs.write32(reg::TWSI_CFG, sld | SLD_START);
        reloaded = wait_for(delay, LOAD_US, 1_000, || regs.read32(reg::TWSI_CFG) & SLD_START == 0);
    }
    if reloaded || regs.read32(reg::EEPROM_LD) & EEPROM_LD_EXISTS == 0 {
        return;
    }
    let mut load = 0;
    let idle = wait_for(delay, LOAD_US, 1_000, || {
        load = regs.read32(reg::EEPROM_LD);
        load & (EEPROM_LD_PROGRESS | EEPROM_LD_START) == 0
    });
    if idle {
        regs.write32(reg::EEPROM_LD, load | EEPROM_LD_START);
        wait_for(delay, LOAD_US, 1_000, || regs.read32(reg::EEPROM_LD) & EEPROM_LD_START == 0);
    }
}

/// Programs `mac` as the card's address.
fn set_mac(regs: &Mmio, mac: [u8; 6]) {
    regs.write32(reg::PAR0, u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]));
    regs.write32(reg::PAR1, u32::from(u16::from_be_bytes([mac[0], mac[1]])));
}

/// Tells the chip where its rings are and how many descriptors they
/// have: one of each, as the second family has.
fn program_rings(regs: &Mmio, chip: &Chip, rings: &Rings) {
    let (transmit, receive, returned) = rings.addresses();
    let ring = RING as u32;
    regs.write32(reg::TX_BASE_ADDR_HI, (transmit >> 32) as u32);
    regs.write32(reg::TDL_HEAD_ADDR_LO, transmit as u32);
    regs.write32(reg::TDH_HEAD_ADDR_LO, 0);
    regs.write32(reg::TD_RING_CNT, ring);
    regs.write32(reg::RX_BASE_ADDR_HI, (receive >> 32) as u32);
    regs.write32(reg::RD0_HEAD_ADDR_LO, receive as u32);
    if !chip.ar816x() {
        for lo in [reg::RD1_HEAD_ADDR_LO, reg::RD2_HEAD_ADDR_LO, reg::RD3_HEAD_ADDR_LO] {
            regs.write32(lo, 0);
        }
    }
    regs.write32(reg::RD_RING_CNT, ring);
    regs.write32(reg::RX_BUF_SIZE, RX_BUF_SIZE);
    regs.write32(reg::RRD0_HEAD_ADDR_LO, returned as u32);
    if !chip.ar816x() {
        for lo in [reg::RRD1_HEAD_ADDR_LO, reg::RRD2_HEAD_ADDR_LO, reg::RRD3_HEAD_ADDR_LO] {
            regs.write32(lo, 0);
        }
    }
    regs.write32(reg::RRD_RING_CNT, ring);
    // The coalescing and statistics blocks, which the chip is not told to
    // write: FreeBSD gives them memory all the same.
    let blocks = rings.blocks();
    regs.write32(reg::CMB_BASE_ADDR_LO, blocks as u32);
    regs.write32(reg::SMB_BASE_ADDR_HI, (blocks >> 32) as u32);
    regs.write32(reg::SMB_BASE_ADDR_LO, (blocks + rings::SMB_OFFSET) as u32);
    if chip.device() == device::AR8152_B {
        // The vendor's SRAM layout for this one.
        regs.write32(reg::SRAM_RX_FIFO_LEN, 0x0000_02a0);
        regs.write32(reg::SRAM_TX_FIFO_LEN, 0x0000_0100);
        regs.write32(reg::SRAM_RX_FIFO_ADDR, 0x029f_0000);
        regs.write32(reg::SRAM_RD0_ADDR, 0x02bf_02a0);
        regs.write32(reg::SRAM_TX_FIFO_ADDR, 0x03bf_02c0);
        regs.write32(reg::SRAM_TD_ADDR, 0x03df_03c0);
        regs.write32(reg::TXF_WATER_MARK, 0);
        regs.write32(reg::RD_DMA_CFG, 0);
    }
}

/// Sets the interrupt moderation, the queues, DMA and the MAC, in
/// FreeBSD's order; the MAC is turned on once the link is up.
fn set_up_registers(regs: &Mmio, chip: &Chip) {
    // The moderation timers count 2 µs: 100 µs for frames received, and
    // on the AR813x 1 ms for frames sent.
    let rx_timer = 100 / 2;
    let tx_timer = 1_000 / 2;
    let mut timers = rx_timer << 16;
    if !chip.ar816x() {
        timers |= tx_timer;
    }
    regs.write32(reg::IM_TIMER, timers);
    let mut master =
        regs.read32(reg::MASTER_CFG) & !(master::IM_RX_TIMER_ENB | master::IM_TX_TIMER_ENB);
    master |= master::SA_TIMER_ENB | master::IM_RX_TIMER_ENB;
    if !chip.ar816x() {
        master |= master::IM_TX_TIMER_ENB;
    }
    regs.write32(reg::MASTER_CFG, master);
    // No interrupt raised again for causes not acknowledged.
    regs.write32(reg::INTR_RETRIG_TIMER, 0);
    // No coalescing block, which every chip's is buggy in: the AR816x's
    // transmit interrupt comes after a third of the ring or 1 ms.
    if chip.ar816x() {
        regs.write32(reg::CMB_TD_THRESH, (RING / 3) as u32);
        regs.write32(reg::CMB_TX_TIMER, tx_timer);
    } else {
        regs.write32(reg::CMB_TX_TIMER, 0);
    }
    regs.write32(reg::SMB_STAT_TIMER, 0);
    // The longest frame the chip takes, so that it drops none; the
    // driver drops those longer than a buffer.
    regs.write32(reg::FRAME_SIZE, chip.model.max_frame);
    if !chip.ar816x() {
        regs.write32(reg::HDS_CFG, 0);
        regs.write32(reg::IPG_IFG_CFG, IPG_IFG_DEFAULT);
        regs.write32(reg::HDPX_CFG, HDPX_DEFAULT);
    }
    let mut threshold = (chip.model.max_frame >> 3) & 0x7ff;
    if chip.ar816x() {
        threshold |= queue::TSO_OFFLOAD_ERRLGPKT_DROP_ENB;
    }
    regs.write32(reg::TSO_OFFLOAD_THRESH, threshold);
    let mut fifo_burst = DMA_BURSTS[usize::from(chip.read_burst)];
    if matches!(chip.device(), device::AR8152_B | device::AR8152_B2) {
        fifo_burst >>= 1;
    }
    let txq = ((fifo_burst << queue::TXQ_FIFO_BURST_SHIFT) & queue::TXQ_FIFO_BURST_MASK)
        | queue::TXQ_TD_BURST
        | queue::TXQ_IP_OPTION_ENB
        | queue::TXQ_8023_ENB
        | queue::TXQ_ENHANCED_MODE;
    regs.write32(reg::TXQ_CFG, txq);
    if chip.ar816x() {
        let burst = queue::TXQ_TD_BURST;
        regs.write32(reg::HQTD_CFG, burst | (burst << 4) | (burst << 8) | queue::HQTD_BURST_ENB);
        let pri = queue::WRR_PRI_DEFAULT;
        regs.write32(
            reg::WRR,
            queue::WRR_PRI_RESTRICT_NONE | pri | (pri << 8) | (pri << 16) | (pri << 24),
        );
    } else {
        regs.write32(reg::RX_RD_FREE_THRESH, queue::RX_RD_FREE_THRESH);
    }
    flow_control_thresholds(regs, chip);
    if !chip.ar816x() {
        regs.write32(reg::RSS_IDT_TABLE0, 0);
        regs.write32(reg::RSS_CPU, 0);
    }
    let mut rxq = queue::RXQ_RD_BURST;
    let fast = chip.is(flag::FAST);
    if chip.ar816x() {
        rxq |= queue::RXQ_816X_IDT_TBL_SIZE;
        if !fast {
            rxq |= queue::RXQ_ASPM_THROUGHPUT_LIMIT_100M;
        }
    } else if !fast && chip.device() != device::AR8151_V2 {
        rxq |= queue::RXQ_ASPM_THROUGHPUT_LIMIT_100M;
    }
    regs.write32(reg::RXQ_CFG, rxq);
    let mut dma = queue::DMA_OUT_ORDER
        | queue::DMA_RD_REQ_PRI
        | chip.rcb
        | queue::DMA_SMB_DIS
        | (u32::from(chip.read_burst) << queue::DMA_RD_BURST_SHIFT)
        | (u32::from(chip.write_burst) << queue::DMA_WR_BURST_SHIFT)
        | queue::DMA_RD_DELAY_CNT
        | queue::DMA_WR_DELAY_CNT;
    if chip.ar816x() {
        dma |= if chip.rev_816x() <= AR816X_REV_A1 {
            queue::DMA_RD_CHNL_SEL_2
        } else {
            queue::DMA_RD_CHNL_SEL_4
        };
    }
    regs.write32(reg::DMA_CFG, dma);
    // CRC added and short frames padded by the MAC; its speed is set for
    // the link once it is up. Broadcasts and every multicast frame, and
    // VLAN tags left in the frames.
    let mut mac = mac::TX_CRC_ENB | mac::TX_AUTO_PAD | mac::FULL_DUPLEX | mac::PREAMBLE;
    if chip.speed_by_software() {
        mac |= mac::HASH_ALG_CRC32 | mac::SPEED_MODE_SW;
    }
    mac |= if fast { mac::SPEED_10_100 } else { mac::SPEED_1000 };
    mac |= mac::BCAST | mac::ALLMULTI;
    mac &= !(mac::PROMISC | mac::VLAN_TAG_STRIP);
    regs.write32(reg::MAR0, u32::MAX);
    regs.write32(reg::MAR1, u32::MAX);
    regs.write32(reg::MAC_CFG, mac);
}

/// The receive FIFO's flow control thresholds, on the chips FreeBSD sets
/// them on: XON at 80% of the FIFO and XOFF at 30% on the AR8131 and
/// AR8132, and the AR816x's from its FIFO's length.
fn flow_control_thresholds(regs: &Mmio, chip: &Chip) {
    let fifo = regs.read32(reg::SRAM_RX_FIFO_LEN);
    if chip.ar816x() {
        let bytes = (fifo & SRAM_RX_FIFO_LEN_MASK) * 8;
        let reserved = if bytes > 8 * KIB { queue::RX_FIFO_PAUSE_816X_RSVD } else { RX_BUF_SIZE };
        let low = bytes.saturating_sub(reserved) / 8;
        let high = queue::RX_FIFO_PAUSE_816X_RSVD / 8;
        regs.write32(
            reg::RX_FIFO_PAUSE_THRESH,
            (low & RX_FIFO_PAUSE_LO_MASK) | ((high << 16) & RX_FIFO_PAUSE_HI_MASK),
        );
    } else if matches!(chip.device(), device::AR8131 | device::AR8132) {
        let high = fifo.saturating_mul(8) / 10;
        let low = fifo.saturating_mul(3) / 10;
        regs.write32(
            reg::RX_FIFO_PAUSE_THRESH,
            (low & RX_FIFO_PAUSE_LO_MASK) | ((high << 16) & RX_FIFO_PAUSE_HI_MASK),
        );
    }
}

/// Stops the receive and transmit queues and waits for them to be idle.
fn stop_queues(regs: &Mmio, delay: &mut dyn FnMut(u32), ar816x: bool) {
    let rxq = regs.read32(reg::RXQ_CFG);
    let on = if ar816x { queue::RXQ_QUEUE0_ENB } else { queue::RXQ_ENB };
    if rxq & on != 0 {
        regs.write32(reg::RXQ_CFG, rxq & !on);
    }
    let txq = regs.read32(reg::TXQ_CFG);
    if txq & queue::TXQ_ENB != 0 {
        regs.write32(reg::TXQ_CFG, txq & !queue::TXQ_ENB);
    }
    delay(40);
    wait_for(delay, IDLE_US, 10, || regs.read32(reg::IDLE_STATUS) & (idle::RXQ | idle::TXQ) == 0);
}

/// Stops the queues and the MAC, and waits for the MAC to be idle.
fn stop_mac(regs: &Mmio, ar816x: bool, delay: &mut dyn FnMut(u32)) {
    stop_queues(regs, delay, ar816x);
    let mac = regs.read32(reg::MAC_CFG);
    if mac & (mac::TX_ENB | mac::RX_ENB) != 0 {
        regs.write32(reg::MAC_CFG, mac & !(mac::TX_ENB | mac::RX_ENB));
    }
    let busy = idle::RXMAC | idle::TXMAC;
    wait_for(delay, IDLE_US, 10, || regs.read32(reg::IDLE_STATUS) & busy == 0);
}

/// Starts the receive and transmit queues: the first receive queue,
/// the only one with a ring.
fn start_queues(regs: &Mmio, chip: &Chip) {
    let mut rxq = regs.read32(reg::RXQ_CFG);
    if !chip.ar816x() {
        rxq &= !queue::RXQ_ENB;
    }
    regs.write32(reg::RXQ_CFG, rxq | queue::RXQ_QUEUE0_ENB);
    let txq = regs.read32(reg::TXQ_CFG);
    regs.write32(reg::TXQ_CFG, txq | queue::TXQ_ENB);
}

/// Stops the chip: its interrupts go off, it writes no more blocks, and
/// its queues and its MAC stop, which leaves it reaching no memory, as
/// FreeBSD's alc_stop() does.
fn halt(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) {
    regs.write32(reg::INTR_MASK, 0);
    regs.write32(reg::INTR_STATUS, u32::MAX);
    let dma = regs.read32(reg::DMA_CFG) & !(queue::DMA_CMB_ENB | queue::DMA_SMB_ENB);
    regs.write32(reg::DMA_CFG, dma | queue::DMA_SMB_DIS);
    delay(1_000);
    stop_mac(regs, chip.ar816x(), delay);
    regs.write32(reg::INTR_STATUS, u32::MAX);
    if chip.ar816x() {
        aspm_816x(regs, chip, false, false, false);
    }
}

/// Waits by the clock, for a running card, which cannot sleep.
fn spin(clock: fn() -> u64) -> impl FnMut(u32) {
    move |us| {
        let until = clock().saturating_add(u64::from(us) * 1_000);
        while clock() < until {
            core::hint::spin_loop();
        }
    }
}

/// A card set up and running.
pub struct Card {
    regs: Mmio,
    chip: Chip,
    rings: Rings,
    info: Info,
    interrupt: Interrupt,
    clock: fn() -> u64,
    /// The link the MAC is set up for.
    link: Link,
    /// When the PHY was last looked at, and how many looks in a row found
    /// no link.
    looked: Option<u64>,
    down_looks: u32,
    /// Whether it was stopped.
    stopped: bool,
}

impl Card {
    fn new(
        chip: Chip,
        mac: [u8; 6],
        regs: Mmio,
        rings: Rings,
        interrupt: Interrupt,
        clock: fn() -> u64,
    ) -> Card {
        let (receive, transmit) = rings.sizes();
        let info = Info {
            model: chip.model.name,
            mac,
            // One buffer of each ring tells a full ring from an empty one.
            receive_slots: (receive - 1) as u16,
            transmit_slots: (transmit - 1) as u16,
        };
        Card {
            regs,
            chip,
            rings,
            info,
            interrupt,
            clock,
            link: Link::default(),
            looked: None,
            down_looks: 0,
            stopped: false,
        }
    }

    /// Looks at the PHY once a second, and sets the MAC up for its link
    /// when it changed, as FreeBSD's alc_miibus_statchg() does; has the
    /// PHY negotiate again after a while without one.
    fn watch_link(&mut self) {
        let now = (self.clock)();
        if self.looked.is_some_and(|then| now.wrapping_sub(then) < LINK_EVERY_NS) {
            return;
        }
        self.looked = Some(now);
        let mut delay = spin(self.clock);
        let link = phy::link(&self.regs, &self.chip, self.link.up, &mut delay);
        if link == self.link {
            if !link.up {
                self.down_looks += 1;
                if self.down_looks >= RENEGOTIATE_AFTER {
                    self.down_looks = 0;
                    phy::negotiate(&self.regs, &self.chip, false, &mut delay);
                }
            }
            return;
        }
        self.link = link;
        self.down_looks = 0;
        stop_mac(&self.regs, self.chip.ar816x(), &mut delay);
        if link.up {
            start_queues(&self.regs, &self.chip);
            let mut mac = self.regs.read32(reg::MAC_CFG)
                & !(mac::FULL_DUPLEX | mac::TX_FC | mac::RX_FC | mac::SPEED_MASK);
            if self.chip.speed_by_software() {
                mac |= mac::HASH_ALG_CRC32 | mac::SPEED_MODE_SW;
            }
            mac |= if link.mbps == 1000 { mac::SPEED_1000 } else { mac::SPEED_10_100 };
            if link.full_duplex {
                mac |= mac::FULL_DUPLEX;
            }
            self.regs.write32(reg::MAC_CFG, mac);
            self.regs.write32(reg::MAC_CFG, mac | mac::TX_ENB | mac::RX_ENB);
        }
        if self.chip.ar816x() {
            aspm_816x(&self.regs, &self.chip, link.up, false, true);
        } else if link.up {
            aspm_813x(&self.regs, &self.chip, link.mbps);
        }
    }

    /// Acknowledges the causes of the card's interrupt, so that the next
    /// one raises it again: writing them back clears them, and the MSI is
    /// raised only when a cause comes with none left. A failure of its
    /// DMA or its queue is left for [`Nic::failed`] to see.
    fn acknowledge(&self) {
        for _ in 0..4 {
            let causes = self.regs.read32(reg::INTR_STATUS) & !(intr::FATAL | intr::DIS_INT);
            if causes == 0 {
                break;
            }
            self.regs.write32(reg::INTR_STATUS, causes);
        }
    }
}

impl Nic for Card {
    fn info(&self) -> Info {
        self.info
    }

    fn link(&self) -> Link {
        self.link
    }

    fn failed(&self) -> bool {
        // A card that is gone reads as all ones.
        let causes = self.regs.read32(reg::INTR_STATUS);
        causes == u32::MAX || causes & intr::FATAL != 0 || self.rings.broken()
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        if self.stopped {
            return None;
        }
        if let Some(len) = self.rings.receive(frame) {
            return Some(len);
        }
        self.watch_link();
        if self.interrupt == Interrupt::Msi {
            // Acknowledge the interrupt, then look again for a frame that
            // came in between.
            self.acknowledge();
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
        if !self.stopped {
            self.stopped = true;
            halt(&self.regs, &self.chip, &mut spin(self.clock));
        }
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
