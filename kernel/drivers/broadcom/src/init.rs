// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Resetting the chip and setting up its state machines, in the order
//! and with the values of FreeBSD's `bge_reset()`, `bge_chipinit()`,
//! `bge_blockinit()`, `bge_init_locked()` and `bge_stop()`, for the
//! chips this driver knows.

use radix_hw::Mmio;
use radix_nic::{Error, Interrupt, Link, wait_for};

use crate::reg::{self, mac_mode, mode_ctl, pci, rcb, sram};
use crate::rings::{BUFFER, Rings, SEND_RING, STD_RING};
use crate::{Chip, asic, chipid};

/// FreeBSD's `BGE_TIMEOUT`: 100,000 looks, of 10 µs for most.
const LOOKS: u32 = 100_000;
/// The longest frame the MAC takes: 1500 bytes of payload, a header, a
/// VLAN tag and the frame check sequence.
const RX_MTU: u32 = 1500 + 14 + 4 + 4;
/// The coalescing FreeBSD sets: 150 µs or 10 descriptors, and a status
/// block after each descriptor while the interrupt is pending.
const COAL_TICKS: u32 = 150;
const COAL_BDS: u32 = 10;
/// FreeBSD's jumbo ring size, whose replenish threshold is an eighth.
const JUMBO_RING: u32 = 256;

/// Sets the largest read request the function makes, which a reset
/// forgets.
pub fn set_max_read(chip: &Chip) {
    let at = chip.express + reg::express::DEVICE_CTL;
    let encoded = (chip.max_read / 128).trailing_zeros() as u16;
    let control = chip.function.read16(at) & !reg::express::CTL_MAX_READ_REQUEST;
    chip.function.write16(at, control | encoded << reg::express::CTL_MAX_READ_REQUEST_SHIFT);
}

/// The bit of the 5719's or 5720's read DMA fix.
pub fn tx_length_fix(chip: &Chip) -> u32 {
    if chip.asicrev == asic::BCM5719 {
        reg::rdma::TX_LENGTH_WA_5719
    } else {
        reg::rdma::TX_LENGTH_WA_5720
    }
}

fn set(regs: &Mmio, at: usize, bits: u32) {
    regs.write32(at, regs.read32(at) | bits);
}

fn clear(regs: &Mmio, at: usize, bits: u32) {
    regs.write32(at, regs.read32(at) & !bits);
}

