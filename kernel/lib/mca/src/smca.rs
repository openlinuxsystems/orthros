// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! AMD's scalable machine-check architecture (Phase 11e, ADR 0204): the
//! banks of Zen and its successors, each of one block of the chip.
//!
//! Where CPUID 0x80000007's EBX bit 3 says so, each bank has registers of
//! its own beyond the legacy four, from MSR C000_2000h, 16 a bank:
//! MCA_CONFIG, through which the kernel says it reads the new registers
//! (`McaX`); MCA_IPID, whose hardware ID and type name the block the bank
//! belongs to and whose instance ID names which one; MCA_SYND, a
//! syndrome; and MCA_DESTAT and MCA_DEADDR, a second place for a deferred
//! error. The legacy registers stay, so the rest of the crate reads the
//! status, address and miscellaneous register as before. References:
//! AMD's Processor Programming Reference for family 19h and 1Ah, the
//! "Machine Check Architecture" chapter, and the bank types Linux names
//! (`arch/x86/kernel/cpu/mce/amd.c`).
//!
//! What changes in the errors:
//!
//! - Memory errors come from the unified memory controllers' banks, the
//!   channel in their instance ID ([`Info::channel`]), not from the error
//!   code.
//! - A memory controller's address is its own, normalized one, not a
//!   physical address: it names no page to retire until the kernel can
//!   translate it through the data fabric.
//! - Where MCA_CONFIG says so, an address's least significant valid bit
//!   is in the status (bits 29:24), not in the address (bits 61:56).

use core::fmt;

/// CPUID 0x80000007's EBX: the scalable machine-check architecture.
pub const CPUID_SMCA: u32 = 1 << 3;

/// MCA_CONFIG's offset from a bank's first register ([`register`]).
pub const CONFIG: u32 = 4;
/// MCA_IPID's.
pub const IPID: u32 = 5;
/// MCA_SYND's.
pub const SYND: u32 = 6;
/// MCA_DESTAT's.
pub const DESTAT: u32 = 8;
/// MCA_DEADDR's.
pub const DEADDR: u32 = 9;

/// MCA_CONFIG: the kernel reads the new registers. Without it, the CPU
/// treats an uncorrected error as one a legacy kernel cannot handle.
pub const CONFIG_MCAX: u64 = 1 << 32;
/// MCA_CONFIG: the least significant valid bit of an address is in the
/// status.
pub const CONFIG_LSB_IN_STATUS: u64 = 1 << 8;

/// MCA_STATUS: MCA_SYND holds a syndrome.
pub const STATUS_SYND_VALID: u64 = 1 << 53;

/// Bank `bank`'s register at `offset` ([`CONFIG`], [`IPID`]...).
pub fn register(bank: usize, offset: u32) -> u32 {
    0xc000_2000 + 0x10 * bank as u32 + offset
}

/// The MCA_CONFIG to write over `config`: the kernel reads the new
/// registers.
pub fn configure(config: u64) -> u64 {
    config | CONFIG_MCAX
}

/// What the scalable registers say about a bank's error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// MCA_IPID.
    pub ipid: u64,
    /// MCA_SYND, when the status says it holds a syndrome.
    pub synd: Option<u64>,
    /// MCA_CONFIG's [`CONFIG_LSB_IN_STATUS`].
    pub lsb_in_status: bool,
}

/// The block a bank belongs to, which its MCA_IPID's hardware ID (bits
/// 43:32) and type (bits 63:48) name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    /// What it is, as `unified memory controller`.
    pub name: &'static str,
    /// It is a memory controller.
    pub memory: bool,
}

/// The blocks a client or server Zen has, by hardware ID and type, as the
/// Processor Programming Reference lists them.
const BLOCKS: &[(u16, u16, &str)] = &[
    (0xb0, 0x0, "load-store unit"),
    (0xb0, 0x10, "load-store unit"),
    (0xb0, 0x1, "instruction fetch unit"),
    (0xb0, 0x2, "level 2 cache"),
    (0xb0, 0x3, "decode unit"),
    (0xb0, 0x5, "execution unit"),
    (0xb0, 0x6, "floating-point unit"),
    (0xb0, 0x7, "level 3 cache"),
    (0x2e, 0x0, "data fabric's coherent station"),
    (0x2e, 0x1, "data fabric's power, interrupts and others"),
    (0x2e, 0x2, "data fabric's coherent station"),
    (0x96, 0x0, "unified memory controller"),
    (0x96, 0x1, "unified memory controller"),
    (0x05, 0x0, "parameter block"),
    (0xff, 0x0, "platform security processor"),
    (0xff, 0x1, "platform security processor"),
    (0x01, 0x0, "system management unit"),
    (0x01, 0x1, "system management unit"),
    (0x01, 0x2, "microprocessor 5 unit"),
    (0x01, 0x3, "microprocessor DMA unit"),
    (0x18, 0x0, "northbridge I/O unit"),
    (0x46, 0x0, "PCI Express unit"),
    (0x46, 0x1, "PCI Express unit"),
    (0x6c, 0x0, "northbridge interface"),
    (0x80, 0x0, "system hub"),
    (0xa8, 0x0, "SATA unit"),
    (0xaa, 0x0, "USB unit"),
];

