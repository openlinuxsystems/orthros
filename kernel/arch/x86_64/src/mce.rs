// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Machine checks (Phase 11, ADR 0145): the error-reporting banks of the
//! machine-check architecture.
//!
//! [`init_cpu`] turns reporting on, as Linux does: IA32_MCG_CTL (where
//! there is one) and every bank's IA32_MCi_CTL to all ones, then CR4.MCE,
//! without which a machine check shuts the CPU down. Intel's family 6
//! before model 0x1a keeps bank 0's control for the firmware, so it is
//! left alone there. What the banks hold stays: an error from before a
//! restart waits in its bank until [`poll`] reads and clears it. The
//! kernel polls every CPU's banks (`radix::mce`), as Linux does where
//! there is no interrupt for corrected errors, and logs what it finds.
//!
//! An uncorrected error raises #MC. [`recovered`] decides whether the CPU
//! can go on; if so it ends the machine check (clearing MCG_STATUS.MCIP,
//! so that the next one is taken rather than shutting the CPU down) and
//! leaves the error in its bank for the next poll, since the handler may
//! not log. Otherwise the exception is fatal and the panic gives
//! [`describe`]'s account of it.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use radix_klog::TextBuf;
use radix_mca::{
    ADDRESS_VALID, BankError, CAP_COUNT, CAP_CTL, CAP_RECOVERY, MISC_VALID, RIPV, UNCORRECTED,
    VALID, fatal,
};
use radix_sync::SpinLock;

use crate::cpu::{self, rdmsr, wrmsr};

/// IA32_MCG_CAP, IA32_MCG_STATUS and IA32_MCG_CTL.
const MCG_CAP: u32 = 0x179;
const MCG_STATUS: u32 = 0x17a;
const MCG_CTL: u32 = 0x17b;
/// IA32_MC0_CTL; bank `n`'s registers are 4 apart: control, status,
/// address and miscellaneous.
const MC0_CTL: u32 = 0x400;

/// The most banks read; IA32_MCG_CAP allows 255, CPUs have up to about 32.
const MAX_BANKS: usize = 32;

/// CR4.MCE.
const CR4_MCE: u64 = 1 << 6;

/// How many banks every CPU reports in; 0 while reporting is off.
static BANKS: AtomicUsize = AtomicUsize::new(0);
/// IA32_MCG_CAP's software error recovery.
static RECOVERY: AtomicBool = AtomicBool::new(false);
/// IA32_MCG_CAP's IA32_MCG_CTL.
static HAS_CTL: AtomicBool = AtomicBool::new(false);
/// Bank 0's control belongs to the firmware.
static SKIP_BANK0: AtomicBool = AtomicBool::new(false);
/// Held while a CPU reads and clears its banks, so that a bank several
/// CPUs share is read by one of them.
static POLLING: SpinLock<()> = SpinLock::new(());

fn register(bank: usize, offset: u32) -> u32 {
    MC0_CTL + 4 * bank as u32 + offset
}

/// Turns reporting on for the boot CPU, unless `off` (`mce=off`) or the
/// CPU has no machine-check architecture (CPUID.1:EDX's MCE and MCA).
pub fn init_boot_cpu(off: bool) {
    let edx = cpu::cpuid(1, 0).edx;
    if off || edx & (1 << 7) == 0 || edx & (1 << 14) == 0 {
        return;
    }
    // SAFETY: CPUID says the machine-check architecture's MSRs exist.
    let cap = unsafe { rdmsr(MCG_CAP) };
    let (family, model, _) = cpu::family_model_stepping();
    RECOVERY.store(cap & CAP_RECOVERY != 0, Ordering::Relaxed);
    HAS_CTL.store(cap & CAP_CTL != 0, Ordering::Relaxed);
    SKIP_BANK0.store(cpu::is_intel() && family == 6 && model < 0x1a, Ordering::Relaxed);
    BANKS.store(((cap & CAP_COUNT) as usize).min(MAX_BANKS), Ordering::Release);
    init_cpu();
}

