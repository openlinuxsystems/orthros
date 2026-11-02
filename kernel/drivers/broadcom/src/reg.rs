// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The chips' registers and their bits, by the names FreeBSD's
//! `if_bgereg.h` gives them, in the register space of their first BAR,
//! in their PCI configuration space, and in the memory inside the chip
//! that a window of the register space shows.

/// The PCI configuration space's registers that the chip adds; the
/// first 256 bytes of its register space show them too.
pub mod pci {
    pub const CMD: usize = 0x04;
    pub const CACHESZ: usize = 0x0c;
    pub const MISC_CTL: usize = 0x68;
    pub const DMA_RW_CTL: usize = 0x6c;
    pub const PCISTATE: usize = 0x70;
    /// The base of the window on the chip's memory, and the word at it,
    /// for the indirect accesses the configuration space gives.
    pub const MEMWIN_BASEADDR: usize = 0x7c;
    pub const MEMWIN_DATA: usize = 0x84;
    /// Where the newer chips say which chip they are.
    pub const PRODID_ASICREV: usize = 0xbc;
    pub const GEN2_PRODID_ASICREV: usize = 0xf4;
    pub const GEN15_PRODID_ASICREV: usize = 0xfc;

    /// The command register's memory write and invalidate.
    pub const CMD_MWIEN: u32 = 0x0010;

    pub const MISC_CTL_CLEAR_INTA: u32 = 0x0000_0001;
    pub const MISC_CTL_MASK_PCI_INTR: u32 = 0x0000_0002;
    pub const MISC_CTL_ENDIAN_WORDSWAP: u32 = 0x0000_0008;
    pub const MISC_CTL_PCISTATE_RW: u32 = 0x0000_0010;
    pub const MISC_CTL_INDIRECT_ACCESS: u32 = 0x0000_0080;
    pub const MISC_CTL_TAGGED_STATUS: u32 = 0x0000_0200;
    pub const MISC_CTL_ASICREV_SHIFT: u32 = 16;
    /// What FreeBSD's `BGE_INIT` sets: word swapping of the registers,
    /// the interrupt cleared and masked, indirect access.
    pub const MISC_CTL_INIT: u32 = MISC_CTL_ENDIAN_WORDSWAP
        | MISC_CTL_CLEAR_INTA
        | MISC_CTL_MASK_PCI_INTR
        | MISC_CTL_INDIRECT_ACCESS;
    /// What the chip's reset leaves it: as above, the state register
    /// writable, the interrupt not cleared.
    pub const MISC_CTL_RESET: u32 = MISC_CTL_INDIRECT_ACCESS
        | MISC_CTL_MASK_PCI_INTR
        | MISC_CTL_ENDIAN_WORDSWAP
        | MISC_CTL_PCISTATE_RW;

    pub const DMA_RW_CTL_DIS_CACHE_ALIGNMENT: u32 = 0x0000_0001;
    pub const DMA_RW_CTL_TAGGED_STATUS_WA: u32 = 0x0000_0080;
    pub const DMA_RW_CTL_CRDRDR_RDMA_MRRS_MSK: u32 = 0x0000_0380;
    pub const fn dma_rw_ctl_wr_wat(x: u32) -> u32 {
        x << 19
    }
    pub const fn dma_rw_ctl_rd_cmd(x: u32) -> u32 {
        x << 24
    }
    pub const fn dma_rw_ctl_wr_cmd(x: u32) -> u32 {
        x << 28
    }

    pub const PCISTATE_ROM_ENABLE: u32 = 0x0000_0020;
    pub const PCISTATE_ROM_RETRY_ENABLE: u32 = 0x0000_0040;
    /// Writes to the APE's registers and memory, allowed to the host.
    pub const PCISTATE_ALLOW_APE: u32 = 0x0001_0000 | 0x0002_0000 | 0x0004_0000;
}

