//! Processor instructions: CPUID, model-specific registers, control
//! registers, interrupts and cache control.

use core::arch::asm;

/// The result of one CPUID leaf.
#[derive(Clone, Copy, Debug)]
pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// Runs CPUID for `leaf` and `subleaf`.
pub fn cpuid(leaf: u32, subleaf: u32) -> CpuidResult {
    let (eax, ebx, ecx, edx): (u32, u32, u32, u32);
    // SAFETY: CPUID only reads processor identification; every x86-64 CPU
    // has it. RBX is reserved by LLVM, so it is saved around the
    // instruction by hand.
    unsafe {
        asm!(
            "mov {tmp:r}, rbx",
            "cpuid",
            "xchg {tmp:r}, rbx",
            tmp = out(reg) ebx,
            inout("eax") leaf => eax,
            inout("ecx") subleaf => ecx,
            out("edx") edx,
            options(nomem, nostack, preserves_flags),
        )
    };
    CpuidResult { eax, ebx, ecx, edx }
}

/// Highest basic CPUID leaf.
pub fn max_leaf() -> u32 {
    cpuid(0, 0).eax
}

/// Highest extended CPUID leaf.
pub fn max_extended_leaf() -> u32 {
    cpuid(0x8000_0000, 0).eax
}

/// The 12-character vendor string, such as `GenuineIntel`.
pub fn vendor() -> [u8; 12] {
    let r = cpuid(0, 0);
    let mut out = [0u8; 12];
    out[0..4].copy_from_slice(&r.ebx.to_le_bytes());
    out[4..8].copy_from_slice(&r.edx.to_le_bytes());
    out[8..12].copy_from_slice(&r.ecx.to_le_bytes());
    out
}

/// Whether this is an Intel processor.
pub fn is_intel() -> bool {
    &vendor() == b"GenuineIntel"
}

