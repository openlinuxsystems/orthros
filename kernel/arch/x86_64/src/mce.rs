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
//! An uncorrected error raises #MC. [`triage`] decides what it leaves to
//! do ([`radix_mca::triage`]): the code goes on, or the program that met
//! bad memory ends (ADR 0168), or the machine stops. Unless it stops, the
//! machine check ends (clearing MCG_STATUS.MCIP, so that the next one is
//! taken rather than shutting the CPU down) and the errors stay in their
//! banks for the next poll, since the handler may not log; the program's
//! end polls first. A fatal one panics with [`describe`]'s account of it.
//! The poll names the page of an uncorrected error the CPU went on after,
//! which the kernel retires.
//!
//! Corrected errors interrupt where the CPU can (ADR 0169,
//! [`radix_mca::corrected`]): through the local APIC's CMCI entry on
//! Intel, from the banks each CPU claims, and through the extended LVT
//! entry the firmware named on AMD, from the banks whose error counter can
//! interrupt. Both use [`VECTOR_CORRECTED_ERROR`], whose handler only asks
//! the kernel to read the banks; the poll gives each error's count, from
//! the status's on Intel and the bank's counter on AMD, which it starts
//! again.
//!
//! On AMD's Zen, whose banks have the scalable architecture's registers
//! too ([`radix_mca::smca`], ADR 0204), [`init_cpu`] sets each bank's
//! MCA_CONFIG to say the kernel reads them, and each error read names
//! the block of the chip its bank belongs to (MCA_IPID) and its syndrome
//! (MCA_SYND). A deferred error is also kept in MCA_DESTAT, which the
//! poll clears with the status, and reads when the status lost it.
//!
//! [`VECTOR_CORRECTED_ERROR`]: crate::interrupts::VECTOR_CORRECTED_ERROR

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};

use radix_klog::TextBuf;
use radix_mca::corrected::{self, CAP_CMCI, Msrs};
use radix_mca::smca::{self, CONFIG, DEADDR, DESTAT, IPID, SYND};
use radix_mca::{
    ADDRESS_VALID, AMD_DEFERRED, BankError, CAP_COUNT, CAP_CTL, CAP_RECOVERY, MISC_VALID, Mca,
    RIPV, UNCORRECTED, VALID, Verdict, bad_page, fatal,
};
use radix_sync::{Once, SpinLock};

use crate::apic;
use crate::cpu::{self, rdmsr, wrmsr};
use crate::interrupts::VECTOR_CORRECTED_ERROR;

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
/// The CPU recovers from uncorrected errors: IA32_MCG_CAP's software
/// error recovery, or AMD's SUCCOR.
static RECOVERY: AtomicBool = AtomicBool::new(false);
/// The machine-check architecture is AMD's.
static AMD: AtomicBool = AtomicBool::new(false);
/// The banks have the scalable architecture's registers too (AMD's Zen).
static SCALABLE: AtomicBool = AtomicBool::new(false);
/// IA32_MCG_CAP's IA32_MCG_CTL.
static HAS_CTL: AtomicBool = AtomicBool::new(false);
/// Bank 0's control belongs to the firmware.
static SKIP_BANK0: AtomicBool = AtomicBool::new(false);
/// Intel's CMCI: IA32_MCG_CAP has it, and the local APIC its LVT entry.
static CMCI: AtomicBool = AtomicBool::new(false);
/// The banks some CPU claimed for CMCI, or whose AMD counter interrupts,
/// one bit each; 0 if none does.
static INTERRUPTING: AtomicU32 = AtomicU32::new(0);
/// AMD: the extended LVT entry the counters raise; [`NO_LVT`] if none.
static THRESHOLD_LVT: AtomicU8 = AtomicU8::new(NO_LVT);
const NO_LVT: u8 = u8::MAX;
/// The kernel's handler for [`VECTOR_CORRECTED_ERROR`].
static CORRECTED_HANDLER: Once<fn()> = Once::new();
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
    let amd = cpu::is_amd();
    let ras =
        if cpu::max_extended_leaf() >= 0x8000_0007 { cpu::cpuid(0x8000_0007, 0).ebx } else { 0 };
    // SUCCOR: CPUID 0x80000007's EBX bit 1.
    let succor = ras & (1 << 1) != 0;
    AMD.store(amd, Ordering::Relaxed);
    SCALABLE.store(amd && ras & smca::CPUID_SMCA != 0, Ordering::Relaxed);
    RECOVERY.store(if amd { succor } else { cap & CAP_RECOVERY != 0 }, Ordering::Relaxed);
    HAS_CTL.store(cap & CAP_CTL != 0, Ordering::Relaxed);
    SKIP_BANK0.store(cpu::is_intel() && family == 6 && model < 0x1a, Ordering::Relaxed);
    CMCI.store(!amd && cap & CAP_CMCI != 0, Ordering::Relaxed);
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
        // Every scalable bank has MCA_CONFIG; McaX only says the kernel
        // reads the scalable registers.
        if SCALABLE.load(Ordering::Relaxed) {
            for bank in 0..banks {
                let config = smca::register(bank, CONFIG);
                wrmsr(config, smca::configure(rdmsr(config)));
            }
        }
        cpu::write_cr4(cpu::cr4() | CR4_MCE);
    }
}

