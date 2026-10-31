// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Synopsys DesignWare I2C controller (DW_apb_i2c) as a bus master,
//! as Intel's LPSS and AMD's FCH have it (Intel's PCH datasheets and AMD's
//! Processor Programming References give its registers; Linux's
//! `i2c-designware` is the reference for what it is used for).
//!
//! Its registers, from its base:
//!
//! - IC_CON (0x00): master mode, the speed (1 standard, 2 fast), 10-bit
//!   addresses, and repeated starts allowed; IC_TAR (0x04), the device's
//!   address. Both are written only while the controller is disabled
//!   (IC_ENABLE, 0x6c; IC_ENABLE_STATUS, 0x9c, says when it is).
//! - IC_DATA_CMD (0x10): a byte to write, or a read (bit 8), queued in the
//!   transmit FIFO, with a stop after it (bit 9) or a repeated start
//!   before it (bit 10); reading it takes a byte from the receive FIFO.
//!   IC_TXFLR and IC_RXFLR (0x74, 0x78) say how full each FIFO is, and
//!   IC_COMP_PARAM_1 (0xf4) how deep.
//! - The SCL clock's high and low counts for each speed (0x14 to 0x20),
//!   in cycles of the controller's clock, and the SDA hold time (0x7c).
//! - IC_RAW_INTR_STAT (0x34): the transfer aborted (TX_ABRT, why in
//!   IC_TX_ABRT_SOURCE, 0x80, cleared by reading 0x54) or a stop was seen
//!   (STOP_DET, cleared by reading 0x60). IC_STATUS (0x70) says whether
//!   the master is active.
//! - IC_COMP_TYPE (0xfc) reads 0x44570140 and IC_COMP_VERSION (0xf8) the
//!   version.
//!
//! A controller built without IC_EMPTYFIFO_HOLD_MASTER_EN ends the
//! transfer with a stop when its transmit FIFO runs dry, so the driver
//! spins while it has bytes left to queue, and sleeps only once all are.
//! It never has more reads queued than the receive FIFO holds.

use radix_hw::Mmio;

use crate::{Error, Message, Platform, Speed};

const IC_CON: usize = 0x00;
const IC_TAR: usize = 0x04;
const IC_DATA_CMD: usize = 0x10;
const IC_SS_SCL_HCNT: usize = 0x14;
const IC_SS_SCL_LCNT: usize = 0x18;
const IC_FS_SCL_HCNT: usize = 0x1c;
const IC_FS_SCL_LCNT: usize = 0x20;
const IC_INTR_MASK: usize = 0x30;
const IC_RAW_INTR_STAT: usize = 0x34;
const IC_CLR_INTR: usize = 0x40;
const IC_CLR_TX_ABRT: usize = 0x54;
const IC_CLR_STOP_DET: usize = 0x60;
const IC_ENABLE: usize = 0x6c;
const IC_STATUS: usize = 0x70;
const IC_TXFLR: usize = 0x74;
const IC_RXFLR: usize = 0x78;
const IC_SDA_HOLD: usize = 0x7c;
const IC_TX_ABRT_SOURCE: usize = 0x80;
const IC_ENABLE_STATUS: usize = 0x9c;
const IC_COMP_PARAM_1: usize = 0xf4;
const IC_COMP_VERSION: usize = 0xf8;
const IC_COMP_TYPE: usize = 0xfc;
/// How many bytes of registers the controller has.
pub const SIZE: usize = 0x100;

const COMP_TYPE: u32 = 0x4457_0140;
/// The version from which IC_SDA_HOLD exists, "1.10a".
const SDA_HOLD_VERSION: u32 = 0x3131_302a;

const CON_MASTER: u32 = 1 << 0;
const CON_SPEED_STANDARD: u32 = 1 << 1;
const CON_SPEED_FAST: u32 = 2 << 1;
const CON_10BIT_MASTER: u32 = 1 << 4;
const CON_RESTART_EN: u32 = 1 << 5;
const CON_SLAVE_DISABLE: u32 = 1 << 6;