/// The PCI Express capability's registers, from its start.
pub mod express {
    pub const DEVICE_CAP: usize = 0x04;
    pub const DEVICE_CTL: usize = 0x08;
    pub const DEVICE_STA: usize = 0x0a;
    /// The largest payload the function supports: 128 bytes shifted by
    /// it.
    pub const CAP_MAX_PAYLOAD: u16 = 0x0007;
    pub const CTL_RELAXED_ORD_ENABLE: u16 = 0x0010;
    pub const CTL_NOSNOOP_ENABLE: u16 = 0x0800;
    pub const CTL_MAX_READ_REQUEST: u16 = 0x7000;
    pub const CTL_MAX_READ_REQUEST_SHIFT: u16 = 12;
    /// The device status register's errors, cleared by writing them.
    pub const STA_ERRORS: u16 = 0x000f;
}

// Mailboxes.
pub const MBX_IRQ0_LO: usize = 0x0204;
pub const MBX_RX_STD_PROD_LO: usize = 0x026c;
pub const MBX_RX_JUMBO_PROD_LO: usize = 0x0274;
pub const MBX_RX_CONS0_LO: usize = 0x0284;
pub const MBX_TX_HOST_PROD0_LO: usize = 0x0304;
pub const MBX_TX_NIC_PROD0_LO: usize = 0x0384;

// The MAC.
pub const MAC_MODE: usize = 0x0400;
pub const MAC_STS: usize = 0x0404;
pub const MAC_EVT_ENB: usize = 0x0408;
pub const MAC_ADDR1_LO: usize = 0x0410;
pub const MAC_ADDR1_HI: usize = 0x0414;
pub const TX_RANDOM_BACKOFF: usize = 0x0438;
pub const RX_MTU: usize = 0x043c;
pub const MI_COMM: usize = 0x044c;
pub const MI_STS: usize = 0x0450;
pub const TX_MODE: usize = 0x045c;
pub const TX_LENGTHS: usize = 0x0464;
pub const RX_MODE: usize = 0x0468;
pub const MAR0: usize = 0x0470;
pub const RX_RULES_CFG: usize = 0x0500;
pub const MAX_RX_FRAME_LOWAT: usize = 0x0504;
pub const SGDIG_STS: usize = 0x05b4;

