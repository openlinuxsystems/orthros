// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Interrupt controllers: the local APIC of each CPU, the I/O APICs and
//! the legacy 8259 PICs.
//!
//! The local APIC runs in x2APIC mode (registers as MSRs) when the CPU
//! supports it, unless the `nox2apic` flag is given, and in xAPIC mode
//! (registers in memory) otherwise. The 8259 PICs are remapped away from
//! the exception vectors and masked, and every I/O APIC input is masked:
//! the kernel takes no device interrupts yet (drivers come with Phase 7),
//! only the local APIC timer and inter-processor interrupts.
//!
//! Under KVM, an EOI written to the APIC leaves the virtual machine, as
//! most APIC writes do. KVM offers a cheaper way, which Linux guests use
//! and so does Radix: paravirtual EOI (`MSR_KVM_PV_EOI_EN` in Linux's
//! `Documentation/virt/kvm/x86/msr.rst`). Each CPU gives the hypervisor
//! the physical address of a word; when the hypervisor delivers an
//! interrupt that needs no EOI write, it sets bit 0, and the handler clears
//! the bit instead of writing the register. The hypervisor may clear the
//! bit itself at any time, so testing and clearing it is one instruction.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use radix_acpi::{Madt, MadtEntry};
use radix_sync::{Once, SpinLock};

use crate::cpu::{self, msr};
use crate::interrupts::{VECTOR_APIC_ERROR, VECTOR_PIC_BASE, VECTOR_SPURIOUS};
use crate::io::{io_delay, outb};
use crate::paging::{self, Access, Cache};
use crate::percpu;

/// Register offsets, as in the xAPIC memory layout. In x2APIC mode the MSR
/// is `0x800 + offset / 16`.
mod reg {
    pub const ID: u32 = 0x20;
    pub const VERSION: u32 = 0x30;
    pub const TPR: u32 = 0x80;
    pub const EOI: u32 = 0xb0;
    pub const SVR: u32 = 0xf0;
    pub const ESR: u32 = 0x280;
    pub const ICR_LOW: u32 = 0x300;
    pub const ICR_HIGH: u32 = 0x310;
    pub const LVT_TIMER: u32 = 0x320;
    pub const LVT_LINT0: u32 = 0x350;
    pub const LVT_LINT1: u32 = 0x360;
    pub const LVT_ERROR: u32 = 0x370;
    pub const TIMER_INITIAL: u32 = 0x380;
    pub const TIMER_CURRENT: u32 = 0x390;
    pub const TIMER_DIVIDE: u32 = 0x3e0;
}

/// IA32_APIC_BASE: x2APIC mode (EXTD).
const BASE_X2APIC: u64 = 1 << 10;
/// IA32_APIC_BASE: global enable (EN).
const BASE_ENABLE: u64 = 1 << 11;
/// LVT mask bit.
const MASKED: u32 = 1 << 16;
/// ICR delivery status (xAPIC only): the previous IPI is still pending.
const ICR_PENDING: u32 = 1 << 12;
/// LVT timer mode: TSC deadline.
const TIMER_TSC_DEADLINE: u32 = 2 << 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    X2Apic,
    /// xAPIC registers mapped at this virtual address.
    XApic(usize),
}

static MODE: Once<Mode> = Once::new();
/// Physical address of the local APIC registers.
static PHYS_BASE: AtomicUsize = AtomicUsize::new(0);
/// Serialises xAPIC ICR writes (the two halves must go together).
static ICR_LOCK: SpinLock<()> = SpinLock::new(());

fn read(offset: u32) -> u32 {
    match MODE.get() {
        Some(Mode::X2Apic) => {
            // SAFETY: x2APIC mode is enabled on this CPU (`enable` switches
            // each CPU as soon as the mode is chosen, before anything uses
            // the APIC), so the MSRs from 0x800 exist; reading them has no
            // side effects.
            unsafe { cpu::rdmsr(0x800 + offset / 16) as u32 }
        }
        Some(&Mode::XApic(base)) => {
            // SAFETY: `base` maps the 4 KiB register page uncached, and the
            // offset is one of the register offsets above.
            unsafe { ((base + offset as usize) as *const u32).read_volatile() }
        }
        None => 0,
    }
}