const TAR_10BIT_MASTER: u32 = 1 << 12;

const CMD_READ: u32 = 1 << 8;
const CMD_STOP: u32 = 1 << 9;
const CMD_RESTART: u32 = 1 << 10;

const INTR_TX_ABRT: u32 = 1 << 6;
const INTR_STOP_DET: u32 = 1 << 9;
const STATUS_MASTER_ACTIVITY: u32 = 1 << 5;
const ENABLE: u32 = 1 << 0;
const ENABLE_ABORT: u32 = 1 << 1;

/// The abort sources: no acknowledgement of the address (7-bit, either
/// byte of a 10-bit one) or of a data byte, and arbitration lost.
const ABRT_ADDRESS_NACK: u32 = 0b111;
const ABRT_DATA_NACK: u32 = 1 << 3;
const ABRT_LOST: u32 = 1 << 12;

/// How long the bus may stay busy before a transfer, and how long each
/// byte may take beyond its time on the bus.
const BUSY_US: u64 = 20_000;
const SLACK_US: u64 = 20_000;
/// How often a FIFO is looked at while bytes are left to queue.
const SPIN_US: u64 = 10;

/// The I2C specification's (NXP's UM10204) shortest SCL high and low
/// times and its longest fall time, in nanoseconds, for each speed.
const STANDARD_NS: (u64, u64, u64) = (4000, 4700, 300);
const FAST_NS: (u64, u64, u64) = (600, 1300, 300);

/// The SCL counts and SDA hold time of a speed, in the controller's
/// clock cycles: from the firmware's `SSCN` or `FMCN`, or computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// SCL's high count.
    pub high: u16,
    /// SCL's low count.
    pub low: u16,
    /// SDA's hold time, if known.
    pub hold: Option<u16>,
}

impl Timing {
    /// The counts the I2C specification's times need on a clock of
    /// `clock_hz`, rounded up, and an SDA hold time of `hold_ns` (none
    /// keeps the controller's).
    pub fn computed(speed: Speed, clock_hz: u64, hold_ns: Option<u64>) -> Timing {
        let (high, low, fall) = match speed {
            Speed::Standard => STANDARD_NS,
            Speed::Fast => FAST_NS,
        };
        let cycles = |ns: u64| (clock_hz * ns).div_ceil(1_000_000_000);
        // The controller adds about 3 cycles to the high count and 1 to
        // the low one.
        let count = |ns: u64, less: u64| cycles(ns).saturating_sub(less).min(0xffff) as u16;
        Timing {
            high: count(high + fall, 3),
            low: count(low + fall, 1),
            hold: hold_ns.map(|ns| cycles(ns).min(0xffff) as u16),
        }
    }
}

/// Why a controller is not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unusable {
    /// Its registers read all ones.
    Absent,
    /// IC_COMP_TYPE is not DesignWare's: the value read.
    NotDesignWare(u32),
    /// The window is smaller than the registers.
    Registers,
}

impl core::fmt::Display for Unusable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unusable::Absent => f.write_str("its registers read all ones"),
            Unusable::NotDesignWare(read) => {
                write!(f, "it is not a DesignWare controller (IC_COMP_TYPE reads {read:#x})")
            }
            Unusable::Registers => f.write_str("the firmware gives too small a register range"),
        }
    }
}

/// A DesignWare I2C controller, set up as a master.
pub struct DesignWare<P> {
    registers: Mmio,
    platform: P,
    speed: Speed,
    tx_depth: usize,
    rx_depth: usize,
    version: u32,
}

