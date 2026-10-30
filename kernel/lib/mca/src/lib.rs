// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The errors x86-64 machine-check banks report (Phase 11, ADR 0145):
//! what each was, and whether the CPU can go on after it.
//!
//! Each bank of the machine-check architecture holds one error in its
//! IA32_MCi_STATUS register, with the address it concerns in IA32_MCi_ADDR
//! and more about it in IA32_MCi_MISC when the status says those are valid.
//! The status's low 16 bits are an error code that Intel and AMD encode
//! alike: a few simple codes, and compound codes that name the cache, TLB,
//! memory controller or bus and the kind of access. References: Intel 64
//! and IA-32 Architectures Software Developer's Manual, volume 3, chapter
//! 17 ("Interpreting the MCA Error Codes"), and the AMD64 Architecture
//! Programmer's Manual, volume 2, chapter 9.
//!
//! [`triage`] decides what a machine check leaves the kernel to do: go
//! on, end the program that met bad memory, or stop the machine. It
//! follows Linux's rules (`arch/x86/kernel/cpu/mce/severity.c`), cut to
//! what the kernel does about an error.
//!
//! The architecture reads and writes the registers; this crate only reads
//! their values, so its tests run on the host.

#![cfg_attr(not(test), no_std)]

pub mod corrected;

use core::fmt;

/// IA32_MCG_CAP: how many banks there are, in its low 8 bits.
pub const CAP_COUNT: u64 = 0xff;
/// IA32_MCG_CAP: the CPU has IA32_MCG_CTL.
pub const CAP_CTL: u64 = 1 << 8;
/// IA32_MCG_CAP: software error recovery; the status says which
/// uncorrected errors the CPU can go on after.
pub const CAP_RECOVERY: u64 = 1 << 24;

/// IA32_MCG_STATUS: the interrupted code can restart where it was.
pub const RIPV: u64 = 1 << 0;
/// IA32_MCG_STATUS: the instruction the error is about is the one the
/// interrupted code was at.
pub const EIPV: u64 = 1 << 1;
/// IA32_MCG_STATUS: a machine check is in progress; another one now shuts
/// the CPU down.
pub const MCIP: u64 = 1 << 2;

/// IA32_MCi_STATUS: the bank holds an error.
pub const VALID: u64 = 1 << 63;
/// An error came while the bank held another, and was lost.
pub const OVERFLOW: u64 = 1 << 62;
/// The hardware did not correct the error.
pub const UNCORRECTED: u64 = 1 << 61;
/// The bank was set to signal the error.
pub const ENABLED: u64 = 1 << 60;
/// IA32_MCi_MISC holds more about the error.
pub const MISC_VALID: u64 = 1 << 59;
/// IA32_MCi_ADDR holds the address the error concerns.
pub const ADDRESS_VALID: u64 = 1 << 58;
/// The processor's state is corrupt: it cannot go on.
pub const CONTEXT_CORRUPT: u64 = 1 << 57;
/// With software error recovery: the error raised #MC.
pub const SIGNALLED: u64 = 1 << 56;
/// With software error recovery: the interrupted code must not go on
/// until something is done about the error.
pub const ACTION_REQUIRED: u64 = 1 << 55;
/// AMD's MCA_STATUS: the error is poisoned data that the code consumed,
/// and IA32_MCi_ADDR holds its physical address. (Intel counts corrected
/// errors in these bits.)
pub const AMD_POISON: u64 = 1 << 43;
/// AMD's MCA_STATUS: an uncorrected error the hardware deferred, not yet
/// consumed.
pub const AMD_DEFERRED: u64 = 1 << 44;

/// The error codes of a data load and of an instruction fetch that met an
/// uncorrected error in the level 0 cache: the ones Intel's software error
/// recovery requires an action for that ending a program is.
const DATA_LOAD: u16 = 0x0134;
const INSTRUCTION_FETCH: u16 = 0x0150;
/// A page: 2^12 bytes.
const PAGE_SHIFT: u64 = 12;

/// Whether the CPU cannot go on after an error with this status by
/// itself: the processor's state is corrupt, or the error is uncorrected
/// and was signalled, and either the CPU has no software error recovery or
/// the error needs an action (which [`triage`] says whether the kernel can
/// take). `recovery` is IA32_MCG_CAP's [`CAP_RECOVERY`].
pub fn fatal(status: u64, recovery: bool) -> bool {
    status & VALID != 0
        && status & UNCORRECTED != 0
        && (status & CONTEXT_CORRUPT != 0
            || status & ENABLED != 0 && (!recovery || status & ACTION_REQUIRED != 0))
}