/// Resets the chip's core, keeping the MAC's port mode and the APE's
/// access to the MAC, and waits for the bootcode to say it is done; a
/// chip whose bootcode does not say so is used all the same, as
/// FreeBSD's is.
pub fn reset(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) {
    let function = &chip.function;
    let mut kept = mac_mode::HALF_DUPLEX | mac_mode::PORTMODE;
    if chip.on_ape() {
        kept |= mac_mode::APE;
    }
    let mac = regs.read32(reg::MAC_MODE) & kept;
    // The NVRAM's lock, which the reset gives up.
    regs.write32(reg::NVRAM_SWARB, reg::NVRAMSWARB_SET1);
    let _ = wait_for(delay, 8_000 * 20, 20, || {
        regs.read32(reg::NVRAM_SWARB) & reg::NVRAMSWARB_GNT1 != 0
    });
    let ape_locked = chip.ape.as_ref().is_some_and(|ape| ape.lock(reg::ape::LOCK_GRC, delay));
    let cache = function.read32(pci::CACHESZ);
    let command = function.read32(pci::CMD);
    function.write32(pci::MISC_CTL, pci::MISC_CTL_RESET);
    // No fast boot.
    regs.write32(reg::FASTBOOT_PC, 0);
    chip.write_memory(sram::FW_MB, sram::FW_MB_MAGIC);
    let mut reset = reg::misc_cfg::RESET_CORE_CLOCKS | reg::misc_cfg::TIME_66MHZ;
    if !chip.plus_5717() && regs.read32(reg::PCIE_VERSION) == 0x60 {
        regs.write32(reg::PCIE_VERSION, 0x20);
    }
    regs.write32(reg::MISC_CFG, reg::misc_cfg::NO_LINK_TRAINING);
    reset |= reg::misc_cfg::NO_LINK_TRAINING;
    regs.write32(reg::MISC_CFG, reset);
    delay(100_000);

    // The PCI Express state the reset cleared: no relaxed ordering or
    // snooping off, the read request, the errors the reset left.
    let control = chip.express + reg::express::DEVICE_CTL;
    let value = function.read16(control)
        & !(reg::express::CTL_RELAXED_ORD_ENABLE | reg::express::CTL_NOSNOOP_ENABLE);
    function.write16(control, value);
    set_max_read(chip);
    function.write16(chip.express + reg::express::DEVICE_STA, reg::express::STA_ERRORS);
    function.write32(pci::MISC_CTL, pci::MISC_CTL_RESET);
    let mut state = pci::PCISTATE_ROM_ENABLE | pci::PCISTATE_ROM_RETRY_ENABLE;
    if chip.on_ape() {
        state |= pci::PCISTATE_ALLOW_APE;
    }
    function.write32(pci::PCISTATE, state);
    function.write32(pci::CACHESZ, cache);
    function.write32(pci::CMD, command);
    regs.write32(reg::MARB_MODE, reg::ENABLE);
    regs.write32(reg::MODE_CTL, mode_ctl::SWAP);
    let now = regs.read32(reg::MAC_MODE);
    regs.write32(reg::MAC_MODE, (now & !kept) | mac);
    delay(40);
    if let Some(ape) = chip.ape.as_ref().filter(|_| ape_locked) {
        ape.unlock(reg::ape::LOCK_GRC);
    }
    let _ = wait_for(delay, LOOKS * 10, 10, || chip.read_memory(sram::FW_MB) == !sram::FW_MB_MAGIC);
    if chip.chipid == chipid::BCM57765_A0 {
        delay(10_000);
    }
    if !chip.plus_5717() {
        // The data FIFO's protection, on the older PCI Express chips.
        set(regs, reg::PCIE_DATA_FIFO, 1 << 25);
    }
    if chip.asicrev == asic::BCM5720 {
        clear(regs, reg::CPMU_CLCK_ORIDE, reg::CPMU_CLCK_ORIDE_MAC_ORIDE_EN);
    }
}

/// Sets the chip's byte order and DMA up, and clears the statistics and
/// status blocks in its memory.
pub fn chip_init(regs: &Mmio, chip: &Chip) {
    let function = &chip.function;
    let mut misc = pci::MISC_CTL_INIT;
    if chip.tagged {
        misc |= pci::MISC_CTL_TAGGED_STATUS;
    }
    function.write32(pci::MISC_CTL, misc);
    // Both blocks are in the window's first 32 KiB.
    function.write32(pci::MEMWIN_BASEADDR, 0);
    for at in (sram::STATS_BLOCK..=sram::STATUS_BLOCK_END).step_by(4) {
        regs.write32(reg::MEMWIN_START + at, 0);
    }
    if chip.is(&[asic::BCM57765, asic::BCM57766]) && chip.chipid >> 8 != chipid::BCM57765_AX {
        // The PCI Express fast training sequence, which keeps it from
        // hanging as it sends.
        set(regs, reg::CPMU_PADRNG_CTL, reg::CPMU_PADRNG_CTL_RDIV2);
    }
    let watermark = if chip.max_payload >= 256 { 7 } else { 3 };
    let mut dma =
        pci::dma_rw_ctl_rd_cmd(6) | pci::dma_rw_ctl_wr_cmd(7) | pci::dma_rw_ctl_wr_wat(watermark);
    if chip.plus_5717() {
        dma &= !pci::DMA_RW_CTL_DIS_CACHE_ALIGNMENT;
        if chip.chipid == chipid::BCM57765_A0 {
            dma &= !pci::DMA_RW_CTL_CRDRDR_RDMA_MRRS_MSK;
        }
        // Chips that may misread a status tag and leave their interrupt
        // off for good.
        if chip.is(&[asic::BCM5719, asic::BCM5720]) {
            dma |= pci::DMA_RW_CTL_TAGGED_STATUS_WA;
        }
    }
    function.write32(pci::DMA_RW_CTL, dma);
    let mut mode = mode_ctl::SWAP;
    if chip.is(&[asic::BCM5720, asic::BCM5762]) {
        // What the APE's firmware set for its own frames.
        mode |= regs.read32(reg::MODE_CTL) & mode_ctl::B2H_KEPT;
    }
    mode |= mode_ctl::MAC_ATTN_INTR | mode_ctl::HOST_SEND_BDS | mode_ctl::TX_NO_PHDR_CSUM;
    regs.write32(reg::MODE_CTL, mode);
    // Memory write and invalidate, which these chips do not do well.
    let command = function.read32(pci::CMD);
    function.write32(pci::CMD, command & !pci::CMD_MWIEN);
    regs.write32(reg::MISC_CFG, reg::misc_cfg::TIME_66MHZ);
}