impl<P: Platform> DesignWare<P> {
    /// The controller at `registers`, set up for `speed` with `timing`.
    pub fn new(
        registers: Mmio,
        platform: P,
        speed: Speed,
        timing: Timing,
    ) -> Result<DesignWare<P>, Unusable> {
        if registers.len() < SIZE {
            return Err(Unusable::Registers);
        }
        match registers.read32(IC_COMP_TYPE) {
            COMP_TYPE => {}
            u32::MAX => return Err(Unusable::Absent),
            other => return Err(Unusable::NotDesignWare(other)),
        }
        let params = registers.read32(IC_COMP_PARAM_1);
        let mut controller = DesignWare {
            tx_depth: ((params >> 16) & 0xff) as usize + 1,
            rx_depth: ((params >> 8) & 0xff) as usize + 1,
            version: registers.read32(IC_COMP_VERSION),
            registers,
            platform,
            speed,
        };
        controller.set_up(timing);
        Ok(controller)
    }

    /// Its FIFOs' depths: transmit, then receive.
    pub fn depths(&self) -> (usize, usize) {
        (self.tx_depth, self.rx_depth)
    }

    /// Its version, as IC_COMP_VERSION reads ("1.21a" is 0x3132312a).
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The bus's speed.
    pub fn speed(&self) -> Speed {
        self.speed
    }

    fn set_up(&mut self, timing: Timing) {
        let _ = self.disable();
        let (high, low, speed) = match self.speed {
            Speed::Standard => (IC_SS_SCL_HCNT, IC_SS_SCL_LCNT, CON_SPEED_STANDARD),
            Speed::Fast => (IC_FS_SCL_HCNT, IC_FS_SCL_LCNT, CON_SPEED_FAST),
        };
        self.registers.write32(high, timing.high.into());
        self.registers.write32(low, timing.low.into());
        if let Some(hold) = timing.hold
            && self.version >= SDA_HOLD_VERSION
        {
            // The transmit hold, and a receive hold of a cycle.
            self.registers.write32(IC_SDA_HOLD, u32::from(hold) | 1 << 16);
        }
        self.registers.write32(IC_CON, CON_MASTER | speed | CON_RESTART_EN | CON_SLAVE_DISABLE);
        // Polled: no interrupt is raised.
        self.registers.write32(IC_INTR_MASK, 0);
    }

    /// Disables the controller, waiting for it to say so.
    fn disable(&self) -> Result<(), Error> {
        self.registers.write32(IC_ENABLE, 0);
        for _ in 0..100 {
            if self.registers.read32(IC_ENABLE_STATUS) & ENABLE == 0 {
                return Ok(());
            }
            self.platform.wait_us(25);
        }
        Err(Error::Timeout)
    }