/// The running CPU's machine-check registers, for [`radix_mca::corrected`],
/// which reads and writes only the banks' IA32_MCi_CTL2 and MCi_MISC.
struct Registers;

impl Msrs for Registers {
    fn read(&mut self, msr: u32) -> u64 {
        // SAFETY: `corrected` reads only the registers of banks below
        // IA32_MCG_CAP's count, IA32_MCi_CTL2 where it says CMCI exists.
        unsafe { rdmsr(msr) }
    }

    fn write(&mut self, msr: u32, value: u64) {
        // SAFETY: as in `read`; the values only set up CMCI and the error
        // counters.
        unsafe { wrmsr(msr, value) }
    }
}

/// Sets up this CPU's interrupt for corrected errors, once its local APIC
/// is: claims its banks for CMCI on Intel, or starts its banks' error
/// counters on AMD, and points the APIC's entry at
/// [`VECTOR_CORRECTED_ERROR`] if any bank interrupts. The CPU's banks are
/// polled either way.
pub fn init_cpu_interrupt() {
    let banks = BANKS.load(Ordering::Acquire);
    if banks == 0 {
        return;
    }
    if AMD.load(Ordering::Relaxed) {
        let found = corrected::arm_thresholds(&mut Registers, 0..banks, apic::extended_lvts());
        if let Some(lvt) = found.lvt.filter(|_| found.interrupting != 0) {
            THRESHOLD_LVT.store(lvt, Ordering::Relaxed);
            apic::set_extended_lvt(lvt, VECTOR_CORRECTED_ERROR, false);
            INTERRUPTING.fetch_or(found.interrupting, Ordering::Relaxed);
        }
    } else if CMCI.load(Ordering::Relaxed) && apic::has_cmci() {
        let first = usize::from(SKIP_BANK0.load(Ordering::Relaxed));
        let claimed = corrected::claim_cmci(&mut Registers, first..banks);
        if claimed != 0 {
            apic::set_cmci(VECTOR_CORRECTED_ERROR, false);
            INTERRUPTING.fetch_or(claimed, Ordering::Relaxed);
        }
    }
}

/// What interrupts for corrected errors, for the log.
pub fn corrected_interrupt() -> Option<&'static str> {
    if INTERRUPTING.load(Ordering::Relaxed) == 0 {
        None
    } else if AMD.load(Ordering::Relaxed) {
        Some("AMD's error thresholds")
    } else {
        Some("CMCI")
    }
}

/// Sets the kernel's handler for corrected errors.
pub fn set_corrected_handler(handler: fn()) {
    let _ = CORRECTED_HANDLER.set(handler);
}

/// [`VECTOR_CORRECTED_ERROR`]'s handler.
pub fn on_corrected_interrupt() {
    radix_klog::tracepoint!("mce.corrected", crate::percpu::current_index());
    if let Some(handler) = CORRECTED_HANDLER.get() {
        handler();
    }
}

/// Masks this CPU's interrupt for corrected errors, or unmasks it.
pub fn mask_corrected(masked: bool) {
    let lvt = THRESHOLD_LVT.load(Ordering::Relaxed);
    if lvt != NO_LVT {
        apic::set_extended_lvt(lvt, VECTOR_CORRECTED_ERROR, masked);
    } else if INTERRUPTING.load(Ordering::Relaxed) != 0 {
        // A CPU that claimed no bank never gets one; masking it is harmless.
        apic::set_cmci(VECTOR_CORRECTED_ERROR, masked);
    }
}

