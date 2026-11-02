// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The chips' registers, from the start of their first memory BAR, and
//! their bits, by FreeBSD's names in `if_rgereg.h`.

pub const MAC0: usize = 0x0000;
pub const MAC4: usize = 0x0004;
pub const MAR0: usize = 0x0008;
pub const MAR4: usize = 0x000c;
pub const TXDESC_ADDR_LO: usize = 0x0020;
pub const TXDESC_ADDR_HI: usize = 0x0024;
pub const INT_CFG0: usize = 0x0034;
pub const CMD: usize = 0x0037;
pub const IMR: usize = 0x0038;
pub const ISR: usize = 0x003c;
pub const TXCFG: usize = 0x0040;
pub const RXCFG: usize = 0x0044;
pub const EECMD: usize = 0x0050;
pub const CFG1: usize = 0x0052;
pub const CFG2: usize = 0x0053;
pub const CFG5: usize = 0x0056;
pub const TDFNR: usize = 0x0057;
pub const TIMERINT0: usize = 0x0058;
pub const TIMERINT1: usize = 0x005c;
pub const CSIDR: usize = 0x0064;
pub const CSIAR: usize = 0x0068;
pub const PHYSTAT: usize = 0x006c;
pub const PMCH: usize = 0x006f;
pub const INT_CFG1: usize = 0x007a;
pub const EPHYAR: usize = 0x0080;
pub const TIMERINT2: usize = 0x008c;
pub const TXSTART: usize = 0x0090;
pub const MACOCP: usize = 0x00b0;
pub const PHYOCP: usize = 0x00b8;
pub const DLLPR: usize = 0x00d0;
pub const TWICMD: usize = 0x00d2;
pub const MCUCMD: usize = 0x00d3;
pub const RXMAXSIZE: usize = 0x00da;
pub const CPLUSCMD: usize = 0x00e0;
pub const IM: usize = 0x00e2;
pub const RXDESC_ADDR_LO: usize = 0x00e4;
pub const RXDESC_ADDR_HI: usize = 0x00e8;
pub const PPSW: usize = 0x00f2;
pub const TIMERINT3: usize = 0x00f4;
/// The 64 interrupt moderation timers.
pub const INTMITI: usize = 0x0a00;
/// Where the standard PHY registers are among the PHY's OCP registers,
/// in units of 16.
pub const PHYBASE: u16 = 0x0a40;
/// The address the bootcode loaded the MAC address to.
pub const ADDR0: usize = 0x19e0;
pub const ADDR1: usize = 0x19e4;
pub const RSS_CTRL: usize = 0x4500;
pub const RXQUEUE_CTRL: usize = 0x4800;
pub const EEE_TXIDLE_TIMER: usize = 0x6048;
/// How far the registers reach.
pub const END: usize = 0x1_0000;

pub mod int_cfg0 {
    pub const EN: u8 = 0x01;
    pub const TIMEOUT_BYPASS: u8 = 0x02;
    pub const MITIGATION_BYPASS: u8 = 0x04;
    pub const CLKREQ_8126: u8 = 0x08;
    pub const RDU_BYPASS_8126: u8 = 0x10;
}

pub mod cmd {
    pub const TXENB: u8 = 0x04;
    pub const RXENB: u8 = 0x08;
    pub const RESET: u8 = 0x10;
    pub const STOPREQ: u8 = 0x80;
}

pub mod isr {
    pub const RX_OK: u32 = 0x0000_0001;
    pub const RX_ERR: u32 = 0x0000_0002;
    pub const TX_OK: u32 = 0x0000_0004;
    pub const TX_ERR: u32 = 0x0000_0008;
    pub const LINKCHG: u32 = 0x0000_0020;
    pub const TX_DESC_UNAVAIL: u32 = 0x0000_0080;
    pub const PCS_TIMEOUT: u32 = 0x0000_4000;
    pub const SYSTEM_ERR: u32 = 0x0000_8000;
    /// What raises the MSI: FreeBSD's `RGE_INTRS`.
    pub const RAISED: u32 =
        RX_OK | RX_ERR | TX_OK | TX_ERR | LINKCHG | TX_DESC_UNAVAIL | PCS_TIMEOUT | SYSTEM_ERR;
}