// The state machines, each with its mode register.
pub const SDI_MODE: usize = 0x0c00;
pub const SDI_STATS_CTL: usize = 0x0c08;
pub const SDI_STATS_ENABLE_MASK: usize = 0x0c0c;
pub const SDC_MODE: usize = 0x1000;
pub const SRS_MODE: usize = 0x1400;
pub const SBDI_MODE: usize = 0x1800;
pub const SBDC_MODE: usize = 0x1c00;
pub const RXLP_MODE: usize = 0x2000;
pub const RXLP_CFG: usize = 0x2010;
pub const RXLP_STATS_CTL: usize = 0x2014;
pub const RXLP_STATS_ENABLE_MASK: usize = 0x2018;
pub const RDBDI_MODE: usize = 0x2400;
pub const RX_JUMBO_RCB_HADDR_HI: usize = 0x2440;
pub const RX_JUMBO_RCB_HADDR_LO: usize = 0x2444;
pub const RX_JUMBO_RCB_MAXLEN_FLAGS: usize = 0x2448;
pub const RX_JUMBO_RCB_NICADDR: usize = 0x244c;
pub const RX_STD_RCB_HADDR_HI: usize = 0x2450;
pub const RX_STD_RCB_HADDR_LO: usize = 0x2454;
pub const RX_STD_RCB_MAXLEN_FLAGS: usize = 0x2458;
pub const RX_STD_RCB_NICADDR: usize = 0x245c;
pub const RDC_MODE: usize = 0x2800;
pub const RBDI_MODE: usize = 0x2c00;
pub const RBDI_STD_REPL_THRESH: usize = 0x2c18;
pub const RBDI_JUMBO_REPL_THRESH: usize = 0x2c1c;
pub const STD_REPLENISH_LWM: usize = 0x2d00;
pub const JMB_REPLENISH_LWM: usize = 0x2d04;
pub const RBDC_MODE: usize = 0x3000;
pub const CPMU_CLCK_ORIDE: usize = 0x3624;
pub const CPMU_PHY_STRAP: usize = 0x3664;
pub const CPMU_PADRNG_CTL: usize = 0x3668;
pub const HCC_MODE: usize = 0x3c00;
pub const HCC_RX_COAL_TICKS: usize = 0x3c08;
pub const HCC_TX_COAL_TICKS: usize = 0x3c0c;
pub const HCC_RX_MAX_COAL_BDS: usize = 0x3c10;
pub const HCC_TX_MAX_COAL_BDS: usize = 0x3c14;
pub const HCC_RX_MAX_COAL_BDS_INT: usize = 0x3c20;
pub const HCC_TX_MAX_COAL_BDS_INT: usize = 0x3c24;
pub const HCC_STATUSBLK_ADDR_HI: usize = 0x3c38;
pub const HCC_STATUSBLK_ADDR_LO: usize = 0x3c3c;
pub const MARB_MODE: usize = 0x4000;
pub const BMAN_MODE: usize = 0x4400;
pub const BMAN_MBUFPOOL_READDMA_LOWAT: usize = 0x4410;
pub const BMAN_MBUFPOOL_MACRX_LOWAT: usize = 0x4414;
pub const BMAN_MBUFPOOL_HIWAT: usize = 0x4418;
pub const BMAN_DMA_DESCPOOL_LOWAT: usize = 0x4434;
pub const BMAN_DMA_DESCPOOL_HIWAT: usize = 0x4438;
pub const RDMA_MODE: usize = 0x4800;
pub const RDMA_RSRVCTRL_REG2: usize = 0x4890;
pub const RDMA_LSO_CRPTEN_CTRL_REG2: usize = 0x48a0;
pub const RDMA_RSRVCTRL: usize = 0x4900;
pub const RDMA_LSO_CRPTEN_CTRL: usize = 0x4910;
pub const RDMA_LENGTH: usize = 0x4be0;
pub const WDMA_MODE: usize = 0x4c00;
pub const FTQ_RESET: usize = 0x5c00;
pub const MSI_MODE: usize = 0x6000;
pub const MODE_CTL: usize = 0x6800;
pub const MISC_CFG: usize = 0x6804;
pub const MISC_LOCAL_CTL: usize = 0x6808;
pub const FASTBOOT_PC: usize = 0x6894;
pub const NVRAM_SWARB: usize = 0x7020;
/// A register FreeBSD names by its address alone: the PCI Express
/// version the older chips report, which they are told is 1.0's.
pub const PCIE_VERSION: usize = 0x7e2c;
/// Another, whose bit 25 protects the data FIFO.
pub const PCIE_DATA_FIFO: usize = 0x7c00;
/// The window on the chip's memory, 32 KiB of it at the base the
/// configuration space's `MEMWIN_BASEADDR` sets.
pub const MEMWIN_START: usize = 0x8000;
/// How far the register space reaches.
pub const END: usize = 0x1_0000;

pub mod mac_mode {
    pub const HALF_DUPLEX: u32 = 0x0000_0002;
    pub const PORTMODE: u32 = 0x0000_000c;
    pub const PORTMODE_MII: u32 = 0x0000_0004;
    pub const PORTMODE_GMII: u32 = 0x0000_0008;
    pub const RX_STATS_ENB: u32 = 0x0000_0800;
    pub const RX_STATS_CLEAR: u32 = 0x0000_1000;
    pub const TX_STATS_ENB: u32 = 0x0000_4000;
    pub const TX_STATS_CLEAR: u32 = 0x0000_8000;
    pub const TXDMA_ENB: u32 = 0x0020_0000;
    pub const RXDMA_ENB: u32 = 0x0040_0000;
    pub const FRMHDR_DMA_ENB: u32 = 0x0080_0000;
    /// The APE's own frames, to and from the MAC.
    pub const APE: u32 = 0x0800_0000 | 0x1000_0000;
}

