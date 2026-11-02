// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Application Processing Engine of the BCM5717 family, the BCM5761
//! and the BCM5762: a processor in the chip whose firmware, when the
//! board has it, shares the port with a management controller (NC-SI or
//! DASH). The driver and the firmware take its locks before either
//! resets the chip or reaches the PHY, and the driver tells the firmware
//! when it starts and when it stops, as FreeBSD's `bge(4)` does.

use radix_hw::Mmio;

use crate::reg::ape;

/// How long a lock is waited for: 20,000 looks, 50 µs apart.
const LOCK_LOOKS: u32 = 20_000;
const LOCK_STEP_US: u32 = 50;
/// How many times an event waits 100 µs for the firmware to take the
/// last.
const EVENT_LOOKS: u32 = 10;
const EVENT_STEP_US: u32 = 100;

/// The APE, as this function of the chip reaches it.
pub struct Ape {
    regs: Mmio,
    /// The BCM5761's locks are elsewhere.
    bcm5761: bool,
    /// The PCI function, 0 to 3, whose locks these are.
    function: u8,
    /// Whether its firmware runs: without it no lock is taken.
    running: bool,
}

/// What the driver tells the firmware.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Start,
    Unload,
}

impl Ape {
    /// The APE behind `regs`, the chip's third BAR, for PCI function
    /// `function`: clears the locks a driver before this one may have
    /// left, and looks for its firmware. The host must have been allowed
    /// to write the APE's registers.
    pub fn new(regs: Mmio, bcm5761: bool, function: u8) -> Ape {
        let mut ape = Ape { regs, bcm5761, function, running: false };
        let grant = ape.grant();
        for lock in ape::LOCK_PHY0..=ape::LOCK_GPIO {
            let bit = ape.release_bit(lock);
            ape.regs.write32(grant + 4 * lock, bit);
        }
        ape.running = ape.regs.read32(ape::SEG_SIG) == ape::SEG_SIG_MAGIC
            && ape.regs.read32(ape::FW_STATUS) & ape::FW_STATUS_READY != 0;
        ape
    }

    /// Whether the APE's firmware runs.
    pub fn running(&self) -> bool {
        self.running
    }

    /// The lock on this function's PHY.
    pub fn phy_lock(&self) -> usize {
        match self.function {
            0 => ape::LOCK_PHY0,
            1 => ape::LOCK_PHY1,
            2 => ape::LOCK_PHY2,
            _ => ape::LOCK_PHY3,
        }
    }

    fn request(&self) -> usize {
        if self.bcm5761 { ape::LOCK_REQ } else { ape::PER_LOCK_REQ }
    }

    fn grant(&self) -> usize {
        if self.bcm5761 { ape::LOCK_GRANT } else { ape::PER_LOCK_GRANT }
    }

    /// The bit this driver requests and releases lock `lock` with: the
    /// first driver's for the PHY locks and function 0, the function's
    /// own otherwise.
    fn release_bit(&self, lock: usize) -> u32 {
        match lock {
            ape::LOCK_PHY0 | ape::LOCK_PHY1 | ape::LOCK_PHY2 | ape::LOCK_PHY3 => ape::LOCK_DRIVER0,
            _ if self.function == 0 => ape::LOCK_DRIVER0,
            _ => 1 << self.function,
        }
    }

    /// Takes lock `lock`, waiting up to a second for the firmware to
    /// give it up: true once it is held, or when there is no firmware to
    /// share it with; false, the request withdrawn, when it was not
    /// given.
    pub fn lock(&self, lock: usize, delay: &mut dyn FnMut(u32)) -> bool {
        if !self.running || (lock == ape::LOCK_GPIO && self.bcm5761) {
            return true;
        }
        let bit = self.release_bit(lock);
        let (request, grant) = (self.request() + 4 * lock, self.grant() + 4 * lock);
        self.regs.write32(request, bit);
        for _ in 0..LOCK_LOOKS {
            if self.regs.read32(grant) == bit {
                return true;
            }
            delay(LOCK_STEP_US);
        }
        self.regs.write32(grant, bit);
        false
    }

    /// Lets lock `lock` go.
    pub fn unlock(&self, lock: usize) {
        if !self.running || (lock == ape::LOCK_GPIO && self.bcm5761) {
            return;
        }
        self.regs.write32(self.grant() + 4 * lock, self.release_bit(lock));
    }

    /// Hands the firmware `event`, once it has taken the last.
    fn send_event(&self, event: u32, delay: &mut dyn FnMut(u32)) {
        for _ in 0..EVENT_LOOKS {
            if !self.lock(ape::LOCK_MEM, delay) {
                return;
            }
            let status = self.regs.read32(ape::EVENT_STATUS);
            if status & ape::EVENT_STATUS_EVENT_PENDING == 0 {
                self.regs.write32(ape::EVENT_STATUS, event | ape::EVENT_STATUS_EVENT_PENDING);
                self.unlock(ape::LOCK_MEM);
                self.regs.write32(ape::EVENT, ape::EVENT_1);
                return;
            }
            self.unlock(ape::LOCK_MEM);
            delay(EVENT_STEP_US);
        }
    }

    /// Tells the firmware that the driver starts or stops. Starting, it
    /// counts the starts in the host's segment of the APE's memory, says
    /// the driver neither takes the PHY lock for the firmware nor beats a
    /// heartbeat, and names no driver: the names are Broadcom's to give.
    pub fn tell(&self, state: State, delay: &mut dyn FnMut(u32)) {
        if !self.running {
            return;
        }
        let event = match state {
            State::Start => {
                let count = if self.regs.read32(ape::HOST_SEG_SIG) == ape::HOST_SEG_SIG_MAGIC {
                    self.regs.read32(ape::HOST_INIT_COUNT).wrapping_add(1)
                } else {
                    0
                };
                self.regs.write32(ape::HOST_INIT_COUNT, count);
                self.regs.write32(ape::HOST_SEG_SIG, ape::HOST_SEG_SIG_MAGIC);
                self.regs.write32(ape::HOST_SEG_LEN, ape::HOST_SEG_LEN_MAGIC);
                self.regs.write32(ape::HOST_BEHAVIOR, ape::HOST_BEHAV_NO_PHYLOCK);
                self.regs.write32(ape::HOST_HEARTBEAT_INT_MS, 0);
                self.regs.write32(ape::HOST_DRVR_STATE, ape::HOST_DRVR_STATE_START);
                ape::EVENT_STATUS_STATE_START
            }
            State::Unload => {
                self.regs.write32(ape::HOST_DRVR_STATE, ape::HOST_DRVR_STATE_UNLOAD);
                ape::EVENT_STATUS_STATE_UNLOAD
            }
        };
        self.send_event(
            event | ape::EVENT_STATUS_DRIVER_EVNT | ape::EVENT_STATUS_STATE_CHNGE,
            delay,
        );
    }
}