/// Writes a ring control block in the chip's memory, through the
/// register space's window, whose base [`block_init`] set to 0.
fn write_rcb(regs: &Mmio, at: usize, word: usize, value: u32) {
    regs.write32(reg::MEMWIN_START + at + word, value);
}

/// Sets up the buffer manager, the rings' control blocks, the host
/// coalescing and every state machine, as `bge_blockinit()` does.
pub fn block_init(
    regs: &Mmio,
    chip: &Chip,
    rings: &Rings,
    mac: [u8; 6],
    delay: &mut dyn FnMut(u32),
) -> Result<(), Error> {
    chip.function.write32(pci::MEMWIN_BASEADDR, 0);
    let (standard, returned, transmit, status) = rings.addresses();

    // The buffer manager's watermarks, for frames of 1500 bytes.
    regs.write32(reg::BMAN_MBUFPOOL_READDMA_LOWAT, 0);
    if chip.plus_5717() {
        regs.write32(reg::BMAN_MBUFPOOL_MACRX_LOWAT, 0x2a);
        regs.write32(reg::BMAN_MBUFPOOL_HIWAT, 0xa0);
    } else {
        regs.write32(reg::BMAN_MBUFPOOL_MACRX_LOWAT, 0x10);
        regs.write32(reg::BMAN_MBUFPOOL_HIWAT, 0x60);
    }
    regs.write32(reg::BMAN_DMA_DESCPOOL_LOWAT, 5);
    regs.write32(reg::BMAN_DMA_DESCPOOL_HIWAT, 10);
    let mut bman = reg::bman::ENABLE | reg::bman::LOMBUF_ATTN;
    if chip.asicrev == asic::BCM5719 {
        // Round robin for the transmit buffers' reads.
        bman |= reg::bman::NO_TX_UNDERRUN;
    }
    regs.write32(reg::BMAN_MODE, bman);
    if !wait_for(delay, LOOKS * 10, 10, || regs.read32(reg::BMAN_MODE) & reg::bman::ENABLE != 0) {
        return Err(Error::Timeout("its buffer manager to start"));
    }
    regs.write32(reg::FTQ_RESET, u32::MAX);
    regs.write32(reg::FTQ_RESET, 0);
    if !wait_for(delay, LOOKS * 10, 10, || regs.read32(reg::FTQ_RESET) == 0) {
        return Err(Error::Timeout("its flow-through queues to reset"));
    }

    // The standard receive ring.
    regs.write32(reg::RX_STD_RCB_HADDR_HI, (standard >> 32) as u32);
    regs.write32(reg::RX_STD_RCB_HADDR_LO, standard as u32);
    let maxlen = if chip.plus_5717() {
        rcb::maxlen_flags(STD_RING as u32, BUFFER << 2)
    } else {
        rcb::maxlen_flags(STD_RING as u32, 0)
    };
    regs.write32(reg::RX_STD_RCB_MAXLEN_FLAGS, maxlen);
    let nic = if chip.bcm571x() { sram::STD_RX_RINGS_5717 } else { sram::STD_RX_RINGS };
    regs.write32(reg::RX_STD_RCB_NICADDR, nic);
    regs.write32(reg::MBX_RX_STD_PROD_LO, 0);
    // The jumbo ring, disabled.
    if chip.jumbo() {
        regs.write32(reg::RX_JUMBO_RCB_HADDR_HI, 0);
        regs.write32(reg::RX_JUMBO_RCB_HADDR_LO, 0);
        regs.write32(
            reg::RX_JUMBO_RCB_MAXLEN_FLAGS,
            rcb::maxlen_flags(0, rcb::FLAG_USE_EXT_RX_BD | rcb::FLAG_RING_DISABLED),
        );
        let nic = if chip.bcm571x() { sram::JUMBO_RX_RINGS_5717 } else { sram::JUMBO_RX_RINGS };
        regs.write32(reg::RX_JUMBO_RCB_NICADDR, nic);
        regs.write32(reg::MBX_RX_JUMBO_PROD_LO, 0);
    }
    regs.write32(reg::RBDI_STD_REPL_THRESH, 8);
    if chip.jumbo() {
        regs.write32(reg::RBDI_JUMBO_REPL_THRESH, JUMBO_RING / 8);
    }
    if chip.plus_5717() {
        regs.write32(reg::STD_REPLENISH_LWM, 32);
        regs.write32(reg::JMB_REPLENISH_LWM, 16);
    }

    // Every send ring disabled, then the first in host memory.
    let sends = if chip.plus_57765() {
        2
    } else if chip.plus_5717() {
        4
    } else {
        1
    };
    for ring in 0..sends {
        let at = sram::SEND_RING_RCB + ring * rcb::SIZE;
        write_rcb(regs, at, rcb::MAXLEN_FLAGS, rcb::maxlen_flags(0, rcb::FLAG_RING_DISABLED));
        write_rcb(regs, at, rcb::NICADDR, 0);
    }
    let at = sram::SEND_RING_RCB;
    write_rcb(regs, at, rcb::HOSTADDR_HI, (transmit >> 32) as u32);
    write_rcb(regs, at, rcb::HOSTADDR_LO, transmit as u32);
    write_rcb(regs, at, rcb::NICADDR, sram::SEND_RING);
    write_rcb(regs, at, rcb::MAXLEN_FLAGS, rcb::maxlen_flags(SEND_RING as u32, 0));

    // Every return ring disabled, then the first.
    let returns = if chip.bcm571x() {
        16
    } else if chip.plus_57765() {
        4
    } else {
        1
    };
    for ring in 0..returns {
        let at = sram::RX_RETURN_RING_RCB + ring * rcb::SIZE;
        write_rcb(regs, at, rcb::HOSTADDR_HI, 0);
        write_rcb(regs, at, rcb::HOSTADDR_LO, 0);
        write_rcb(regs, at, rcb::MAXLEN_FLAGS, rcb::FLAG_RING_DISABLED);
        write_rcb(regs, at, rcb::NICADDR, 0);
        regs.write32(reg::MBX_RX_CONS0_LO + ring * 8, 0);
    }
    let at = sram::RX_RETURN_RING_RCB;
    write_rcb(regs, at, rcb::HOSTADDR_HI, (returned >> 32) as u32);
    write_rcb(regs, at, rcb::HOSTADDR_LO, returned as u32);
    write_rcb(regs, at, rcb::NICADDR, 0);
    write_rcb(regs, at, rcb::MAXLEN_FLAGS, rcb::maxlen_flags(chip.return_count() as u32, 0));

    // The MAC's backoff seed and gaps; the receive rules and lists.
    let seed = mac.iter().map(|&byte| u32::from(byte)).sum::<u32>();
    regs.write32(reg::TX_RANDOM_BACKOFF, seed & reg::TX_BACKOFF_SEED_MASK);
    let mut lengths = 0x2620;
    if chip.is(&[asic::BCM5720, asic::BCM5762]) {
        lengths |= regs.read32(reg::TX_LENGTHS) & reg::TXLEN_KEPT;
    }
    regs.write32(reg::TX_LENGTHS, lengths);
    regs.write32(reg::RX_RULES_CFG, 0x08);
    regs.write32(reg::RXLP_CFG, 0x181);
    regs.write32(reg::RXLP_STATS_ENABLE_MASK, 0x007f_ffff);
    regs.write32(reg::RXLP_STATS_CTL, 0x1);

    // Host coalescing, stopped to be set up, and the status block.
    regs.write32(reg::HCC_MODE, 0);
    if !wait_for(delay, LOOKS * 10, 10, || regs.read32(reg::HCC_MODE) & reg::hcc::ENABLE == 0) {
        return Err(Error::Timeout("its host coalescing to stop"));
    }
    regs.write32(reg::HCC_RX_COAL_TICKS, COAL_TICKS);
    regs.write32(reg::HCC_TX_COAL_TICKS, COAL_TICKS);
    regs.write32(reg::HCC_RX_MAX_COAL_BDS, COAL_BDS);
    regs.write32(reg::HCC_TX_MAX_COAL_BDS, COAL_BDS);
    regs.write32(reg::HCC_RX_MAX_COAL_BDS_INT, 1);
    regs.write32(reg::HCC_TX_MAX_COAL_BDS_INT, 1);
    regs.write32(reg::HCC_STATUSBLK_ADDR_HI, (status >> 32) as u32);
    regs.write32(reg::HCC_STATUSBLK_ADDR_LO, status as u32);
    regs.write32(reg::HCC_MODE, reg::hcc::STATBLK_32BYTE | reg::hcc::ENABLE);
    regs.write32(reg::RBDC_MODE, reg::ENABLE | reg::RBDC_ATTN);
    regs.write32(reg::RXLP_MODE, reg::ENABLE);

    // The MAC's DMA, statistics cleared; the APE's frames kept moving.
    let mut mac_mode = mac_mode::TXDMA_ENB
        | mac_mode::RXDMA_ENB
        | mac_mode::RX_STATS_CLEAR
        | mac_mode::TX_STATS_CLEAR
        | mac_mode::RX_STATS_ENB
        | mac_mode::TX_STATS_ENB
        | mac_mode::FRMHDR_DMA_ENB
        | mac_mode::PORTMODE_MII;
    if chip.on_ape() {
        mac_mode |= mac_mode::APE;
    }
    regs.write32(reg::MAC_MODE, mac_mode);
    delay(40);
    set(regs, reg::MISC_LOCAL_CTL, reg::MLC_INTR_ONATTN);

    // Write DMA, with the host coalescing's tag fix.
    regs.write32(reg::WDMA_MODE, reg::ENABLE | reg::wdma::ALL_ATTNS | reg::wdma::STATUS_TAG_FIX);
    delay(40);

    // Read DMA, and its chips' fixes.
    let mut rdma = reg::ENABLE | reg::rdma::ALL_ATTNS | reg::rdma::FIFO_LONG_BURST;
    if chip.asicrev == asic::BCM5717 {
        rdma |= reg::rdma::MULT_DMA_RD_DIS;
    }
    if chip.tso3() || chip.tso() {
        rdma |= reg::rdma::TSO4_ENABLE;
        if chip.tso3() || chip.asicrev == asic::BCM57780 {
            rdma |= reg::rdma::TSO6_ENABLE;
        }
    }
    if chip.is(&[asic::BCM5720, asic::BCM5762]) {
        rdma |= regs.read32(reg::RDMA_MODE) & reg::rdma::H2BNC_VLAN_DET;
        rdma &= !reg::rdma::MULT_DMA_RD_DIS;
    }
    let reserve =
        if chip.asicrev == asic::BCM5762 { reg::RDMA_RSRVCTRL_REG2 } else { reg::RDMA_RSRVCTRL };
    let mut control = regs.read32(reserve);
    if chip.chipid == chipid::BCM5719_A0 || chip.asicrev == asic::BCM5762 {
        // A margin that keeps the sent data whole and the FIFO from
        // overflowing.
        control &= !reg::rdma::RSRVCTRL_MASKS;
        control |= reg::rdma::RSRVCTRL_FIFO_LWM_1_5K
            | reg::rdma::RSRVCTRL_FIFO_HWM_1_5K
            | reg::rdma::RSRVCTRL_TXMRGN_320B;
    }
    // Fewer receive descriptors fetched at once, so that the read DMA's
    // FIFO does not overflow.
    regs.write32(reserve, control | reg::rdma::RSRVCTRL_FIFO_OFLW_FIX);
    match chip.asicrev {
        asic::BCM5719 => set(
            regs,
            reg::RDMA_LSO_CRPTEN_CTRL,
            reg::rdma::LSO_BLEN_BD_4K | reg::rdma::LSO_BLEN_LSO_4K,
        ),
        asic::BCM5720 => set(
            regs,
            reg::RDMA_LSO_CRPTEN_CTRL,
            reg::rdma::LSO_BLEN_BD_512 | reg::rdma::LSO_BLEN_LSO_4K,
        ),
        asic::BCM5762 => set(
            regs,
            reg::RDMA_LSO_CRPTEN_CTRL_REG2,
            reg::rdma::LSO_BLEN_BD_4K | reg::rdma::LSO_BLEN_LSO_4K,
        ),
        _ => {}
    }
    regs.write32(reg::RDMA_MODE, rdma);
    delay(40);
    if chip.rdma_bug() {
        let miscounted = (0..reg::rdma::CHANNELS / 2).any(|channel| {
            let lengths = regs.read32(reg::RDMA_LENGTH + channel * 4);
            lengths & 0xffff > 1518 || lengths >> 16 > 1518
        });
        if miscounted {
            set(regs, reg::RDMA_LSO_CRPTEN_CTRL, tx_length_fix(chip));
        }
    }

    // The rest of the receive and send state machines.
    regs.write32(reg::RDC_MODE, reg::ENABLE);
    regs.write32(reg::RBDI_MODE, reg::ENABLE);
    regs.write32(reg::RDBDI_MODE, reg::ENABLE);
    regs.write32(reg::SBDC_MODE, reg::ENABLE);
    let mut sdc = reg::ENABLE;
    if chip.asicrev == asic::BCM5761 {
        sdc |= reg::SDC_CDELAY;
    }
    regs.write32(reg::SDC_MODE, sdc);
    let lso = if chip.tso3() || chip.tso() { reg::SDI_HW_LSO_PRE_DMA } else { 0 };
    regs.write32(reg::SDI_MODE, reg::ENABLE | lso);
    regs.write32(reg::SBDI_MODE, reg::ENABLE);
    regs.write32(reg::SRS_MODE, reg::ENABLE);
    regs.write32(reg::SDI_STATS_ENABLE_MASK, 0x007f_ffff);
    regs.write32(reg::SDI_STATS_CTL, reg::SDI_STATS_ENABLE | reg::SDI_STATS_FASTER);

    // The link's changes acknowledged, then an attention when it
    // changes.
    regs.write32(reg::MAC_STS, reg::mac_sts::CHANGES);
    regs.write32(reg::MI_STS, 0);
    regs.write32(reg::MAC_STS, reg::mac_sts::CHANGES);
    set(regs, reg::MAC_EVT_ENB, reg::EVTENB_LINK_CHANGED);
    Ok(())
}