pub mod mac_sts {
    pub const CFG_CHANGED: u32 = 0x0000_0008;
    pub const SYNC_CHANGED: u32 = 0x0000_0010;
    pub const LINK_CHANGED: u32 = 0x0000_1000;
    pub const MI_COMPLETE: u32 = 0x0040_0000;
    /// The changes that are acknowledged by writing them back.
    pub const CHANGES: u32 = SYNC_CHANGED | CFG_CHANGED | MI_COMPLETE | LINK_CHANGED;
}

pub const EVTENB_LINK_CHANGED: u32 = 0x0000_1000;
pub const TX_BACKOFF_SEED_MASK: u32 = 0x3ff;
pub const TXLEN_KEPT: u32 = 0x00ff_0000 | 0xff00_0000;

pub mod tx_mode {
    pub const ENABLE: u32 = 0x0000_0002;
    pub const FLOWCTL_ENABLE: u32 = 0x0000_0010;
    pub const MBUF_LOCKUP_FIX: u32 = 0x0000_0100;
    pub const JMB_FRM_LEN: u32 = 0x0040_0000;
    pub const CNT_DN_MODE: u32 = 0x0080_0000;
}

pub mod rx_mode {
    pub const ENABLE: u32 = 0x0000_0002;
    pub const FLOWCTL_ENABLE: u32 = 0x0000_0004;
    pub const RX_PROMISC: u32 = 0x0000_0100;
    pub const RX_KEEP_VLAN_DIAG: u32 = 0x0000_0400;
    pub const IPV6_ENABLE: u32 = 0x0100_0000;
    pub const IPV4_FRAG_FIX: u32 = 0x0200_0000;
}

pub mod mi {
    pub const COMM_READFAIL: u32 = 0x1000_0000;
    pub const COMM_BUSY: u32 = 0x2000_0000;
    pub const CMD_WRITE: u32 = 0x0400_0000;
    pub const CMD_READ: u32 = 0x0800_0000;
    pub const fn reg(x: u16) -> u32 {
        ((x as u32) & 0x1f) << 16
    }
    pub const fn phy(x: u16) -> u32 {
        ((x as u32) & 0x1f) << 21
    }
}

pub const ENABLE: u32 = 0x0000_0002;

pub mod hcc {
    pub const ENABLE: u32 = 0x0000_0002;
    pub const COAL_NOW: u32 = 0x0000_0008;
    pub const STATBLK_32BYTE: u32 = 0x0000_0100;
}

pub const RBDC_ATTN: u32 = 0x0000_0004;
pub const SDC_CDELAY: u32 = 0x0000_0010;
pub const SDI_HW_LSO_PRE_DMA: u32 = 0x0000_0008;
pub const SDI_STATS_ENABLE: u32 = 0x0000_0001;
pub const SDI_STATS_FASTER: u32 = 0x0000_0002;

pub mod wdma {
    pub const ALL_ATTNS: u32 = 0x0000_03fc;
    pub const STATUS_TAG_FIX: u32 = 0x2000_0000;
}

pub mod rdma {
    pub const ALL_ATTNS: u32 = 0x0000_03fc;
    pub const FIFO_LONG_BURST: u32 = 0x0003_0000;
    pub const MULT_DMA_RD_DIS: u32 = 0x0100_0000;
    pub const TSO4_ENABLE: u32 = 0x0800_0000;
    pub const TSO6_ENABLE: u32 = 0x1000_0000;
    pub const H2BNC_VLAN_DET: u32 = 0x2000_0000;

    pub const RSRVCTRL_FIFO_OFLW_FIX: u32 = 0x0000_0004;
    pub const RSRVCTRL_FIFO_LWM_1_5K: u32 = 0x0000_0c00;
    pub const RSRVCTRL_FIFO_HWM_1_5K: u32 = 0x000c_0000;
    pub const RSRVCTRL_TXMRGN_320B: u32 = 0x2800_0000;
    pub const RSRVCTRL_MASKS: u32 = 0x0000_0ff0 | 0x000f_f000 | 0xffe0_0000;