/// Whose machine-check architecture a CPU has, and whether it recovers
/// from uncorrected errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mca {
    /// AMD's (or Hygon's), whose status registers have bits of their own;
    /// Intel's otherwise.
    pub amd: bool,
    /// Whether an uncorrected error that leaves the processor's state whole
    /// need not stop the machine: Intel's software error recovery
    /// (IA32_MCG_CAP's [`CAP_RECOVERY`]) or AMD's SUCCOR (CPUID
    /// 0x80000007's EBX bit 1).
    pub recovery: bool,
}

/// What a machine check leaves to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The interrupted code goes on.
    GoOn,
    /// The program that was running in user mode met bad memory and must
    /// not go on; the page it met, by physical address, if the CPU says.
    EndProgram(Option<u64>),
    /// The machine cannot go on.
    Fatal,
}

/// What one error asks for.
enum Asks {
    Nothing,
    EndProgram(Option<u64>),
    Fatal,
}

/// Decides what a machine check leaves to do, from IA32_MCG_STATUS, the
/// CPU's [`Mca`], whether the CPU was in user mode, and the errors its
/// banks hold:
///
/// - A corrupt processor state stops the machine.
/// - With Intel's software error recovery, an error that requires an
///   action ends the program that met it: a data load or instruction
///   fetch in user mode, with the address and more about the error valid
///   and no error lost. Any other that requires an action stops the
///   machine, unless the CPU says the interrupted code can restart and the
///   error was not about it (another thread's). An optional action (SRAO)
///   or an unsignalled error (UCNA) is left for the poll, which retires
///   its page ([`bad_page`]).
/// - With AMD's SUCCOR, an uncorrected error ends the program that met it
///   in user mode, and stops the machine in the kernel.
/// - Without recovery, an uncorrected error in the kernel, or one with an
///   error lost, stops the machine.
/// - Then, if no error asked for more, the code goes on if the CPU says it
///   can restart; if not, the program ends, or in the kernel the machine
///   stops.
pub fn triage(
    mcg_status: u64,
    mca: Mca,
    user: bool,
    errors: impl IntoIterator<Item = BankError>,
) -> Verdict {
    let mut end: Option<Option<u64>> = None;
    for error in errors {
        match asks(&error, mcg_status, mca, user) {
            Asks::Nothing => {}
            Asks::EndProgram(page) => {
                if end.flatten().is_none() {
                    end = Some(page);
                }
            }
            Asks::Fatal => return Verdict::Fatal,
        }
    }
    match end {
        Some(page) => Verdict::EndProgram(page),
        None if mcg_status & RIPV != 0 => Verdict::GoOn,
        None if user => Verdict::EndProgram(None),
        None => Verdict::Fatal,
    }
}

fn asks(error: &BankError, mcg_status: u64, mca: Mca, user: bool) -> Asks {
    let status = error.status;
    if status & VALID == 0 || status & UNCORRECTED == 0 {
        return Asks::Nothing;
    }
    if status & CONTEXT_CORRUPT != 0 {
        return Asks::Fatal;
    }
    match mca {
        Mca { amd: false, recovery: true } => {
            if status & SIGNALLED == 0 {
                // UCNA; one that requires an action is a contradiction.
                return if status & ACTION_REQUIRED != 0 { Asks::Fatal } else { Asks::Nothing };
            }
            if status & ACTION_REQUIRED == 0 {
                return Asks::Nothing;
            }
            if status & OVERFLOW != 0 {
                return Asks::Fatal;
            }
            if mcg_status & (RIPV | EIPV) == RIPV {
                return Asks::Nothing;
            }
            let code = status as u16 & !0x1000;
            let described = status & (ADDRESS_VALID | MISC_VALID) == ADDRESS_VALID | MISC_VALID;
            if user && described && (code == DATA_LOAD || code == INSTRUCTION_FETCH) {
                Asks::EndProgram(bad_page(error, mca))
            } else {
                Asks::Fatal
            }
        }
        Mca { amd: true, recovery: true } if status & ENABLED != 0 => {
            if user {
                Asks::EndProgram(bad_page(error, mca))
            } else {
                Asks::Fatal
            }
        }
        _ if status & ENABLED == 0 => Asks::Nothing,
        _ if !user || status & OVERFLOW != 0 => Asks::Fatal,
        _ => Asks::Nothing,
    }
}