/// What `bge_init_locked()` does after the blocks: the frame length,
/// the address and the filters, the rings handed over, the MAC's
/// transmitter and receiver on, and the interrupt.
pub fn run(
    regs: &Mmio,
    chip: &Chip,
    rings: &Rings,
    mac: [u8; 6],
    interrupt: Interrupt,
    delay: &mut dyn FnMut(u32),
) {
    regs.write32(reg::RX_MTU, RX_MTU);
    regs.write32(reg::MAC_ADDR1_LO, u32::from(mac[0]) << 8 | u32::from(mac[1]));
    regs.write32(reg::MAC_ADDR1_HI, u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]));
    // Not promiscuous; every multicast frame; VLAN tags kept in the
    // frames.
    let mode = regs.read32(reg::RX_MODE) & !reg::rx_mode::RX_PROMISC;
    regs.write32(reg::RX_MODE, mode | reg::rx_mode::RX_KEEP_VLAN_DIAG);
    for word in 0..4 {
        regs.write32(reg::MAR0 + 4 * word, u32::MAX);
    }
    // The standard ring's buffers, and the send ring empty.
    regs.write32(reg::MBX_RX_STD_PROD_LO, rings.standard_producer());
    regs.write32(reg::MBX_TX_HOST_PROD0_LO, 0);
    regs.write32(reg::MBX_TX_NIC_PROD0_LO, 0);

    let mut tx = regs.read32(reg::TX_MODE) | reg::tx_mode::MBUF_LOCKUP_FIX;
    if chip.is(&[asic::BCM5720, asic::BCM5762]) {
        let kept = reg::tx_mode::JMB_FRM_LEN | reg::tx_mode::CNT_DN_MODE;
        tx = (tx & !kept) | (regs.read32(reg::TX_MODE) & kept);
    }
    regs.write32(reg::TX_MODE, tx | reg::tx_mode::ENABLE);
    delay(100);
    let mut rx = regs.read32(reg::RX_MODE) | reg::rx_mode::IPV6_ENABLE;
    if chip.asicrev == asic::BCM5762 {
        rx |= reg::rx_mode::IPV4_FRAG_FIX;
    }
    regs.write32(reg::RX_MODE, rx | reg::rx_mode::ENABLE);
    delay(10);
    // How many frames are taken once the buffers run low.
    regs.write32(reg::MAX_RX_FRAME_LOWAT, if chip.plus_57765() { 1 } else { 2 });
    // The firmware is told the driver runs.
    set(regs, reg::MODE_CTL, mode_ctl::STACKUP);
    let misc = chip.function.read32(pci::MISC_CTL);
    if interrupt == Interrupt::Msi {
        // One-shot MSI: raised once, until the mailbox is written.
        let msi = regs.read32(reg::MSI_MODE) & !reg::msi_mode::ONE_SHOT_DISABLE;
        regs.write32(reg::MSI_MODE, msi | reg::msi_mode::ENABLE);
        let misc = (misc | pci::MISC_CTL_CLEAR_INTA) & !pci::MISC_CTL_MASK_PCI_INTR;
        chip.function.write32(pci::MISC_CTL, misc);
        regs.write32(reg::MBX_IRQ0_LO, 0);
    } else {
        // Polled: the interrupt masked, the status block still written.
        chip.function.write32(pci::MISC_CTL, misc | pci::MISC_CTL_MASK_PCI_INTR);
        regs.write32(reg::MBX_IRQ0_LO, 1);
    }
}