/// The hardware ID of the unified memory controllers.
const UMC: u16 = 0x96;

impl Info {
    /// The bank's hardware ID and type.
    pub fn id(&self) -> (u16, u16) {
        (((self.ipid >> 32) & 0xfff) as u16, (self.ipid >> 48) as u16)
    }

    /// The block the bank belongs to, if it is one the kernel knows.
    pub fn block(&self) -> Option<Block> {
        let (hwid, kind) = self.id();
        BLOCKS
            .iter()
            .find(|&&(h, k, _)| h == hwid && k == kind)
            .map(|&(_, _, name)| Block { name, memory: hwid == UMC })
    }

    /// For a memory controller's bank, the channel it serves: the sixth
    /// hexadecimal digit of its instance ID, MCA_IPID's low 32 bits, as
    /// Linux reads it.
    pub fn channel(&self) -> Option<u8> {
        let memory = self.block().is_some_and(|block| block.memory);
        memory.then_some(((self.ipid & 0xffff_ffff) >> 20) as u8)
    }

    /// The least significant valid bit of the address in `address`, from
    /// where MCA_CONFIG says it is.
    pub fn lsb(&self, status: u64, address: u64) -> u64 {
        if self.lsb_in_status { (status >> 24) & 0x3f } else { (address >> 56) & 0x3f }
    }
}

/// An error's extended code, MCA_STATUS's bits 21:16, which says more
/// about it within its block.
pub fn extended_code(status: u64) -> u8 {
    ((status >> 16) & 0x3f) as u8
}

/// The memory controllers' extended codes the kernel names.
fn memory_code(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("a DRAM ECC error"),
        1 => Some("poisoned data written"),
        _ => None,
    }
}

impl Info {
    /// Describes where the error with `status` was, for
    /// [`BankError`](crate::BankError)'s text: `in the unified memory
    /// controller on channel 1, a DRAM ECC error`.
    pub(crate) fn describe(&self, f: &mut fmt::Formatter<'_>, status: u64) -> fmt::Result {
        let code = extended_code(status);
        match self.block() {
            Some(block) => {
                write!(f, " in the {}", block.name)?;
                if let Some(channel) = self.channel() {
                    write!(f, " on channel {channel}")?;
                }
                match memory_code(code).filter(|_| block.memory) {
                    Some(what) => write!(f, ", {what}"),
                    None => write!(f, ", extended code {code}"),
                }
            }
            None => {
                let (hwid, kind) = self.id();
                write!(
                    f,
                    " in a block of hardware ID {hwid:#x}, type {kind:#x}, extended code {code}"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipid(hwid: u64, kind: u64, instance: u64) -> u64 {
        kind << 48 | hwid << 32 | instance
    }

    #[test]
    fn banks_name_their_blocks() {
        let umc = Info { ipid: ipid(0x96, 0, 0x0015_0f00), synd: None, lsb_in_status: false };
        assert_eq!(umc.id(), (0x96, 0));
        assert_eq!(umc.block(), Some(Block { name: "unified memory controller", memory: true }));
        assert_eq!(umc.channel(), Some(1));
        let l3 = Info { ipid: ipid(0xb0, 7, 0x0020_0000), ..umc };
        assert_eq!(l3.block(), Some(Block { name: "level 3 cache", memory: false }));
        assert_eq!(l3.channel(), None);
        let unknown = Info { ipid: ipid(0x259, 0, 0), ..umc };
        assert_eq!(unknown.block(), None);
        assert_eq!(unknown.channel(), None);
    }

    #[test]
    fn registers_and_configuration() {
        assert_eq!(register(0, CONFIG), 0xc000_2004);
        assert_eq!(register(15, IPID), 0xc000_20f5);
        assert_eq!(register(2, DESTAT), 0xc000_2028);
        // Whatever else the firmware set stays.
        assert_eq!(configure(0x0000_0000_0000_0121), 0x0000_0001_0000_0121);
        assert_eq!(extended_code(0x9c00_0000_0001_0000 | 0x0013_0000), 0x13);
        let info = Info { ipid: 0, synd: None, lsb_in_status: true };
        assert_eq!(info.lsb(0x0c00_0000, 0x3f00_0000_0000_0000), 12);
        let info = Info { lsb_in_status: false, ..info };
        assert_eq!(info.lsb(0x0c00_0000, 0x0c00_0000_0000_0000), 12);
    }
}
