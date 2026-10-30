// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Machine checks (Phase 11, ADR 0145): the hardware errors the CPUs'
//! machine-check banks report.
//!
//! An error a CPU cannot go on after panics where the architecture takes
//! it. The rest wait in the banks for the `mce` thread, which reads every
//! CPU's banks at the end of boot, finding what an earlier boot left
//! there, and then every [`DEFAULT_SECONDS`] (`mce-poll=seconds`), as
//! Linux polls where the CPU has no interrupt for corrected errors. Each
//! error is logged with its CPU and bank: a warning for one the hardware
//! corrected, an error for one it did not but the CPU went on after.
//!
//! Where the CPU interrupts for corrected errors (ADR 0169: Intel's CMCI,
//! AMD's thresholds), the interrupt wakes the thread to read the banks at
//! once. More than 15 interrupts in a second are a storm: the interrupts
//! are masked and the banks read every second, until 30 seconds go by
//! without a corrected error ([`radix_mca::corrected::Storm`]).
//!
//! The page of memory an uncorrected error names leaves use for good (ADR
//! 0168): out of the frame allocator at once if it is free, or when it is
//! freed ([`radix_mm::Frames::retire`]). A program that read it has ended
//! by then, or its SIGBUS handler runs (`user::on_fault`, which reads the
//! banks first).
//!
//! Memory controllers' errors are counted ([`memory_errors`]), by bank and
//! channel where the bank says, for `/kernel/devices/system/edac`.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use radix_hal::{Cpu, CpuLocal, MachineCheck};
use radix_klog::{error, info, warn};
use radix_mca::corrected::{STORM_INTERRUPTS, STORM_READ_NS, Storm};
use radix_mm::{FRAMES, Retired};
use radix_sync::{Once, SpinLock};

use crate::Arch;
use crate::sched::{self, Options, Policy, ThreadRef};
use crate::work;

/// How often the banks are read, by default: Linux's interval too.
const DEFAULT_SECONDS: u64 = 300;

/// The CPU the thread runs on, which posts the reads to the others.
const HOME: usize = 0;

/// The first read, at the end of boot.
static AT_BOOT: AtomicBool = AtomicBool::new(true);

/// The thread, for the interrupt to wake.
static THREAD: Once<ThreadRef> = Once::new();
/// Interrupts for corrected errors the thread has not counted yet.
static INTERRUPTS: AtomicU32 = AtomicU32::new(0);
/// Whether the read under way found a corrected error.
static FOUND_CORRECTED: AtomicBool = AtomicBool::new(false);

/// The most bank and channel pairs counted apart; errors past them count
/// as without a channel.
const MAX_CHANNELS: usize = 32;

/// The memory errors the banks reported since boot: Linux's EDAC counts of
/// one memory controller (`mc0`), whose channels are the bank and channel
/// pairs the errors named, in the order they first did.
#[derive(Clone, Copy, Debug)]
pub struct MemoryErrors {
    /// Corrected and uncorrected errors, channel or not.
    pub corrected: u64,
    pub uncorrected: u64,
    /// Those whose bank named no channel.
    pub corrected_unplaced: u64,
    pub uncorrected_unplaced: u64,
    channels: [Channel; MAX_CHANNELS],
    len: usize,
}

/// A bank and channel's errors.
#[derive(Clone, Copy, Debug, Default)]
pub struct Channel {
    pub bank: usize,
    pub channel: u8,
    pub corrected: u64,
    pub uncorrected: u64,
}

impl MemoryErrors {
    const fn new() -> MemoryErrors {
        MemoryErrors {
            corrected: 0,
            uncorrected: 0,
            corrected_unplaced: 0,
            uncorrected_unplaced: 0,
            channels: [Channel { bank: 0, channel: 0, corrected: 0, uncorrected: 0 }; MAX_CHANNELS],
            len: 0,
        }
    }

    /// The channels, in the order they first had an error.
    pub fn channels(&self) -> &[Channel] {
        &self.channels[..self.len]
    }

    fn count(&mut self, bank: usize, channel: Option<u8>, corrected: bool, count: u64) {
        let known = channel.and_then(|channel| self.place(bank, channel));
        let (all, unplaced) = if corrected {
            (&mut self.corrected, &mut self.corrected_unplaced)
        } else {
            (&mut self.uncorrected, &mut self.uncorrected_unplaced)
        };
        *all = all.saturating_add(count);
        let Some(index) = known else {
            *unplaced = unplaced.saturating_add(count);
            return;
        };
        let place = &mut self.channels[index];
        let counter = if corrected { &mut place.corrected } else { &mut place.uncorrected };
        *counter = counter.saturating_add(count);
    }

    /// The index of the bank and channel's counts, made if they are new;
    /// `None` once [`MAX_CHANNELS`] are.
    fn place(&mut self, bank: usize, channel: u8) -> Option<usize> {
        if let Some(index) =
            self.channels().iter().position(|c| c.bank == bank && c.channel == channel)
        {
            return Some(index);
        }
        let slot = self.channels.get_mut(self.len)?;
        *slot = Channel { bank, channel, ..Channel::default() };
        self.len += 1;
        Some(self.len - 1)
    }
}