    pub const LSO_BLEN_BD_512: u32 = 0x0002_0000;
    pub const LSO_BLEN_BD_4K: u32 = 0x0003_0000;
    pub const LSO_BLEN_LSO_4K: u32 = 0x000c_0000;
    pub const TX_LENGTH_WA_5719: u32 = 0x0200_0000;
    pub const TX_LENGTH_WA_5720: u32 = 0x0020_0000;
    /// The read DMA channels whose lengths the 5719 and 5720 miscount.
    pub const CHANNELS: usize = 4;
}

pub mod bman {
    pub const ENABLE: u32 = 0x0000_0002;
    pub const LOMBUF_ATTN: u32 = 0x0000_0010;
    pub const NO_TX_UNDERRUN: u32 = 0x8000_0000;
}

pub mod msi_mode {
    pub const ENABLE: u32 = 0x0000_0002;
    pub const ONE_SHOT_DISABLE: u32 = 0x0000_0020;
}

pub mod mode_ctl {
    pub const WORDSWAP_NONFRAME: u32 = 0x0000_0004;
    pub const BYTESWAP_DATA: u32 = 0x0000_0010;
    pub const WORDSWAP_DATA: u32 = 0x0000_0020;
    /// The swapping of frames between the host and the APE's BMC, which
    /// the 5720's and 5762's firmware sets.
    pub const B2H_KEPT: u32 = 0x0000_0040 | 0x0000_0080 | 0x0000_8000 | 0x0004_0000;
    pub const STACKUP: u32 = 0x0001_0000;
    pub const HOST_SEND_BDS: u32 = 0x0002_0000;
    pub const TX_NO_PHDR_CSUM: u32 = 0x0010_0000;
    pub const MAC_ATTN_INTR: u32 = 0x0400_0000;
    /// FreeBSD's DMA swap options on a little-endian host.
    pub const SWAP: u32 = WORDSWAP_NONFRAME | BYTESWAP_DATA | WORDSWAP_DATA;
}

pub mod misc_cfg {
    pub const RESET_CORE_CLOCKS: u32 = 0x0000_0001;
    /// The 1 µs timer's prescaler for a 66 MHz clock.
    pub const TIME_66MHZ: u32 = 0x41 << 1;
    /// Keeps the PCI Express link from training during the reset.
    pub const NO_LINK_TRAINING: u32 = 1 << 29;
}

pub const MLC_INTR_ONATTN: u32 = 0x0000_0008;
pub const NVRAMSWARB_SET1: u32 = 0x0000_0002;
pub const NVRAMSWARB_GNT1: u32 = 0x0000_0200;
pub const SGDIGSTS_IS_SERDES: u32 = 0x0000_0100;
pub const CPMU_PHY_STRAP_IS_SERDES: u32 = 0x0000_0020;
pub const CPMU_PADRNG_CTL_RDIV2: u32 = 0x0004_0000;
pub const CPMU_CLCK_ORIDE_MAC_ORIDE_EN: u32 = 0x8000_0000;

/// A ring control block's words, by their offset in it.
pub mod rcb {
    pub const HOSTADDR_HI: usize = 0;
    pub const HOSTADDR_LO: usize = 4;
    pub const MAXLEN_FLAGS: usize = 8;
    pub const NICADDR: usize = 12;
    pub const SIZE: usize = 16;
    pub const FLAG_USE_EXT_RX_BD: u32 = 0x0001;
    pub const FLAG_RING_DISABLED: u32 = 0x0002;
    pub const fn maxlen_flags(maxlen: u32, flags: u32) -> u32 {
        (maxlen << 16) | flags
    }
}