/// The chip's revision, in the transmit configuration register.
pub const TXCFG_HWREV: u32 = 0x7cf0_0000;
/// What the transmitter is set to.
pub const TXCFG_CONFIG: u32 = 0x0300_0700;

pub mod rxcfg {
    pub const ALLPHYS: u32 = 0x0000_0001;
    pub const INDIV: u32 = 0x0000_0002;
    pub const MULTI: u32 = 0x0000_0004;
    pub const BROAD: u32 = 0x0000_0008;
    pub const RUNT: u32 = 0x0000_0010;
    pub const ERRPKT: u32 = 0x0000_0020;
    /// Every frame filter.
    pub const FILTERS: u32 = ALLPHYS | INDIV | MULTI | BROAD | RUNT | ERRPKT;
    /// What the receiver is set to, by family.
    pub const CONFIG: u32 = 0x4100_0700;
    pub const CONFIG_8125B: u32 = 0x4100_0c00;
    pub const CONFIG_8125D: u32 = 0x4120_0c00;
    pub const CONFIG_8126: u32 = 0x4120_0d00;
}

pub const EECMD_WRITECFG: u8 = 0xc0;
pub const CFG1_SPEED_DOWN: u8 = 0x10;
pub const CFG2_CLKREQ_EN: u8 = 0x80;
pub const CFG5_PME_STS: u8 = 0x01;

pub mod csiar {
    pub const BYTE_EN: u32 = 0x0000_000f;
    pub const BYTE_EN_SHIFT: u32 = 12;
    pub const ADDR_MASK: u32 = 0x0000_0fff;
    pub const BUSY: u32 = 0x8000_0000;
}

pub mod phystat {
    pub const FDX: u16 = 0x0001;
    pub const LINK: u16 = 0x0002;
    pub const MBPS_10: u16 = 0x0004;
    pub const MBPS_100: u16 = 0x0008;
    pub const MBPS_1000: u16 = 0x0010;
    pub const MBPS_2500: u16 = 0x0400;
    pub const MBPS_5000: u16 = 0x1000;
}

pub mod ephyar {
    pub const DATA_MASK: u32 = 0x0000_ffff;
    pub const BUSY: u32 = 0x8000_0000;
    pub const ADDR_MASK: u32 = 0x0000_007f;
    pub const ADDR_SHIFT: u32 = 16;
}

pub const TXSTART_START: u16 = 0x0001;

/// The MAC's and the PHY's OCP access registers: busy, the address
/// (halved) above, the data below.
pub mod ocp {
    pub const DATA_MASK: u32 = 0x0000_ffff;
    pub const BUSY: u32 = 0x8000_0000;
    pub const ADDR_SHIFT: u32 = 16;
}

pub const DLLPR_PFM_EN: u8 = 0x40;
pub const DLLPR_TX_10M_PS_EN: u8 = 0x80;

pub mod mcucmd {
    pub const RXFIFO_EMPTY: u8 = 0x10;
    pub const TXFIFO_EMPTY: u8 = 0x20;
    pub const IS_OOB: u8 = 0x80;
}

pub const CPLUSCMD_RXCSUM: u16 = 0x0020;

/// The PHY's advertisements of the speeds above a gigabit, in its OCP
/// register 0xa5d4.
pub const ADV_2500TFDX: u16 = 0x0080;
pub const ADV_5000TFDX: u16 = 0x0100;

/// The standard PHY registers and bits FreeBSD's `mii.h` names.
pub mod mii {
    pub const BMCR: u16 = 0x00;
    pub const ANAR: u16 = 0x04;
    pub const GTCR: u16 = 0x09;
    pub const BMCR_RESET: u16 = 0x8000;
    pub const BMCR_AUTOEN: u16 = 0x1000;
    pub const BMCR_STARTNEG: u16 = 0x0200;
    pub const ANAR_10: u16 = 0x0020;
    pub const ANAR_10_FD: u16 = 0x0040;
    pub const ANAR_TX: u16 = 0x0080;
    pub const ANAR_TX_FD: u16 = 0x0100;
    pub const ANAR_FC: u16 = 0x0400;
    pub const ANAR_PAUSE_ASYM: u16 = 0x0800;
    pub const GTCR_ADV_1000THDX: u16 = 0x0100;
    pub const GTCR_ADV_1000TFDX: u16 = 0x0200;
}
