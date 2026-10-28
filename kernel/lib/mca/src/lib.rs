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
//! The architecture reads and writes the registers; this crate only reads
//! their values, so its tests run on the host.

#![cfg_attr(not(test), no_std)]

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

/// Whether the CPU cannot go on after an error with this status: the
/// processor's state is corrupt, or the error is uncorrected and was
/// signalled, and either the CPU has no software error recovery or the
/// error needs an action the kernel does not take yet (such as ending the
/// program that read poisoned memory). `recovery` is IA32_MCG_CAP's
/// [`CAP_RECOVERY`].
pub fn fatal(status: u64, recovery: bool) -> bool {
    status & VALID != 0
        && status & UNCORRECTED != 0
        && (status & CONTEXT_CORRUPT != 0
            || status & ENABLED != 0 && (!recovery || status & ACTION_REQUIRED != 0))
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
}