    /// Runs `messages` with the device at `address` (`ten_bit` for a
    /// 10-bit one): each after the last with a repeated start, then a
    /// stop.
    pub fn transfer(
        &mut self,
        address: u16,
        ten_bit: bool,
        messages: &mut [Message<'_>],
    ) -> Result<(), Error> {
        let limit = if ten_bit { 0x3ff } else { 0x7f };
        if address > limit || messages.is_empty() || messages.iter().any(|m| m.len() == 0) {
            return Err(Error::Invalid);
        }
        self.wait_idle()?;
        self.disable()?;
        // Older controllers take a 10-bit address's flag in IC_TAR, newer
        // ones in IC_CON.
        let con = self.registers.read32(IC_CON) & !CON_10BIT_MASTER;
        let (con, tar) = if ten_bit {
            (con | CON_10BIT_MASTER, u32::from(address) | TAR_10BIT_MASTER)
        } else {
            (con, address.into())
        };
        self.registers.write32(IC_CON, con);
        self.registers.write32(IC_TAR, tar);
        self.registers.read32(IC_CLR_INTR);
        self.registers.write32(IC_ENABLE, ENABLE);
        let result = self.run(messages);
        if result.is_err() {
            self.abort();
        }
        self.registers.read32(IC_CLR_STOP_DET);
        result
    }

    fn wait_idle(&self) -> Result<(), Error> {
        let mut waited = 0;
        while self.registers.read32(IC_STATUS) & STATUS_MASTER_ACTIVITY != 0 {
            if waited >= BUSY_US {
                return Err(Error::Timeout);
            }
            self.platform.wait_us(100);
            waited += 100;
        }
        Ok(())
    }

    /// Queues the messages' bytes and reads, and takes in what is read.
    fn run(&mut self, messages: &mut [Message<'_>]) -> Result<(), Error> {
        let total: usize = messages.iter().map(Message::len).sum();
        let reads: usize =
            messages.iter().filter(|m| matches!(m, Message::Read(_))).map(Message::len).sum();
        let byte_us = 9_000_000 / u64::from(self.speed.hz()) + 1;
        // Each byte, and each message's address.
        let budget = SLACK_US + byte_us * (total + messages.len()) as u64;
        let (mut queued, mut received, mut waited) = (0, 0, 0);
        // Where the next command and the next read byte go.
        let (mut send, mut sent) = (0, 0);
        let (mut take, mut taken) = (0, 0);
        loop {
            // Take in what was read.
            let mut ready = self.registers.read32(IC_RXFLR) as usize;
            while ready > 0 && received < reads {
                while !matches!(messages[take], Message::Read(_)) {
                    take += 1;
                }
                let Message::Read(buffer) = &mut messages[take] else { unreachable!() };
                buffer[taken] = self.registers.read32(IC_DATA_CMD) as u8;
                received += 1;
                ready -= 1;
                taken += 1;
                if taken == buffer.len() {
                    (take, taken) = (take + 1, 0);
                }
            }
            // Queue what fits, with no more reads in flight than the
            // receive FIFO holds.
            let mut room = self.tx_depth.saturating_sub(self.registers.read32(IC_TXFLR) as usize);
            while room > 0 && send < messages.len() {
                let message = &messages[send];
                let reading = matches!(message, Message::Read(_));
                if reading && queued - received >= self.rx_depth {
                    break;
                }
                let mut command = match message {
                    Message::Write(bytes) => u32::from(bytes[sent]),
                    Message::Read(_) => CMD_READ,
                };
                if sent == 0 && send > 0 {
                    command |= CMD_RESTART;
                }
                if send == messages.len() - 1 && sent == message.len() - 1 {
                    command |= CMD_STOP;
                }
                self.registers.write32(IC_DATA_CMD, command);
                if reading {
                    queued += 1;
                }
                room -= 1;
                sent += 1;
                if sent == message.len() {
                    (send, sent) = (send + 1, 0);
                }
            }
            let raw = self.registers.read32(IC_RAW_INTR_STAT);
            if raw & INTR_TX_ABRT != 0 {
                let source = self.registers.read32(IC_TX_ABRT_SOURCE);
                self.registers.read32(IC_CLR_TX_ABRT);
                return Err(if source & ABRT_ADDRESS_NACK != 0 {
                    Error::NoDevice
                } else if source & ABRT_DATA_NACK != 0 {
                    Error::Nack
                } else if source & ABRT_LOST != 0 {
                    Error::ArbitrationLost
                } else {
                    Error::Aborted(source)
                });
            }
            if send == messages.len() && received == reads && raw & INTR_STOP_DET != 0 {
                return Ok(());
            }
            if waited > budget {
                return Err(Error::Timeout);
            }
            if send < messages.len() {
                self.platform.spin_us(SPIN_US);
                waited += SPIN_US;
            } else {
                // All queued: sleep for what is left on the bus.
                let left = u64::from(self.registers.read32(IC_TXFLR)) + 1;
                let us = (byte_us * left).clamp(20, 1000);
                self.platform.wait_us(us);
                waited += us;
            }
        }
    }

    /// Stops a transfer that failed and leaves the controller disabled.
    fn abort(&self) {
        self.registers.write32(IC_ENABLE, ENABLE | ENABLE_ABORT);
        for _ in 0..40 {
            if self.registers.read32(IC_ENABLE) & ENABLE_ABORT == 0 {
                break;
            }
            self.platform.wait_us(25);
        }
        self.registers.read32(IC_CLR_TX_ABRT);
        let _ = self.disable();
    }
}