fn write(offset: u32, value: u32) {
    match MODE.get() {
        Some(Mode::X2Apic) => {
            // SAFETY: as in `read`; writes go to this CPU's own APIC.
            unsafe { cpu::wrmsr(0x800 + offset / 16, u64::from(value)) }
        }
        Some(&Mode::XApic(base)) => {
            // SAFETY: as in `read`.
            unsafe { ((base + offset as usize) as *mut u32).write_volatile(value) }
        }
        None => {}
    }
}

/// The local APIC ID of the running CPU.
pub fn id() -> u32 {
    match MODE.get() {
        Some(Mode::X2Apic) => read(reg::ID),
        Some(Mode::XApic(_)) => read(reg::ID) >> 24,
        None => cpu::cpuid(1, 0).ebx >> 24,
    }
}

/// Whether the local APICs run in x2APIC mode.
pub fn is_x2apic() -> bool {
    MODE.get() == Some(&Mode::X2Apic)
}

/// Short name of the mode, for the boot report.
pub fn mode_name() -> &'static str {
    match MODE.get() {
        Some(Mode::X2Apic) => "x2APIC",
        Some(Mode::XApic(_)) => "xAPIC",
        None => "none",
    }
}

/// Acknowledges the interrupt being handled.
pub fn end_of_interrupt() {
    // A plain load first, so that only a CPU with paravirtual EOI pays for
    // the locked instruction; if the hypervisor clears the bit in between,
    // the instruction finds it clear and the register is written.
    let word = &percpu::current().pv_eoi;
    if word.load(Ordering::Relaxed) & 1 != 0 && word.fetch_and(!1, Ordering::Relaxed) & 1 != 0 {
        return;
    }
    write(reg::EOI, 0);
}

/// KVM's CPUID signature and the feature and MSR of paravirtual EOI.
mod kvm {
    pub const SIGNATURE: [u8; 12] = *b"KVMKVMKVM\0\0\0";
    /// CPUID `base + 1`, EAX: `KVM_FEATURE_PV_EOI`.
    pub const FEATURE_PV_EOI: u32 = 1 << 6;
    pub const MSR_PV_EOI_EN: u32 = 0x4b56_4d04;
}

/// KVM's feature bits (CPUID `base + 1`, EAX), or 0 outside KVM. KVM's
/// leaves start at 0x4000_0000, or at a multiple of 0x100 above it when
/// another hypervisor's come first, so every base is tried, as Linux does.
fn kvm_features() -> u32 {
    static FEATURES: Once<u32> = Once::new();
    *FEATURES.call_once(|| {
        if !cpu::feature::hypervisor() {
            return 0;
        }
        (0x4000_0000..0x4001_0000u32)
            .step_by(0x100)
            .find(|&base| {
                let id = cpu::cpuid(base, 0);
                let mut signature = [0u8; 12];
                signature[..4].copy_from_slice(&id.ebx.to_le_bytes());
                signature[4..8].copy_from_slice(&id.ecx.to_le_bytes());
                signature[8..].copy_from_slice(&id.edx.to_le_bytes());
                signature == kvm::SIGNATURE
            })
            .map_or(0, |base| cpu::cpuid(base + 1, 0).eax)
    })
}

/// Whether the CPUs use paravirtual EOI.
static PV_EOI: AtomicBool = AtomicBool::new(false);

/// Turns on paravirtual EOI for this CPU if KVM offers it.
fn enable_pv_eoi() {
    if kvm_features() & kvm::FEATURE_PV_EOI == 0 {
        return;
    }
    let word = &percpu::current().pv_eoi;
    // The word must be zero when the hypervisor is given it.
    word.store(0, Ordering::Relaxed);
    let Some(phys) = paging::translate(word as *const _ as usize) else { return };
    // SAFETY: KVM advertised the MSR. It takes the physical address of a
    // 4-byte aligned word in RAM, this CPU's own, which lives as long as
    // the kernel and which the hypervisor only changes from this CPU.
    unsafe { cpu::wrmsr(kvm::MSR_PV_EOI_EN, phys | 1) };
    PV_EOI.store(true, Ordering::Relaxed);
}