/// The processor brand string, if the CPU reports one, trimmed.
pub fn brand(out: &mut [u8; 48]) -> &str {
    if max_extended_leaf() < 0x8000_0004 {
        return "";
    }
    for (i, leaf) in (0x8000_0002..=0x8000_0004).enumerate() {
        let r = cpuid(leaf, 0);
        for (j, word) in [r.eax, r.ebx, r.ecx, r.edx].into_iter().enumerate() {
            out[i * 16 + j * 4..i * 16 + j * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
    let len = out.iter().position(|&b| b == 0).unwrap_or(out.len());
    core::str::from_utf8(&out[..len]).unwrap_or("").trim()
}

/// Family, model and stepping, as displayed (with the extended fields
/// folded in).
pub fn family_model_stepping() -> (u32, u32, u32) {
    let signature = cpuid(1, 0).eax;
    let base_family = (signature >> 8) & 0xf;
    let family =
        if base_family == 0xf { base_family + ((signature >> 20) & 0xff) } else { base_family };
    let base_model = (signature >> 4) & 0xf;
    let model = if base_family == 0x6 || base_family == 0xf {
        base_model | (((signature >> 16) & 0xf) << 4)
    } else {
        base_model
    };
    (family, model, signature & 0xf)
}

/// Feature bits this kernel checks.
pub mod feature {
    use super::cpuid;

    /// x2APIC mode (CPUID 1, ECX bit 21).
    pub fn x2apic() -> bool {
        cpuid(1, 0).ecx & (1 << 21) != 0
    }

    /// No-execute pages (CPUID 0x80000001, EDX bit 20).
    pub fn nx() -> bool {
        super::max_extended_leaf() >= 0x8000_0001 && cpuid(0x8000_0001, 0).edx & (1 << 20) != 0
    }

    /// Global pages (CPUID 1, EDX bit 13).
    pub fn pge() -> bool {
        cpuid(1, 0).edx & (1 << 13) != 0
    }

    /// The page attribute table (CPUID 1, EDX bit 16).
    pub fn pat() -> bool {
        cpuid(1, 0).edx & (1 << 16) != 0
    }

    /// The time-stamp counter runs at a constant rate in all power states
    /// (CPUID 0x80000007, EDX bit 8).
    pub fn invariant_tsc() -> bool {
        super::max_extended_leaf() >= 0x8000_0007 && cpuid(0x8000_0007, 0).edx & (1 << 8) != 0
    }

    /// The `clflush` instruction (CPUID 1, EDX bit 19).
    pub fn clflush() -> bool {
        cpuid(1, 0).edx & (1 << 19) != 0
    }

    /// Running under a hypervisor (CPUID 1, ECX bit 31).
    pub fn hypervisor() -> bool {
        cpuid(1, 0).ecx & (1 << 31) != 0
    }

    /// Structured extended features (CPUID 7, subleaf 0), EBX.
    fn leaf7_ebx() -> u32 {
        if super::max_leaf() >= 7 { cpuid(7, 0).ebx } else { 0 }
    }

    /// Process-context identifiers (CPUID 1, ECX bit 17).
    pub fn pcid() -> bool {
        cpuid(1, 0).ecx & (1 << 17) != 0
    }

    /// Supervisor-mode execution prevention (CPUID 7, EBX bit 7).
    pub fn smep() -> bool {
        leaf7_ebx() & (1 << 7) != 0
    }

    /// Supervisor-mode access prevention (CPUID 7, EBX bit 20).
    pub fn smap() -> bool {
        leaf7_ebx() & (1 << 20) != 0
    }

    /// The `rdrand` instruction (CPUID 1, ECX bit 30).
    pub fn rdrand() -> bool {
        cpuid(1, 0).ecx & (1 << 30) != 0
    }

    /// The `rdseed` instruction (CPUID 7, EBX bit 18).
    pub fn rdseed() -> bool {
        leaf7_ebx() & (1 << 18) != 0
    }

    /// 1 GiB pages (CPUID 0x80000001, EDX bit 26).
    pub fn huge_pages() -> bool {
        super::max_extended_leaf() >= 0x8000_0001 && cpuid(0x8000_0001, 0).edx & (1 << 26) != 0
    }

    /// FXSAVE and FXRSTOR (CPUID 1, EDX bit 24).
    pub fn fxsr() -> bool {
        cpuid(1, 0).edx & (1 << 24) != 0
    }

    /// XSAVE, XRSTOR and XCR0 (CPUID 1, ECX bit 26).
    pub fn xsave() -> bool {
        cpuid(1, 0).ecx & (1 << 26) != 0
    }

    /// XSAVEOPT (CPUID 0xD subleaf 1, EAX bit 0).
    pub fn xsaveopt() -> bool {
        xsave() && super::max_leaf() >= 0xd && cpuid(0xd, 1).eax & 1 != 0
    }

    /// MONITOR and MWAIT (CPUID 1, ECX bit 3).
    pub fn monitor() -> bool {
        cpuid(1, 0).ecx & (1 << 3) != 0
    }

    /// The local APIC timer's TSC-deadline mode (CPUID 1, ECX bit 24).
    pub fn tsc_deadline() -> bool {
        cpuid(1, 0).ecx & (1 << 24) != 0
    }
}

/// A 64-bit word from the CPU's entropy source (`rdseed`), if it has one
/// and it answers within a few tries. RDSEED fails when the source has
/// not refilled yet, so it is retried with a pause.
pub fn rdseed() -> Option<u64> {
    if !feature::rdseed() {
        return None;
    }
    for _ in 0..64 {
        let (value, ok): (u64, u8);
        // SAFETY: CPUID says the instruction exists; it only returns a
        // number and sets the carry flag.
        unsafe {
            asm!("rdseed {}", "setc {}", out(reg) value, out(reg_byte) ok, options(nomem, nostack))
        };
        if ok != 0 {
            return Some(value);
        }
        core::hint::spin_loop();
    }
    None
}

/// A 64-bit word from the CPU's random number generator (`rdrand`), if it
/// has one and it answers within ten tries, as Intel recommends.
pub fn rdrand() -> Option<u64> {
    if !feature::rdrand() {
        return None;
    }
    for _ in 0..10 {
        let (value, ok): (u64, u8);
        // SAFETY: as for `rdseed`.
        unsafe {
            asm!("rdrand {}", "setc {}", out(reg) value, out(reg_byte) ok, options(nomem, nostack))
        };
        if ok != 0 {
            return Some(value);
        }
    }
    None
}

/// Model-specific register numbers.
pub mod msr {
    pub const APIC_BASE: u32 = 0x1b;
    pub const PLATFORM_ID: u32 = 0x17;
    pub const BIOS_UPDT_TRIG: u32 = 0x79;
    pub const BIOS_SIGN_ID: u32 = 0x8b;
    pub const PAT: u32 = 0x277;
    pub const EFER: u32 = 0xc000_0080;
    pub const GS_BASE: u32 = 0xc000_0101;
    pub const TSC_DEADLINE: u32 = 0x6e0;
}

/// Reads a model-specific register.
///
/// # Safety
///
/// `msr` must exist on this processor (reading an unknown MSR raises #GP),
/// and reading it must have no side effect that breaks memory safety.
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: the caller guarantees the MSR exists and may be read.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags))
    };
    (u64::from(high) << 32) | u64::from(low)
}