/// The page whose memory went bad, by physical address, that an error
/// names, if it is one the hardware did not correct (or AMD deferred) and
/// the CPU says where it is to within a page: Intel's IA32_MCi_MISC gives
/// the address's kind (physical) and its least significant valid bit;
/// AMD's names poisoned data, and its IA32_MCi_ADDR the bit.
pub fn bad_page(error: &BankError, mca: Mca) -> Option<u64> {
    let status = error.status;
    let bad = status & UNCORRECTED != 0 || mca.amd && status & AMD_DEFERRED != 0;
    if status & VALID == 0 || !bad {
        return None;
    }
    let address = error.address?;
    let (address, lsb) = if mca.amd {
        if status & (AMD_POISON | AMD_DEFERRED) == 0 {
            return None;
        }
        (address & ((1 << 56) - 1), (address >> 56) & 0x3f)
    } else {
        let misc = error.misc?;
        // Bits 8:6, the address mode: 2 is a physical address.
        if (misc >> 6) & 7 != 2 {
            return None;
        }
        (address, misc & 0x3f)
    };
    (lsb <= PAGE_SHIFT).then_some(address & !((1 << PAGE_SHIFT) - 1))
}

/// The error a bank holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BankError {
    /// IA32_MCi_STATUS.
    pub status: u64,
    /// IA32_MCi_ADDR, when the status says it is valid.
    pub address: Option<u64>,
    /// IA32_MCi_MISC, when the status says it is valid.
    pub misc: Option<u64>,
}

impl BankError {
    /// Whether the hardware corrected the error.
    pub fn corrected(&self) -> bool {
        self.status & UNCORRECTED == 0
    }
}

impl fmt::Display for BankError {
    /// For example `corrected data read error in the level 0 data cache,
    /// address 0x12345000 (status 0x9c00000000000134, misc 0x86)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = self.status;
        f.write_str(if self.corrected() { "corrected " } else { "uncorrected " })?;
        describe_code(f, status as u16)?;
        if let Some(address) = self.address {
            write!(f, ", address {address:#x}")?;
        }
        if status & UNCORRECTED != 0 {
            if status & CONTEXT_CORRUPT != 0 {
                f.write_str("; the processor's state is corrupt")?;
            } else if status & ACTION_REQUIRED != 0 {
                f.write_str("; the code that met it must not go on")?;
            } else if status & SIGNALLED != 0 {
                f.write_str("; the CPU went on")?;
            }
        }
        if status & OVERFLOW != 0 {
            f.write_str("; a later error was lost")?;
        }
        write!(f, " (status {status:#018x}")?;
        if let Some(misc) = self.misc {
            write!(f, ", misc {misc:#x}")?;
        }
        f.write_str(")")
    }
}

/// The level and kind of a cache or TLB in a compound code: its `TT` and
/// `LL` fields, followed by `what`.
fn describe_place(f: &mut fmt::Formatter<'_>, tt: u16, ll: u16, what: &str) -> fmt::Result {
    if ll < 3 {
        write!(f, "level {ll} ")?;
    }
    match tt {
        0 => f.write_str("instruction ")?,
        1 => f.write_str("data ")?,
        _ => {}
    }
    f.write_str(what)
}

/// A compound code's `RRRR` field: the kind of access.
fn request(rrrr: u16) -> &'static str {
    match rrrr {
        1 => "read error",
        2 => "write error",
        3 => "data read error",
        4 => "data write error",
        5 => "instruction fetch error",
        6 => "prefetch error",
        7 => "eviction error",
        8 => "snoop error",
        _ => "error",
    }
}