/// Whether the CPUs acknowledge interrupts through KVM's paravirtual EOI,
/// for the boot report.
pub fn pv_eoi() -> bool {
    PV_EOI.load(Ordering::Relaxed)
}

/// Reads and clears the error status register.
pub fn read_error_status() -> u32 {
    write(reg::ESR, 0);
    read(reg::ESR)
}

/// Chooses the APIC mode and maps the registers. Called once, on the boot
/// CPU, before [`init_cpu`].
pub fn init(madt: Option<&Madt<'_>>, allow_x2apic: bool) {
    // SAFETY: IA32_APIC_BASE exists on every x86-64 CPU.
    let base = unsafe { cpu::rdmsr(msr::APIC_BASE) };
    let phys = madt.map_or(base & 0x000f_ffff_ffff_f000, |madt| madt.local_apic_address);
    PHYS_BASE.store(phys as usize, Ordering::Relaxed);
    // Firmware may hand over in x2APIC mode already. Leaving it needs the
    // APIC disabled first, so the kernel stays in it whatever the flag says.
    let firmware_x2apic = base & BASE_X2APIC != 0;
    if firmware_x2apic && !allow_x2apic {
        radix_klog::warn!("APIC: the firmware enabled x2APIC mode, so nox2apic is ignored");
    }
    let mode = if firmware_x2apic || (allow_x2apic && cpu::feature::x2apic()) {
        Mode::X2Apic
    } else {
        Mode::XApic(paging::map_physical(phys, 4096, Access::ReadWrite, Cache::Uncached))
    };
    let _ = MODE.set(mode);
    // From here on every APIC access uses the chosen mode, so the boot CPU
    // must be in it before anything reads a register (its APIC ID, next).
    enable();
}

/// Switches the running CPU's local APIC on and into the chosen mode. The
/// boot CPU does this in [`init`], the others first thing after they load
/// their IDT; it does nothing when the APIC is already there.
pub fn enable() {
    let x2apic = MODE.get() == Some(&Mode::X2Apic);
    // SAFETY: IA32_APIC_BASE exists on every x86-64 CPU, and these writes
    // keep its base address. x2APIC mode is only chosen when the CPU has
    // it. The SDM forbids going from disabled straight to x2APIC, so the
    // APIC is enabled in xAPIC mode first and switched in a second write.
    unsafe {
        let mut base = cpu::rdmsr(msr::APIC_BASE);
        if base & BASE_ENABLE == 0 {
            base |= BASE_ENABLE;
            cpu::wrmsr(msr::APIC_BASE, base);
        }
        if x2apic && base & BASE_X2APIC == 0 {
            cpu::wrmsr(msr::APIC_BASE, base | BASE_X2APIC);
        }
    }
}

/// The NMI wiring from the MADT: LINT pin and flags for the CPU with ACPI
/// processor UID `uid`, if the table says.
fn nmi_pin(madt: Option<&Madt<'_>>, uid: Option<u32>) -> (u8, u16) {
    madt.into_iter()
        .flat_map(|madt| madt.entries())
        .find_map(|entry| match entry {
            MadtEntry::LocalApicNmi { processor_uid, flags, lint }
                if processor_uid == u32::MAX || Some(processor_uid) == uid =>
            {
                Some((lint, flags))
            }
            _ => None,
        })
        .unwrap_or((1, 0))
}