/// Writes a model-specific register.
///
/// # Safety
///
/// `msr` must exist, and the value must not break the kernel's memory
/// safety (for example by moving GS base away from this CPU's data).
pub unsafe fn wrmsr(msr: u32, value: u64) {
    // SAFETY: the caller guarantees the write is valid and harmless.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack, preserves_flags),
        )
    };
}

/// Reads CR0.
pub fn cr0() -> u64 {
    let value: u64;
    // SAFETY: reading a control register has no side effects.
    unsafe { asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Reads CR2, the address of the last page fault.
pub fn cr2() -> u64 {
    let value: u64;
    // SAFETY: reading a control register has no side effects.
    unsafe { asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Reads CR3, the page-table root.
pub fn cr3() -> u64 {
    let value: u64;
    // SAFETY: reading a control register has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Reads CR4.
pub fn cr4() -> u64 {
    let value: u64;
    // SAFETY: reading a control register has no side effects.
    unsafe { asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Writes CR0.
///
/// # Safety
///
/// The new value must keep protected mode and paging on, and must not make
/// memory the kernel uses inaccessible.
pub unsafe fn write_cr0(value: u64) {
    // SAFETY: the caller guarantees the value is valid.
    unsafe { asm!("mov cr0, {}", in(reg) value, options(nostack, preserves_flags)) };
}

/// Switches to the page tables rooted at physical address `pml4`.
///
/// # Safety
///
/// The tables must map the running code, its stack and every object the
/// kernel references afterwards, at the same virtual addresses.
pub unsafe fn write_cr3(pml4: u64) {
    // SAFETY: the caller guarantees the new tables are complete.
    unsafe { asm!("mov cr3, {}", in(reg) pml4, options(nostack, preserves_flags)) };
}

/// Writes CR4.
///
/// # Safety
///
/// The new value must keep PAE on and must only enable features the
/// processor supports.
pub unsafe fn write_cr4(value: u64) {
    // SAFETY: the caller guarantees the value is valid.
    unsafe { asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags)) };
}

/// Flushes the TLB entry for one page.
pub fn invlpg(addr: usize) {
    // SAFETY: invalidating a TLB entry only costs a later page walk.
    unsafe { asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags)) };
}

/// Flushes the non-global TLB entries of the current address space by
/// reloading CR3.
pub fn flush_tlb() {
    // SAFETY: reloading the current CR3 keeps the same mappings; bit 63
    // (keep the TLB) is never set in the value CR3 reads back.
    unsafe { write_cr3(cr3()) };
}

/// Flushes every TLB entry, global ones and those of every PCID, by
/// toggling CR4.PGE (or reloading CR3 when global pages are off).
pub fn flush_tlb_all() {
    let cr4 = cr4();
    if cr4 & (1 << 7) != 0 {
        // SAFETY: clearing and setting PGE again changes no mapping; the
        // SDM specifies that either write flushes the whole TLB.
        unsafe {
            write_cr4(cr4 & !(1 << 7));
            write_cr4(cr4);
        }
    } else {
        flush_tlb();
    }
}

/// Lets the kernel touch user pages despite SMAP (sets RFLAGS.AC). Only
/// for code that means to, such as the SMAP self-test.
pub fn stac() {
    // SAFETY: setting AC only relaxes SMAP checks for this CPU.
    unsafe { asm!("stac", options(nomem, nostack)) };
}

/// Undoes [`stac`].
pub fn clac() {
    // SAFETY: clearing AC only restores SMAP checks.
    unsafe { asm!("clac", options(nomem, nostack)) };
}

/// Reads the time-stamp counter.
pub fn rdtsc() -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: RDTSC only reads a counter.
    unsafe {
        asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags))
    };
    (u64::from(high) << 32) | u64::from(low)
}