/// Describes an MCA error code. Bit 12, which says whether corrected
/// errors of this kind are filtered, does not change what it is.
fn describe_code(f: &mut fmt::Formatter<'_>, code: u16) -> fmt::Result {
    let c = code & !0x1000;
    let (tt, ll) = ((c >> 2) & 3, c & 3);
    match c {
        0x0000 => f.write_str("error without an error code"),
        0x0001 => f.write_str("unclassified error"),
        0x0002 => f.write_str("microcode ROM parity error"),
        0x0003 => f.write_str("external error (another processor signalled BINIT#)"),
        0x0004 => f.write_str("functional redundancy check error"),
        0x0005 => f.write_str("internal parity error"),
        0x0006 => f.write_str("SMM handler code access violation"),
        0x0400 => f.write_str("internal timer error"),
        // 0000 0000 0000 11LL: a cache, of no kind.
        _ if c & 0xfffc == 0x000c => {
            f.write_str("error in the ")?;
            describe_place(f, 2, ll, "cache")
        }
        // 0000 0000 0001 TTLL.
        _ if c & 0xfff0 == 0x0010 => {
            f.write_str("error in the ")?;
            describe_place(f, tt, ll, "TLB")
        }
        // 0000 0000 1MMM CCCC, or 0000 0100 1MMM CCCC for Intel's
        // extended memory errors.
        _ if c & 0xff80 == 0x0080 || c & 0xff80 == 0x0480 => {
            f.write_str(match (c >> 4) & 7 {
                1 => "memory read error",
                2 => "memory write error",
                3 => "memory address or command error",
                4 => "memory scrubbing error",
                _ => "memory error",
            })?;
            match c & 0xf {
                0xf => Ok(()),
                channel => write!(f, " on channel {channel}"),
            }
        }
        // 0000 01xx xxxx xxxx.
        _ if c & 0xfc00 == 0x0400 => f.write_str("internal unclassified error"),
        // 0000 0001 RRRR TTLL.
        _ if c & 0xff00 == 0x0100 => {
            write!(f, "{} in the ", request((c >> 4) & 0xf))?;
            describe_place(f, tt, ll, "cache")
        }
        // 0000 1PPT RRRR IILL.
        _ if c & 0xf800 == 0x0800 => {
            write!(f, "bus or interconnect {}", request((c >> 4) & 0xf))?;
            f.write_str(match (c >> 2) & 3 {
                0 => " on memory",
                2 => " on I/O",
                _ => "",
            })?;
            if c & 0x0100 != 0 {
                f.write_str(", timed out")?;
            }
            Ok(())
        }
        _ => write!(f, "error with model-specific code {code:#06x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(status: u64, address: Option<u64>, misc: Option<u64>) -> String {
        BankError { status, address, misc }.to_string()
    }

    #[test]
    fn corrected_cache_errors_read_as_the_manual_names_them() {
        assert_eq!(
            text(0x9c00_0000_0000_0134, Some(0x1234_5000), Some(0x86)),
            "corrected data read error in the level 0 data cache, address 0x12345000 \
             (status 0x9c00000000000134, misc 0x86)"
        );
        // Bit 12 (filtering) aside, and a cache of no level.
        assert_eq!(
            text(0x9000_0000_0000_115b, None, None),
            "corrected instruction fetch error in the cache (status 0x900000000000115b)"
        );
        assert_eq!(
            text(0x9000_0000_0000_000e, None, None),
            "corrected error in the level 2 cache (status 0x900000000000000e)"
        );
        assert_eq!(
            text(0x9000_0000_0000_0010, None, None),
            "corrected error in the level 0 instruction TLB (status 0x9000000000000010)"
        );
    }

    #[test]
    fn memory_and_bus_errors() {
        assert_eq!(
            text(0xb600_0000_0000_009f, Some(0x987_6000), None),
            "uncorrected memory read error, address 0x9876000; the processor's state is \
             corrupt (status 0xb60000000000009f)"
        );
        assert_eq!(
            text(0xcc00_0000_0001_00c2, Some(0x4000), Some(0x1)),
            "corrected memory scrubbing error on channel 2, address 0x4000; a later error \
             was lost (status 0xcc000000000100c2, misc 0x1)"
        );
        assert_eq!(
            text(0x9000_0000_0000_0e0b, None, None),
            "corrected bus or interconnect error on I/O (status 0x9000000000000e0b)"
        );
        assert_eq!(
            text(0x9000_0000_0000_0913, None, None),
            "corrected bus or interconnect read error on memory, timed out \
             (status 0x9000000000000913)"
        );
    }

    #[test]
    fn simple_and_model_specific_codes() {
        assert_eq!(
            text(0xb200_0000_0000_0005, None, None),
            "uncorrected internal parity error; the processor's state is corrupt \
             (status 0xb200000000000005)"
        );
        assert_eq!(
            text(0x9000_0000_0000_0401, None, None),
            "corrected internal unclassified error (status 0x9000000000000401)"
        );
        assert_eq!(
            text(0x9000_0000_0000_2000, None, None),
            "corrected error with model-specific code 0x2000 (status 0x9000000000002000)"
        );
    }

    #[test]
    fn which_errors_the_cpu_cannot_go_on_after() {
        // Corrected, or not valid: never.
        assert!(!fatal(0x9c00_0000_0000_0134, true));
        assert!(!fatal(0x3600_0000_0000_009f, false));
        // The processor's state is corrupt: always, even unsignalled.
        assert!(fatal(0xa200_0000_0000_009f, true));
        // Uncorrected and signalled, without software error recovery.
        assert!(fatal(0xb000_0000_0000_009f, false));
        // With it: an action is required (SRAR), or not (SRAO), or the
        // error was not signalled (UCNA).
        assert!(fatal(0xbd80_0000_0000_0134, true));
        assert!(!fatal(0xbd00_0000_0000_00cf, true));
        assert!(!fatal(0xa000_0000_0000_009f, true));
        assert_eq!(
            text(0xbd00_0000_0000_00cf, Some(0x1000), Some(0x86)),
            "uncorrected memory scrubbing error, address 0x1000; the CPU went on \
             (status 0xbd000000000000cf, misc 0x86)"
        );
        assert_eq!(
            text(0xbd80_0000_0000_0134, Some(0x1000), Some(0x86)),
            "uncorrected data read error in the level 0 data cache, address 0x1000; the \
             code that met it must not go on (status 0xbd80000000000134, misc 0x86)"
        );
    }

    const INTEL: Mca = Mca { amd: false, recovery: true };
    const AMD: Mca = Mca { amd: true, recovery: true };

    fn error(status: u64, address: Option<u64>, misc: Option<u64>) -> BankError {
        BankError { status, address, misc }
    }

    /// An SRAR data load at 0x12345678, with a physical address valid to
    /// its cache line.
    const SRAR: u64 = 0xbd80_0000_0000_0134;

    #[test]
    fn a_program_that_read_bad_memory_ends_and_its_page_is_known() {
        let load = error(SRAR, Some(0x1234_5678), Some(0x86));
        let mcg = EIPV | MCIP;
        assert_eq!(triage(mcg, INTEL, true, [load]), Verdict::EndProgram(Some(0x1234_5000)));
        // Fetching an instruction too, with other banks quiet or corrected.
        let fetch = error(SRAR & !0xffff | 0x150, Some(0x8000), Some(0x8c));
        let corrected = error(0x9c00_0000_0000_0134, Some(0x1000), Some(0x86));
        assert_eq!(triage(mcg, INTEL, true, [corrected, fetch]), Verdict::EndProgram(Some(0x8000)));
        // A page it cannot name: not a physical address, or coarser than
        // a page. The program ends all the same.
        let virtual_address = error(SRAR, Some(0x1234_5678), Some(0x46));
        assert_eq!(triage(mcg, INTEL, true, [virtual_address]), Verdict::EndProgram(None));
        let coarse = error(SRAR, Some(0x1234_5678), Some(0x8d));
        assert_eq!(triage(mcg, INTEL, true, [coarse]), Verdict::EndProgram(None));
        assert_eq!(
            triage(mcg, INTEL, true, [coarse, load]),
            Verdict::EndProgram(Some(0x1234_5000))
        );
        // In the kernel, the machine stops.
        assert_eq!(triage(mcg, INTEL, false, [load]), Verdict::Fatal);
    }

    #[test]
    fn an_action_required_the_kernel_cannot_take_stops_the_machine() {
        let mcg = EIPV | MCIP;
        // Another kind of access, an address not given, an error lost.
        for status in [SRAR & !0xffff | 0x9f, SRAR & !ADDRESS_VALID, SRAR | OVERFLOW] {
            assert_eq!(
                triage(mcg, INTEL, true, [error(status, Some(0x1000), Some(0x86))]),
                Verdict::Fatal,
                "{status:#x}"
            );
        }
        // A corrupt processor state, in any mode, with any recovery.
        let corrupt = error(0xb200_0000_0000_0005, None, None);
        for mca in [INTEL, AMD, Mca { amd: false, recovery: false }] {
            assert_eq!(triage(RIPV | MCIP, mca, true, [corrupt]), Verdict::Fatal);
        }
        // UCNA that requires an action is a contradiction.
        let contradiction = error(SRAR & !SIGNALLED, Some(0x1000), Some(0x86));
        assert_eq!(triage(RIPV | MCIP, INTEL, true, [contradiction]), Verdict::Fatal);
    }

    #[test]
    fn errors_that_need_no_action_now_let_the_code_go_on() {
        let load = error(SRAR, Some(0x1234_5678), Some(0x86));
        // Another thread's error: this one can restart, and was not at it.
        assert_eq!(triage(RIPV | MCIP, INTEL, false, [load]), Verdict::GoOn);
        // SRAO (a scrubbing error) and UCNA: the poll retires their page.
        let scrub = error(0xbd00_0000_0000_00cf, Some(0x7_7000), Some(0x86));
        let ucna = error(0xac00_0000_0000_009f, Some(0x9_9123), Some(0x86));
        assert_eq!(triage(RIPV | MCIP, INTEL, false, [scrub, ucna]), Verdict::GoOn);
        assert_eq!(bad_page(&scrub, INTEL), Some(0x7_7000));
        assert_eq!(bad_page(&ucna, INTEL), Some(0x9_9000));
        assert_eq!(bad_page(&error(0x9c00_0000_0000_0134, Some(0x1000), Some(0x86)), INTEL), None);
        // Nothing to say, where the code can restart or not.
        assert_eq!(triage(RIPV | MCIP, INTEL, false, []), Verdict::GoOn);
        assert_eq!(triage(MCIP, INTEL, false, []), Verdict::Fatal);
        assert_eq!(triage(MCIP, INTEL, true, []), Verdict::EndProgram(None));
    }

    #[test]
    fn without_recovery_an_uncorrected_error_stops_the_machine_unless_a_program_can_go_on() {
        let none = Mca { amd: false, recovery: false };
        let uncorrected = error(0xb000_0000_0000_009f, Some(0x1000), Some(0x86));
        assert_eq!(triage(RIPV | MCIP, none, false, [uncorrected]), Verdict::Fatal);
        assert_eq!(triage(RIPV | MCIP, none, true, [uncorrected]), Verdict::GoOn);
        assert_eq!(triage(MCIP, none, true, [uncorrected]), Verdict::EndProgram(None));
        let lost = error(0xf000_0000_0000_009f, None, None);
        assert_eq!(triage(RIPV | MCIP, none, true, [lost]), Verdict::Fatal);
        assert_eq!(bad_page(&uncorrected, none), Some(0x1000));
    }

    #[test]
    fn amd_ends_the_program_that_consumed_poison() {
        // Valid, uncorrected, enabled, address valid, task context
        // corrupt, poison; the address valid from bit 6.
        let status = VALID | UNCORRECTED | ENABLED | ADDRESS_VALID | (1 << 55) | AMD_POISON | 0x135;
        let poison = error(status, Some(6 << 56 | 0x4_5678), None);
        assert_eq!(triage(MCIP, AMD, true, [poison]), Verdict::EndProgram(Some(0x4_5000)));
        assert_eq!(triage(MCIP, AMD, false, [poison]), Verdict::Fatal);
        let unnamed = error(status & !AMD_POISON, Some(0x4_5678), None);
        assert_eq!(triage(MCIP, AMD, true, [unnamed]), Verdict::EndProgram(None));
        assert_eq!(
            triage(MCIP, AMD, true, [error(status | CONTEXT_CORRUPT, None, None)]),
            Verdict::Fatal
        );
        // A deferred error is not consumed yet: the poll retires its page.
        let deferred = error(VALID | ADDRESS_VALID | AMD_DEFERRED | 0x9f, Some(0x8_8000), None);
        assert_eq!(triage(RIPV | MCIP, AMD, false, [deferred]), Verdict::GoOn);
        assert_eq!(bad_page(&deferred, AMD), Some(0x8_8000));
        // Intel counts corrected errors in those bits.
        assert_eq!(bad_page(&error(deferred.status, Some(0x8_8000), Some(0x86)), INTEL), None);
        // Without SUCCOR, as without recovery.
        let older = Mca { amd: true, recovery: false };
        assert_eq!(triage(RIPV | MCIP, older, true, [poison]), Verdict::GoOn);
        assert_eq!(triage(RIPV | MCIP, older, false, [poison]), Verdict::Fatal);
    }
}