/// Enables this CPU's local APIC: spurious vector, error vector, LINT pins
/// (NMI on the pin the MADT names) and an open task priority. The timer
/// stays masked until [`crate::time`] starts it.
pub fn init_cpu(madt: Option<&Madt<'_>>, uid: Option<u32>) {
    enable();
    write(reg::SVR, 0x100 | u32::from(VECTOR_SPURIOUS));
    write(reg::TPR, 0);
    write(reg::LVT_TIMER, MASKED);
    write(reg::LVT_ERROR, u32::from(VECTOR_APIC_ERROR));
    let (pin, flags) = nmi_pin(madt, uid);
    // Delivery mode NMI; MPS polarity 3 means active low.
    let nmi = 0x400 | if flags & 0x3 == 0x3 { 1 << 13 } else { 0 };
    write(reg::LVT_LINT0, if pin == 0 { nmi } else { MASKED });
    write(reg::LVT_LINT1, if pin == 1 { nmi } else { MASKED });
    let _ = read_error_status();
    end_of_interrupt();
    enable_pv_eoi();
}

/// Version register: (version, highest LVT entry).
pub fn version() -> (u32, u32) {
    let value = read(reg::VERSION);
    (value & 0xff, (value >> 16) & 0xff)
}

/// Physical address of the registers.
pub fn phys_base() -> u64 {
    PHYS_BASE.load(Ordering::Relaxed) as u64
}

/// Sends an inter-processor interrupt: `low` is the ICR's low word
/// (vector, delivery mode, shorthand), `dest` the target APIC ID.
fn send(dest: u32, low: u32) {
    match MODE.get() {
        Some(Mode::X2Apic) => {
            // Unlike a store to the xAPIC page, WRMSR to the x2APIC ICR is
            // not ordered after earlier stores, which the receiver must see
            // (a new TLB generation, the stop flag): fence first.
            cpu::mfence();
            cpu::lfence();
            // SAFETY: x2APIC mode is on; the ICR MSR takes the destination
            // in its upper half.
            unsafe { cpu::wrmsr(0x830, (u64::from(dest) << 32) | u64::from(low)) };
        }
        Some(Mode::XApic(_)) => {
            // Interrupt handlers send IPIs too (a tick waking a thread on
            // another CPU), so the lock is never held with interrupts on.
            let were_enabled = cpu::disable_interrupts();
            let guard = ICR_LOCK.lock();
            while read(reg::ICR_LOW) & ICR_PENDING != 0 {
                cpu::pause();
            }
            write(reg::ICR_HIGH, dest << 24);
            write(reg::ICR_LOW, low);
            while read(reg::ICR_LOW) & ICR_PENDING != 0 {
                cpu::pause();
            }
            drop(guard);
            if were_enabled {
                cpu::enable_interrupts();
            }
        }
        None => {}
    }
}

/// Sends interrupt `vector` to the CPU with APIC ID `dest`.
pub fn send_fixed(dest: u32, vector: u8) {
    crate::percpu::current().ipis_sent.fetch_add(1, Ordering::Relaxed);
    send(dest, u32::from(vector));
}

/// Sends INIT to `dest`, which resets it into wait-for-SIPI.
pub fn send_init(dest: u32) {
    // Level-triggered assert, then (xAPIC only) de-assert, as the MP
    // specification describes.
    send(dest, 0xc500);
    if !matches!(MODE.get(), Some(Mode::X2Apic)) {
        send(dest, 0x8500);
    }
}

/// Sends a start-up IPI telling `dest` to run real-mode code at
/// `page * 4096`.
pub fn send_startup(dest: u32, page: u8) {
    send(dest, 0x0600 | u32::from(page));
}

/// Sends an NMI to every CPU except this one.
pub fn send_nmi_to_others() {
    // Delivery mode NMI, shorthand "all excluding self".
    send(0, 0x000c_0400);
}

/// Local APIC timer: divide-by-16, masked one-shot from the largest count,
/// for calibration.
pub fn timer_start_calibration() {
    write(reg::TIMER_DIVIDE, 0x3);
    write(reg::LVT_TIMER, MASKED);
    write(reg::TIMER_INITIAL, u32::MAX);
}

/// The timer's current count.
pub fn timer_current() -> u32 {
    read(reg::TIMER_CURRENT)
}

/// Sets the timer up for one-shot interrupts on `vector`, divide-by-16,
/// armed by [`timer_arm`].
pub fn timer_start_one_shot(vector: u8) {
    write(reg::TIMER_DIVIDE, 0x3);
    write(reg::LVT_TIMER, u32::from(vector));
}