/// The chip's own memory, by its addresses there.
pub mod sram {
    /// The send rings' control blocks, and the return rings'.
    pub const SEND_RING_RCB: usize = 0x0100;
    pub const RX_RETURN_RING_RCB: usize = 0x0200;
    pub const STATS_BLOCK: usize = 0x0300;
    pub const STATUS_BLOCK_END: usize = 0x0b4f;
    /// The firmware's mailbox: the driver writes the magic before a
    /// reset, and the bootcode its complement when it is done.
    pub const FW_MB: usize = 0x0b50;
    pub const FW_MB_MAGIC: u32 = 0x4b65_7654;
    /// The bootcode's signature, and the hardware configuration it found
    /// in the NVRAM.
    pub const DATA_SIG: usize = 0x0b54;
    pub const DATA_SIG_MAGIC: u32 = 0x4b65_7654;
    pub const DATA_CFG: usize = 0x0b58;
    pub const HWCFG_MEDIA: u32 = 0x0000_0030;
    pub const MEDIA_FIBER: u32 = 0x0000_0020;
    /// The MAC address the bootcode read, under a signature.
    pub const MAC_ADDR_HIGH_MB: usize = 0x0c14;
    pub const MAC_ADDR_LOW_MB: usize = 0x0c18;
    pub const MAC_ADDR_SIG: u32 = 0x484b;
    /// Where the rings the chip fetches descriptors into are.
    pub const STD_RX_RINGS: u32 = 0x0000_6000;
    pub const STD_RX_RINGS_5717: u32 = 0x0004_0000;
    pub const JUMBO_RX_RINGS: u32 = 0x0000_7000;
    pub const JUMBO_RX_RINGS_5717: u32 = 0x0004_4400;
    pub const SEND_RING: u32 = 0x0000_4000;
}

/// The APE's registers and shared memory, in the third BAR.
pub mod ape {
    pub const EVENT: usize = 0x000c;
    pub const LOCK_REQ: usize = 0x002c;
    pub const LOCK_GRANT: usize = 0x004c;
    pub const SEG_SIG: usize = 0x4000;
    pub const FW_STATUS: usize = 0x400c;
    pub const HOST_SEG_SIG: usize = 0x4200;
    pub const HOST_SEG_LEN: usize = 0x4204;
    pub const HOST_INIT_COUNT: usize = 0x4208;
    pub const HOST_BEHAVIOR: usize = 0x4210;
    pub const HOST_HEARTBEAT_INT_MS: usize = 0x4214;
    pub const HOST_DRVR_STATE: usize = 0x421c;
    pub const EVENT_STATUS: usize = 0x4300;
    pub const PER_LOCK_REQ: usize = 0x8400;
    pub const PER_LOCK_GRANT: usize = 0x8420;
    /// How far the registers this driver uses reach.
    pub const END: usize = 0x8440;

    pub const EVENT_1: u32 = 0x0000_0001;
    pub const LOCK_DRIVER0: u32 = 0x0000_1000;
    pub const SEG_SIG_MAGIC: u32 = 0x4150_4521;
    pub const FW_STATUS_READY: u32 = 0x0000_0100;
    pub const HOST_SEG_SIG_MAGIC: u32 = 0x484f_5354;
    pub const HOST_SEG_LEN_MAGIC: u32 = 0x0000_0020;
    pub const HOST_BEHAV_NO_PHYLOCK: u32 = 0x0000_0001;
    pub const HOST_DRVR_STATE_START: u32 = 0x0000_0001;
    pub const HOST_DRVR_STATE_UNLOAD: u32 = 0x0000_0002;
    pub const EVENT_STATUS_DRIVER_EVNT: u32 = 0x0000_0010;
    pub const EVENT_STATUS_STATE_CHNGE: u32 = 0x0000_0500;
    pub const EVENT_STATUS_STATE_START: u32 = 0x0001_0000;
    pub const EVENT_STATUS_STATE_UNLOAD: u32 = 0x0002_0000;
    pub const EVENT_STATUS_EVENT_PENDING: u32 = 0x8000_0000;

    /// The locks, by their number.
    pub const LOCK_PHY0: usize = 0;
    pub const LOCK_GRC: usize = 1;
    pub const LOCK_PHY1: usize = 2;
    pub const LOCK_PHY2: usize = 3;
    pub const LOCK_MEM: usize = 4;
    pub const LOCK_PHY3: usize = 5;
    pub const LOCK_GPIO: usize = 7;
}