/// Turns reporting on for this CPU, as [`init_boot_cpu`] did for the
/// first.
pub fn init_cpu() {
    let banks = BANKS.load(Ordering::Acquire);
    if banks == 0 {
        return;
    }
    // SAFETY: the boot CPU found the machine-check architecture, and
    // every CPU of a machine has the same; writing all ones to the
    // control registers only enables reporting.
    unsafe {
        if HAS_CTL.load(Ordering::Relaxed) {
            wrmsr(MCG_CTL, u64::MAX);
        }
        let first = usize::from(SKIP_BANK0.load(Ordering::Relaxed));
        for bank in first..banks {
            wrmsr(register(bank, 0), u64::MAX);
        }
        cpu::write_cr4(cpu::cr4() | CR4_MCE);
    }
}

/// How many banks each CPU reports in.
pub fn banks() -> usize {
    BANKS.load(Ordering::Relaxed)
}

/// Bank `bank`'s error, if it holds one.
fn read(bank: usize) -> Option<BankError> {
    // SAFETY: `bank` is below IA32_MCG_CAP's count, so its registers
    // exist, and the address and miscellaneous registers are read only
    // when the status says they hold something.
    unsafe {
        let status = rdmsr(register(bank, 1));
        (status & VALID != 0).then(|| BankError {
            status,
            address: (status & ADDRESS_VALID != 0).then(|| rdmsr(register(bank, 2))),
            misc: (status & MISC_VALID != 0).then(|| rdmsr(register(bank, 3))),
        })
    }
}

/// Reads and clears this CPU's banks, then calls `found` with each error
/// (not under the lock, so that it may log).
pub fn poll(found: &mut dyn FnMut(usize, bool, &dyn core::fmt::Display)) {
    let banks = banks();
    let mut errors = [None; MAX_BANKS];
    {
        let _one = POLLING.lock();
        for (bank, slot) in errors.iter_mut().enumerate().take(banks) {
            *slot = read(bank);
            if slot.is_some() {
                // SAFETY: writing 0 to a status register clears it.
                unsafe { wrmsr(register(bank, 1), 0) };
            }
        }
    }
    for (bank, error) in errors.iter().enumerate() {
        if let Some(error) = error {
            found(bank, error.corrected(), error);
        }
    }
}

/// In the #MC handler: whether the CPU can go on, which ends the machine
/// check. It can when the interrupted code can restart (MCG_STATUS.RIPV)
/// and no bank holds an error it cannot go on after.
pub fn recovered() -> bool {
    let banks = banks();
    if banks == 0 {
        return false;
    }
    // SAFETY: reporting is on, so the registers exist.
    let status = unsafe { rdmsr(MCG_STATUS) };
    let recovery = RECOVERY.load(Ordering::Relaxed);
    if status & RIPV == 0 || (0..banks).filter_map(read).any(|e| fatal(e.status, recovery)) {
        return false;
    }
    // SAFETY: clearing MCG_STATUS ends the machine check.
    unsafe { wrmsr(MCG_STATUS, 0) };
    radix_klog::tracepoint!("mce.recovered", status);
    true
}

/// For the panic after a machine check the CPU cannot go on after: the
/// bank and error that stopped it (the first fatal one, or else the first
/// uncorrected one), and whether the interrupted code can restart.
pub fn describe(out: &mut TextBuf<256>) {
    let banks = banks();
    if banks == 0 {
        return;
    }
    let recovery = RECOVERY.load(Ordering::Relaxed);
    let worst = (0..banks).filter_map(|bank| read(bank).map(|error| (bank, error))).max_by_key(
        |(bank, e)| {
            (fatal(e.status, recovery), e.status & UNCORRECTED != 0, core::cmp::Reverse(*bank))
        },
    );
    match worst {
        Some((bank, error)) => {
            let _ = write!(out, ": bank {bank}: {error}");
        }
        None => {
            let _ = write!(out, ": no bank holds an error");
        }
    }
    // SAFETY: reporting is on, so the register exists.
    if unsafe { rdmsr(MCG_STATUS) } & RIPV == 0 {
        let _ = write!(out, "; the interrupted code cannot restart");
    }
}