/// How many banks each CPU reports in.
pub fn banks() -> usize {
    BANKS.load(Ordering::Relaxed)
}

/// Whether the banks are AMD's scalable ones.
pub fn scalable_banks() -> bool {
    SCALABLE.load(Ordering::Relaxed)
}

fn mca() -> Mca {
    Mca { amd: AMD.load(Ordering::Relaxed), recovery: RECOVERY.load(Ordering::Relaxed) }
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
            smca: scalable(bank, status),
        })
    }
}

/// What a scalable bank's own registers say about its error with
/// `status`; `None` on banks without them.
fn scalable(bank: usize, status: u64) -> Option<smca::Info> {
    if !SCALABLE.load(Ordering::Relaxed) {
        return None;
    }
    // SAFETY: CPUID says the banks are scalable, so each bank below
    // IA32_MCG_CAP's count has these registers.
    unsafe {
        Some(smca::Info {
            ipid: rdmsr(smca::register(bank, IPID)),
            synd: (status & smca::STATUS_SYND_VALID != 0)
                .then(|| rdmsr(smca::register(bank, SYND))),
            lsb_in_status: rdmsr(smca::register(bank, CONFIG)) & smca::CONFIG_LSB_IN_STATUS != 0,
        })
    }
}

/// A scalable bank's deferred error in MCA_DESTAT, which the status lost,
/// if it holds one.
fn read_deferred(bank: usize) -> Option<BankError> {
    // SAFETY: as in `scalable`; the address is read only when valid.
    unsafe {
        let status = rdmsr(smca::register(bank, DESTAT));
        (status & VALID != 0 && status & AMD_DEFERRED != 0).then(|| BankError {
            status,
            address: (status & ADDRESS_VALID != 0).then(|| rdmsr(smca::register(bank, DEADDR))),
            misc: None,
            smca: scalable(bank, status),
        })
    }
}

/// Reads and clears this CPU's banks, then calls `found` with each error,
/// how many it stands for, and the page it says went bad (not under the
/// lock, so that it may log). On AMD each bank's counter starts again; on
/// Zen, a deferred error the status lost comes from MCA_DESTAT, after the
/// status's. A bank at a time, so that only one bank's errors are on the
/// stack.
pub fn poll(found: &mut radix_hal::MachineCheckFound<'_>) {
    let amd = AMD.load(Ordering::Relaxed);
    let scalable = SCALABLE.load(Ordering::Relaxed);
    let mca = mca();
    let cmci = CMCI.load(Ordering::Relaxed);
    for bank in 0..banks() {
        let (error, deferred, counted) = {
            let _one = POLLING.lock();
            let error = read(bank);
            if error.is_some() {
                // SAFETY: writing 0 to a status register clears it.
                unsafe { wrmsr(register(bank, 1), 0) };
            }
            let mut deferred = None;
            if scalable {
                let held = error.is_some_and(|error| error.status & AMD_DEFERRED != 0);
                if !held {
                    deferred = read_deferred(bank);
                }
                if held || deferred.is_some() {
                    // SAFETY: as for the status.
                    unsafe { wrmsr(smca::register(bank, DESTAT), 0) };
                }
            }
            let counted = if amd { corrected::take_threshold(&mut Registers, bank) } else { 0 };
            (error, deferred, counted)
        };
        for error in [error, deferred].iter().flatten() {
            let count = corrected::errors(error, cmci);
            found(&radix_hal::MachineCheck {
                bank,
                corrected: error.corrected(),
                count: if error.corrected() { count.max(counted) } else { count },
                memory: corrected::memory(error).map(|memory| memory.channel),
                page: bad_page(error, mca),
                what: error,
            });
        }
    }
}

/// In the #MC handler: what the machine check leaves to do, `user` if it
/// came in user mode. Unless that is to stop the machine, it ends the
/// machine check.
pub fn triage(user: bool) -> Verdict {
    let banks = banks();
    if banks == 0 {
        return Verdict::Fatal;
    }
    // SAFETY: reporting is on, so the registers exist.
    let status = unsafe { rdmsr(MCG_STATUS) };
    let verdict = radix_mca::triage(status, mca(), user, (0..banks).filter_map(read));
    if verdict != Verdict::Fatal {
        // SAFETY: clearing MCG_STATUS ends the machine check.
        unsafe { wrmsr(MCG_STATUS, 0) };
        radix_klog::tracepoint!("mce.recovered", status);
    }
    verdict
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