/// Sets the MAC up for `link`: its port mode for the speed, its duplex,
/// and no pause frames.
pub fn set_link(regs: &Mmio, link: Link, delay: &mut dyn FnMut(u32)) {
    let mut mac = regs.read32(reg::MAC_MODE) & !(mac_mode::PORTMODE | mac_mode::HALF_DUPLEX);
    mac |= if link.mbps == 1000 { mac_mode::PORTMODE_GMII } else { mac_mode::PORTMODE_MII };
    if !link.full_duplex {
        mac |= mac_mode::HALF_DUPLEX;
    }
    let tx = regs.read32(reg::TX_MODE) & !reg::tx_mode::FLOWCTL_ENABLE;
    let rx = regs.read32(reg::RX_MODE) & !reg::rx_mode::FLOWCTL_ENABLE;
    regs.write32(reg::MAC_MODE, mac);
    delay(40);
    regs.write32(reg::TX_MODE, tx);
    regs.write32(reg::RX_MODE, rx);
}

/// Stops a state machine: clears its enable bit and waits for it to
/// say it stopped.
fn stop_block(regs: &Mmio, at: usize, delay: &mut dyn FnMut(u32)) {
    clear(regs, at, reg::ENABLE);
    let _ = wait_for(delay, LOOKS * 100, 100, || regs.read32(at) & reg::ENABLE == 0);
}

/// Stops the chip as `bge_stop()` does: its interrupt masked, its
/// receive and send state machines stopped in their order, then reset,
/// so that it reaches no memory; the APE's firmware told.
pub fn stop(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) {
    // A chip that is gone has nothing to stop, and its state machines
    // would never say they stopped.
    if regs.read32(reg::MAC_MODE) == u32::MAX {
        return;
    }
    let misc = chip.function.read32(pci::MISC_CTL);
    chip.function.write32(pci::MISC_CTL, misc | pci::MISC_CTL_MASK_PCI_INTR);
    regs.write32(reg::MBX_IRQ0_LO, 1);
    for block in [
        reg::RX_MODE,
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
    ] {
        stop_block(regs, block, delay);
    }
    regs.write32(reg::FTQ_RESET, u32::MAX);
    regs.write32(reg::FTQ_RESET, 0);
    reset(regs, chip, delay);
    if let Some(ape) = &chip.ape {
        ape.tell(crate::ape::State::Unload, delay);
    }
    clear(regs, reg::MODE_CTL, mode_ctl::STACKUP);
}
