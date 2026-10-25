// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! KVM's paravirtual interfaces, as Linux's
//! `Documentation/virt/kvm/x86/cpuid.rst` and `msr.rst` describe them.
//!
//! - Paravirtual EOI, which [`crate::apic`] turns on.
//! - Steal time: each CPU gives the hypervisor the physical address of a
//!   64-byte record ([`StealTime`]), where the hypervisor counts the time
//!   the virtual CPU waited for a real one and, in its `preempted` byte,
//!   says whether the host has stopped the virtual CPU to run something
//!   else.
//! - Paravirtual TLB flush, on top of steal time: a TLB shootdown sends no
//!   IPI to a virtual CPU the host has stopped. It sets the record's
//!   flush bit instead, and the hypervisor flushes that CPU's whole TLB
//!   before it runs it again ([`flush_when_resumed`]). On a busy host a
//!   stopped virtual CPU can wait milliseconds for a real one, and without
//!   this the shootdown would wait with it; Linux guests do the same.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use radix_sync::Once;

use crate::percpu::{self, PerCpu};
use crate::{cpu, paging};

const SIGNATURE: [u8; 12] = *b"KVMKVMKVM\0\0\0";
/// CPUID `base + 1`, EAX: `KVM_FEATURE_STEAL_TIME`.
const FEATURE_STEAL_TIME: u32 = 1 << 5;
/// CPUID `base + 1`, EAX: `KVM_FEATURE_PV_EOI`.
pub const FEATURE_PV_EOI: u32 = 1 << 6;
/// CPUID `base + 1`, EAX: `KVM_FEATURE_PV_TLB_FLUSH`.
const FEATURE_PV_TLB_FLUSH: u32 = 1 << 9;
/// CPUID `base + 1`, EDX: `KVM_HINTS_REALTIME`, the virtual CPUs have real
/// ones to themselves and are never stopped.
const HINT_REALTIME: u32 = 1 << 0;
const MSR_STEAL_TIME: u32 = 0x4b56_4d03;
pub const MSR_PV_EOI_EN: u32 = 0x4b56_4d04;
/// `preempted`: the host has stopped the virtual CPU (`KVM_VCPU_PREEMPTED`).
const PREEMPTED: u8 = 1 << 0;
/// `preempted`: flush the virtual CPU's TLB before it runs again
/// (`KVM_VCPU_FLUSH_TLB`).
const FLUSH_TLB: u8 = 1 << 1;

/// A CPU's steal-time record (`struct kvm_steal_time`), which the
/// hypervisor writes while the CPU is not running.
#[repr(C, align(64))]
pub struct StealTime {
    /// Nanoseconds the virtual CPU was ready to run but did not.
    steal: AtomicU64,
    /// Odd while the hypervisor updates `steal`.
    version: AtomicU32,
    flags: AtomicU32,
    /// [`PREEMPTED`] and [`FLUSH_TLB`].
    preempted: AtomicU8,
    _pad: [AtomicU8; 47],
}

const _: () = assert!(size_of::<StealTime>() == 64);

impl StealTime {
    pub const fn new() -> StealTime {
        StealTime {
            steal: AtomicU64::new(0),
            version: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            preempted: AtomicU8::new(0),
            _pad: [const { AtomicU8::new(0) }; 47],
        }
    }
}

/// KVM's feature bits and hints (CPUID `base + 1`, EAX and EDX), or 0
/// outside KVM. KVM's leaves start at 0x4000_0000, or at a multiple of
/// 0x100 above it when another hypervisor's come first, so every base is
/// tried, as Linux does.
fn leaf() -> (u32, u32) {
    static LEAF: Once<(u32, u32)> = Once::new();
    *LEAF.call_once(|| {
        if !cpu::feature::hypervisor() {
            return (0, 0);
        }
        (0x4000_0000..0x4001_0000u32)
            .step_by(0x100)
            .find(|&base| {
                let id = cpu::cpuid(base, 0);
                let mut signature = [0u8; 12];
                signature[..4].copy_from_slice(&id.ebx.to_le_bytes());
                signature[4..8].copy_from_slice(&id.ecx.to_le_bytes());
                signature[8..].copy_from_slice(&id.edx.to_le_bytes());
                signature == SIGNATURE
            })
            .map_or((0, 0), |base| {
                let features = cpu::cpuid(base + 1, 0);
                (features.eax, features.edx)
            })
    })
}

/// KVM's feature bits, or 0 outside KVM.
pub fn features() -> u32 {
    leaf().0
}

/// Whether TLB shootdowns use the steal-time records.
static PV_TLB_FLUSH: AtomicBool = AtomicBool::new(false);

/// Gives the hypervisor this CPU's steal-time record if KVM offers steal
/// time. The boot CPU, which comes first, also turns on paravirtual TLB
/// flush if KVM offers it: a CPU whose record the hypervisor does not
/// have keeps its `preempted` byte 0, so shootdowns keep sending it IPIs.
pub fn init_cpu() {
    let (features, hints) = leaf();
    if features & FEATURE_STEAL_TIME == 0 {
        return;
    }
    let record = &percpu::current().steal_time;
    let Some(phys) = paging::translate(core::ptr::from_ref(record) as usize) else { return };
    // SAFETY: KVM advertised the MSR. It takes the physical address of a
    // 64-byte aligned record in RAM, plus the enable bit: this CPU's own
    // record, which lives as long as the kernel and whose fields are all
    // atomics, as the hypervisor writes them.
    unsafe { cpu::wrmsr(MSR_STEAL_TIME, phys | 1) };
    if percpu::current_index() == 0 {
        PV_TLB_FLUSH.store(
            features & FEATURE_PV_TLB_FLUSH != 0 && hints & HINT_REALTIME == 0,
            Ordering::Relaxed,
        );
    }
}

/// Whether TLB shootdowns skip virtual CPUs the host has stopped, for the
/// boot report.
pub fn pv_tlb_flush() -> bool {
    PV_TLB_FLUSH.load(Ordering::Relaxed)
}

/// Asks the hypervisor to flush the whole TLB of `cpu` before it runs it
/// again, if the host has stopped it: then the CPU needs no TLB IPI, and
/// this returns true. The page-table changes come first: the exchange
/// that sets the bit is a full barrier on x86, and so is the one with
/// which the hypervisor takes it before it lets the CPU run.
pub fn flush_when_resumed(cpu: &PerCpu) -> bool {
    if !PV_TLB_FLUSH.load(Ordering::Relaxed) {
        return false;
    }
    let state = &cpu.steal_time.preempted;
    let now = state.load(Ordering::Relaxed);
    now & PREEMPTED != 0
        && state.compare_exchange(now, now | FLUSH_TLB, Ordering::AcqRel, Ordering::Relaxed).is_ok()
}