/// Whether interrupts are enabled on this CPU.
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    // SAFETY: reading RFLAGS through the stack has no side effects.
    unsafe { asm!("pushfq", "pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    flags & (1 << 9) != 0
}

/// Enables interrupts.
pub fn enable_interrupts() {
    // SAFETY: the kernel's interrupt handlers are installed before this is
    // first called (`Platform::init` runs first).
    unsafe { asm!("sti", options(nomem, nostack)) };
}

/// Disables interrupts and returns whether they were enabled.
pub fn disable_interrupts() -> bool {
    let were = interrupts_enabled();
    // SAFETY: masking interrupts cannot break memory safety.
    unsafe { asm!("cli", options(nomem, nostack)) };
    were
}

/// Halts until the next interrupt, leaving the interrupt flag as it is.
pub fn halt() {
    // SAFETY: HLT only waits.
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
}

/// Stops this CPU for good.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: `cli; hlt` only stops this CPU.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// The current frame pointer (RBP).
#[inline(always)]
pub fn frame_pointer() -> usize {
    let rbp: usize;
    // SAFETY: reading RBP has no side effects.
    unsafe { asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack, preserves_flags)) };
    rbp
}

/// Writes back and invalidates the cache line containing `addr`.
pub fn clflush(addr: usize) {
    // SAFETY: CLFLUSH on a mapped address only affects caches; callers pass
    // addresses of objects they hold references to.
    unsafe { asm!("clflush [{}]", in(reg) addr, options(nostack, preserves_flags)) };
}

/// Orders all earlier memory accesses before later ones.
pub fn mfence() {
    // SAFETY: a fence has no side effects beyond ordering.
    unsafe { asm!("mfence", options(nostack, preserves_flags)) };
}

/// Waits until every earlier instruction has completed locally.
pub fn lfence() {
    // SAFETY: a fence has no side effects beyond ordering.
    unsafe { asm!("lfence", options(nostack, preserves_flags)) };
}

/// Writes back and invalidates every cache line.
pub fn wbinvd() {
    // SAFETY: WBINVD only flushes caches.
    unsafe { asm!("wbinvd", options(nostack, preserves_flags)) };
}

/// Raises a breakpoint exception (`int3`).
pub fn breakpoint() {
    // SAFETY: the IDT routes #BP to a handler that returns.
    unsafe { asm!("int3", options(nomem, nostack)) };
}

/// Hints to the CPU that it is spinning.
pub fn pause() {
    core::hint::spin_loop();
}

/// Writes extended control register `index`.
///
/// # Safety
///
/// CR4.OSXSAVE must be set, and `value` may only enable state components
/// the processor supports (with x87 always on, and AVX only with SSE).
pub unsafe fn xsetbv(index: u32, value: u64) {
    // SAFETY: the caller guarantees the value is valid.
    unsafe {
        asm!("xsetbv", in("ecx") index, in("eax") value as u32, in("edx") (value >> 32) as u32,
             options(nomem, nostack, preserves_flags))
    };
}

/// Arms address monitoring on the cache line holding `addr`, for
/// [`sti_mwait`].
pub fn monitor(addr: usize) {
    // SAFETY: MONITOR only arms the monitoring hardware; callers check the
    // CPU has it and pass the address of an object they hold.
    unsafe {
        asm!("monitor", in("rax") addr, in("ecx") 0u32, in("edx") 0u32,
             options(nomem, nostack, preserves_flags))
    };
}

/// Enables interrupts and waits in C1 until an interrupt arrives or the
/// monitored line is written. The `sti` takes effect only once MWAIT has
/// started, so an interrupt cannot slip in between and be missed.
pub fn sti_mwait() {
    // SAFETY: MWAIT only waits; the interrupt handlers are installed.
    unsafe { asm!("sti", "mwait", in("eax") 0u32, in("ecx") 0u32, options(nomem, nostack)) };
}

/// Enables interrupts and halts until one arrives, without the window
/// between the two in which an interrupt would be missed.
pub fn sti_hlt() {
    // SAFETY: HLT only waits; the interrupt handlers are installed.
    unsafe { asm!("sti", "hlt", options(nomem, nostack)) };
}