/// Arms the one-shot timer to fire after `count` input clocks (divided).
pub fn timer_arm(count: u32) {
    write(reg::TIMER_INITIAL, count.max(1));
}

/// Sets the timer up for TSC-deadline interrupts on `vector`, armed by
/// [`timer_arm_deadline`].
pub fn timer_start_tsc_deadline(vector: u8) {
    write(reg::LVT_TIMER, u32::from(vector) | TIMER_TSC_DEADLINE);
    // The SDM asks for a fence between switching the LVT to TSC-deadline
    // mode and the first write of the deadline MSR.
    cpu::mfence();
}

/// Arms the TSC-deadline timer to fire when the TSC reaches `tsc`.
pub fn timer_arm_deadline(tsc: u64) {
    // SAFETY: the caller set TSC-deadline mode (which CPUID says exists);
    // the MSR only schedules a timer interrupt.
    unsafe { cpu::wrmsr(msr::TSC_DEADLINE, tsc.max(1)) };
}

/// Stops the timer.
pub fn timer_stop() {
    write(reg::LVT_TIMER, MASKED);
    write(reg::TIMER_INITIAL, 0);
}

/// Remaps the 8259 PICs to vectors 0x20-0x2F and masks every input.
pub fn disable_pics() {
    let base = VECTOR_PIC_BASE;
    let steps: [(u16, u8); 10] = [
        (0x20, 0x11),
        (0xa0, 0x11),
        (0x21, base),
        (0xa1, base + 8),
        (0x21, 4),
        (0xa1, 2),
        (0x21, 1),
        (0xa1, 1),
        (0x21, 0xff),
        (0xa1, 0xff),
    ];
    for (port, value) in steps {
        // SAFETY: ports 0x20/0x21 and 0xA0/0xA1 belong to the 8259 PICs;
        // this is the standard initialisation sequence followed by masking
        // every input, which touches no memory.
        unsafe { outb(port, value) };
        io_delay();
    }
}

/// An I/O APIC found in the MADT.
#[derive(Clone, Copy, Debug)]
pub struct IoApic {
    /// Its ID.
    pub id: u8,
    /// Physical address of its registers.
    pub address: u64,
    /// First global system interrupt.
    pub gsi_base: u32,
    /// Number of inputs.
    pub inputs: u32,
    /// Version register.
    pub version: u32,
}

static IOAPICS: SpinLock<([Option<IoApic>; 8], usize)> = SpinLock::new(([None; 8], 0));

/// Masks every input of every I/O APIC in the MADT.
pub fn init_ioapics(madt: &Madt<'_>) {
    let mut list = IOAPICS.lock();
    for entry in madt.entries() {
        let MadtEntry::IoApic { id, address, gsi_base } = entry else { continue };
        let base = paging::map_physical(address, 4096, Access::ReadWrite, Cache::Uncached);
        let select = base as *mut u32;
        let window = (base + 0x10) as *mut u32;
        let read = |index: u32| {
            // SAFETY: `base` maps the I/O APIC's register page uncached;
            // IOREGSEL at offset 0 selects the register IOWIN at 0x10 shows.
            unsafe {
                select.write_volatile(index);
                window.read_volatile()
            }
        };
        let version = read(1);
        let inputs = ((version >> 16) & 0xff) + 1;
        for input in 0..inputs {
            // SAFETY: as above; redirection entry `input` is at register
            // 0x10 + 2 * input, and setting bit 16 masks it.
            unsafe {
                select.write_volatile(0x10 + 2 * input);
                window.write_volatile(read(0x10 + 2 * input) | MASKED);
            }
        }
        let count = list.1;
        if count < list.0.len() {
            list.0[count] = Some(IoApic { id, address, gsi_base, inputs, version: version & 0xff });
            list.1 += 1;
        }
    }
}

/// The I/O APICs found.
pub fn ioapics(mut f: impl FnMut(&IoApic)) {
    let list = IOAPICS.lock();
    list.0.iter().flatten().for_each(&mut f);
}
