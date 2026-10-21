// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Early microcode loading on Intel processors.
//!
//! When the image includes a microcode file (`ob image --microcode`), GRUB
//! loads it as a module whose command line says `microcode`. Every CPU
//! looks for the newest update for its signature and platform before it
//! does anything else of note, and writes it to the processor if it is
//! newer than what is running. Hyper-threads share microcode, so the second
//! thread of a core finds the update already applied.

use core::sync::atomic::{AtomicU32, Ordering};

use radix_microcode::{CpuId, Update};
use radix_sync::Once;

use crate::cpu::{self, msr};

/// The microcode file from the boot loader.
static FILE: Once<&'static [u8]> = Once::new();
/// APs that loaded an update.
static AP_UPDATES: AtomicU32 = AtomicU32::new(0);

/// Records the microcode module. Called once, before [`load`].
pub fn set_file(bytes: &'static [u8]) {
    let _ = FILE.set(bytes);
}

/// The running microcode revision.
pub fn revision() -> u32 {
    // SAFETY: IA32_BIOS_SIGN_ID exists on Intel CPUs, the only ones this
    // is called on. Writing 0 and running CPUID 1 makes the processor
    // report its revision in the upper half, as the SDM prescribes.
    unsafe {
        cpu::wrmsr(msr::BIOS_SIGN_ID, 0);
        let _ = cpu::cpuid(1, 0);
        (cpu::rdmsr(msr::BIOS_SIGN_ID) >> 32) as u32
    }
}

/// This CPU's identity for matching updates.
pub fn cpu_id() -> CpuId {
    let signature = cpu::cpuid(1, 0).eax;
    let (family, model, _) = cpu::family_model_stepping();
    let platform = if family > 6 || (family == 6 && model >= 5) {
        // SAFETY: IA32_PLATFORM_ID exists on Intel family 6 model 5 and
        // later; reading it has no side effects.
        1 << ((unsafe { cpu::rdmsr(msr::PLATFORM_ID) } >> 50) & 7)
    } else {
        0
    };
    CpuId { signature, platform }
}

fn apply(update: &Update<'_>) -> Result<u32, u32> {
    let data = update.data();
    if !(data.as_ptr() as usize).is_multiple_of(16) {
        return Err(revision());
    }
    // SAFETY: the update passed its checksums and matches this CPU; the
    // processor reads `data` (16-byte aligned, mapped, and never freed)
    // when IA32_BIOS_UPDT_TRIG is written with its linear address, and
    // verifies Intel's signature before applying it.
    unsafe { cpu::wrmsr(msr::BIOS_UPDT_TRIG, data.as_ptr() as u64) };
    let now = revision();
    if now == update.revision() { Ok(now) } else { Err(now) }
}

/// Loads the newest matching update on this CPU. `boot_cpu` selects
/// logging: the boot CPU reports what it did, the others only count.
pub fn load(boot_cpu: bool) {
    let Some(file) = FILE.get() else { return };
    if !cpu::is_intel() {
        if boot_cpu {
            radix_klog::warn!("microcode: only Intel processors are supported; module ignored");
        }
        return;
    }
    let id = cpu_id();
    let current = revision();
    if boot_cpu {
        let (count, bad) = radix_microcode::updates(file)
            .fold((0, 0), |(ok, bad), u| if u.is_ok() { (ok + 1, bad) } else { (ok, bad + 1) });
        radix_klog::info!(
            "microcode: module of {} bytes with {count} updates{}; CPU signature {:#x}, platform \
             {:#x}, revision {current:#x}",
            file.len(),
            if bad > 0 { " (and data that failed its checks)" } else { "" },
            id.signature,
            id.platform
        );
    }
    let Some(update) = radix_microcode::find(file, id) else {
        if boot_cpu {
            radix_klog::info!("microcode: no update for this CPU in the module");
        }
        return;
    };
    if update.revision() <= current {
        if boot_cpu {
            radix_klog::info!(
                "microcode: running revision {current:#x} is current (module has {:#x})",
                update.revision()
            );
        }
        return;
    }
    let result = apply(&update);
    if boot_cpu {
        let (year, month, day) = update.date();
        match result {
            Ok(now) => radix_klog::info!(
                "microcode: updated from revision {current:#x} to {now:#x} \
                 (released {year:04}-{month:02}-{day:02})"
            ),
            Err(now) => radix_klog::warn!(
                "microcode: revision {:#x} did not take effect; still running {now:#x}",
                update.revision()
            ),
        }
    } else if result.is_ok() {
        AP_UPDATES.fetch_add(1, Ordering::Relaxed);
    }
}

/// How many other CPUs loaded the update.
pub fn ap_updates() -> u32 {
    AP_UPDATES.load(Ordering::Relaxed)
}