static MEMORY_ERRORS: SpinLock<MemoryErrors> = SpinLock::new(MemoryErrors::new());

/// The memory errors counted since boot.
pub fn memory_errors() -> MemoryErrors {
    *MEMORY_ERRORS.lock_irq()
}

/// Whether the CPUs report hardware errors: the banks are read.
pub fn on() -> bool {
    Arch::machine_check_banks() > 0
}

/// Starts the thread, reading the banks every `seconds` (`mce-poll`).
pub fn start(seconds: Option<&str>) {
    let banks = Arch::machine_check_banks();
    if banks == 0 {
        info!("Machine check: off (the CPU has no machine-check architecture, or `mce=off`)");
        return;
    }
    let seconds = match seconds.map(str::parse::<u64>) {
        None => DEFAULT_SECONDS,
        Some(Ok(seconds)) if (1..=86_400).contains(&seconds) => seconds,
        Some(_) => {
            warn!("Machine check: `mce-poll` takes 1 to 86400 seconds; using {DEFAULT_SECONDS}");
            DEFAULT_SECONDS
        }
    };
    match Arch::corrected_error_interrupt() {
        Some(how) => info!(
            "Machine check: {banks} banks on each CPU, read at boot and every {seconds} s, and \
             when {how} says they corrected errors"
        ),
        None => {
            info!("Machine check: {banks} banks on each CPU, read at boot and every {seconds} s");
        }
    }
    Arch::set_corrected_error_handler(on_interrupt);
    let options = Options::new().pinned(HOME).policy(Policy::Fair { nice: 0 });
    if let Err(err) = sched::spawn("mce", run, seconds as usize, options) {
        error!("Machine check: could not start the thread that reads the banks: {err}");
    }
}

/// In an interrupt handler: a CPU's banks corrected errors.
fn on_interrupt() {
    INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    if let Some(thread) = THREAD.get() {
        sched::wake(thread);
    }
}

fn run(seconds: usize) {
    if let Some(me) = sched::current() {
        let _ = THREAD.set(me);
    }
    let mut storm = Storm::new();
    let mut next = 0;
    loop {
        let now = sched::now_ns();
        let interrupts = INTERRUPTS.swap(0, Ordering::Relaxed);
        if storm.interrupts(now, interrupts) {
            warn!(
                "Machine check: a storm of corrected errors (more than {STORM_INTERRUPTS} \
                 interrupts in a second); the banks are read every second instead"
            );
            work::on_all(HOME, || Arch::mask_corrected_errors(true));
        }
        if interrupts > 0 || now >= next {
            FOUND_CORRECTED.store(false, Ordering::Relaxed);
            work::on_all(HOME, read);
            AT_BOOT.store(false, Ordering::Relaxed);
            if storm.read(now, FOUND_CORRECTED.load(Ordering::Relaxed)) {
                info!("Machine check: the storm of corrected errors is over");
                work::on_all(HOME, || Arch::mask_corrected_errors(false));
            }
            let wait = if storm.on() { STORM_READ_NS } else { seconds as u64 * 1_000_000_000 };
            next = now.saturating_add(wait);
        }
        sched::block_until(next, "mce");
    }
}

/// ", 5 of them since the bank was read" for a count above 1.
struct Times(u64);

impl fmt::Display for Times {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            0 | 1 => Ok(()),
            count => write!(f, "; {count} of them since the bank was read"),
        }
    }
}

/// Reads this CPU's banks, logs what they held, counts the memory errors,
/// and retires the pages they name.
pub(crate) fn read() {
    let cpu = Arch::current_cpu();
    let when = if AT_BOOT.load(Ordering::Relaxed) { ", at boot" } else { "" };
    Arch::poll_machine_checks(&mut |found: &MachineCheck<'_>| {
        let MachineCheck { bank, corrected, count, memory, page, what } = *found;
        let times = Times(count);
        if corrected {
            FOUND_CORRECTED.store(true, Ordering::Relaxed);
            warn!("Machine check: CPU {cpu} bank {bank}{when}: {what}{times}");
        } else {
            error!("Machine check: CPU {cpu} bank {bank}{when}: {what}{times}");
        }
        if let Some(channel) = memory {
            MEMORY_ERRORS.lock_irq().count(bank, channel, corrected, count);
        }
        let Some(page) = page else { return };
        match FRAMES.retire(page) {
            Retired::Now => warn!("Machine check: the page at {page:#x} is out of use for good"),
            Retired::WhenFreed => {
                warn!("Machine check: the page at {page:#x} leaves use for good once freed");
            }
            Retired::NotManaged => {}
            Retired::TooMany => {
                error!("Machine check: the page at {page:#x} cannot be retired: too many are");
            }
        }
    });
}
