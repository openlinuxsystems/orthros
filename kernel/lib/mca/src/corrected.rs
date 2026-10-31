// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Corrected errors (ADR 0169): the interrupts that announce them, the
//! counts the banks keep of them, and the storms of them that turn the
//! interrupts off.
//!
//! - **Intel's CMCI** (IA32_MCG_CAP's [`CAP_CMCI`]): a bank whose
//!   IA32_MCi_CTL2 has CMCI on raises the local APIC's LVT CMCI entry once
//!   it has corrected as many errors as the register's threshold says, and
//!   counts them in its status ([`errors`]). A bank several CPUs share (the
//!   memory controller's, the last-level cache's) has one IA32_MCi_CTL2, so
//!   the first CPU to turn CMCI on in it claims it ([`claim_cmci`]), as in
//!   Linux. The threshold is 1, Linux's: each corrected error interrupts.
//! - **AMD's thresholds**: a bank's MCA_MISC0 can hold a counter of the
//!   errors it corrects, which raises an extended LVT entry of the local
//!   APIC when it reaches its limit ([`arm_thresholds`]). The limit is
//!   Linux's, [`THRESHOLD_LIMIT`]: the counter counts between reads
//!   ([`take_threshold`]) and the interrupt says that it is full.
//! - **Storms** ([`Storm`]): more than [`STORM_INTERRUPTS`] interrupts in
//!   a second, as Linux has it, turn them off; the banks are read every
//!   second instead until [`STORM_QUIET_NS`] go by without a corrected
//!   error.
//! - **Memory errors** ([`memory_error`]) are counted by channel, from the
//!   compound error code of a memory controller.
//!
//! References: Intel's SDM, volume 3, sections 17.3.2.2 (IA32_MCi_CTL2)
//! and 17.5 (CMCI); AMD's BIOS and Kernel Developer's Guides for families
//! 10h to 16h and the Processor Programming References for 17h on
//! (MCA_MISC0); Linux's `arch/x86/kernel/cpu/mce/intel.c` and `amd.c`, as
//! the reference for their use (no code taken).

use crate::{BankError, UNCORRECTED};

/// IA32_MCG_CAP: corrected machine-check interrupts (CMCI).
pub const CAP_CMCI: u64 = 1 << 10;

/// IA32_MC0_CTL2; bank `n`'s is `n` after it.
pub const MC0_CTL2: u32 = 0x280;
/// IA32_MCi_CTL2: CMCI on.
pub const CTL2_CMCI: u64 = 1 << 30;
/// IA32_MCi_CTL2: the corrected errors that raise a CMCI.
pub const CTL2_THRESHOLD: u64 = 0x7fff;

/// IA32_MCi_STATUS, with CMCI: the corrected errors since the bank was
/// cleared, in bits 52:38.
const STATUS_COUNT_SHIFT: u32 = 38;
const STATUS_COUNT: u64 = 0x7fff;

/// MCi_MISC (MCA_MISC0 on Zen, whose legacy registers alias them): bank
/// `n`'s is 4 after bank `n - 1`'s.
pub const MC0_MISC: u32 = 0x403;
/// MCA_MISC0: the register is there.
pub const MISC_VALID: u64 = 1 << 63;
/// MCA_MISC0: it has an error counter.
pub const MISC_COUNTER: u64 = 1 << 62;
/// MCA_MISC0: the firmware locked it.
pub const MISC_LOCKED: u64 = 1 << 61;
/// MCA_MISC0: the counter can interrupt (bank 4's, the northbridge's,
/// always could).
pub const MISC_INTERRUPT: u64 = 1 << 60;
/// MCA_MISC0: which extended LVT entry of the local APIC it raises, as
/// the firmware set it.
const MISC_LVT_SHIFT: u32 = 52;
const MISC_LVT: u64 = 0xf << MISC_LVT_SHIFT;
/// MCA_MISC0: the counter counts.
pub const MISC_COUNTING: u64 = 1 << 51;
/// MCA_MISC0: what reaching the limit raises; 1 is the APIC's interrupt.
const MISC_INTERRUPT_TYPE: u64 = 3 << 49;
const MISC_APIC: u64 = 1 << 49;
/// MCA_MISC0: the counter reached its limit, and stopped.
pub const MISC_FULL: u64 = 1 << 48;
/// MCA_MISC0: the counter, in bits 43:32, which reaching 0xfff fills.
const MISC_COUNT_SHIFT: u32 = 32;
const MISC_COUNT: u64 = 0xfff << MISC_COUNT_SHIFT;
const COUNT_MAX: u64 = 0xfff;

/// The errors an AMD counter counts before it fills and interrupts:
/// Linux's default.
pub const THRESHOLD_LIMIT: u64 = COUNT_MAX;

/// The extended LVT entries an AMD local APIC can have.
const MAX_EXTENDED_LVTS: u8 = 4;

/// More interrupts than this in [`STORM_WINDOW_NS`] make a storm.
pub const STORM_INTERRUPTS: u32 = 15;
/// The window interrupts are counted in.
pub const STORM_WINDOW_NS: u64 = 1_000_000_000;
/// How often the banks are read during a storm.
pub const STORM_READ_NS: u64 = 1_000_000_000;
/// How long the reads must find no corrected error for a storm to end.
pub const STORM_QUIET_NS: u64 = 30_000_000_000;

/// The machine-check registers of the running CPU.
pub trait Msrs {
    /// Reads model-specific register `msr`.
    fn read(&mut self, msr: u32) -> u64;
    /// Writes it.
    fn write(&mut self, msr: u32, value: u64);
}

/// Intel: turns CMCI on, with a threshold of 1, in each bank of `banks`
/// that has it and that no CPU has claimed, and returns the banks this CPU
/// claimed, one bit each. A bank whose IA32_MCi_CTL2 has CMCI on already
/// is another CPU's, or the firmware's, and stays so; one where it does
/// not stay on has no CMCI. Either is only polled.
pub fn claim_cmci(msrs: &mut impl Msrs, banks: core::ops::Range<usize>) -> u32 {
    let mut claimed = 0;
    for bank in banks.take_while(|&bank| bank < 32) {
        let register = MC0_CTL2 + bank as u32;
        let ctl2 = msrs.read(register);
        if ctl2 & CTL2_CMCI != 0 {
            continue;
        }
        msrs.write(register, (ctl2 & !CTL2_THRESHOLD) | CTL2_CMCI | 1);
        if msrs.read(register) & CTL2_CMCI != 0 {
            claimed |= 1 << bank;
        }
    }
    claimed
}

/// Intel: turns CMCI off in the banks of `claimed` again, as
/// [`claim_cmci`] returned them, for another CPU to claim.
pub fn release_cmci(msrs: &mut impl Msrs, claimed: u32) {
    for bank in (0..32).filter(|bank| claimed & (1 << bank) != 0) {
        let register = MC0_CTL2 + bank;
        let ctl2 = msrs.read(register);
        msrs.write(register, ctl2 & !(CTL2_CMCI | CTL2_THRESHOLD));
    }
}

/// How many errors a bank's error stands for: with CMCI, the corrected
/// errors its status counted since the bank was last read (when they
/// overflow the count, at least that many); otherwise one.
pub fn errors(error: &BankError, cmci: bool) -> u64 {
    if !cmci || error.status & UNCORRECTED != 0 {
        return 1;
    }
    ((error.status >> STATUS_COUNT_SHIFT) & STATUS_COUNT).max(1)
}

/// AMD's threshold counters, as [`arm_thresholds`] left them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Thresholds {
    /// The banks whose counter counts, one bit each.
    pub counting: u32,
    /// Those whose counter also interrupts.
    pub interrupting: u32,
    /// The extended LVT entry they raise, the first that the firmware
    /// gave such a counter: the kernel sets it up.
    pub lvt: Option<u8>,
}

/// AMD: starts the error counter of each bank of `banks` whose MCA_MISC0
/// has one the firmware left unlocked, from the count that fills at
/// [`THRESHOLD_LIMIT`] errors, and returns what it did. A counter that can
/// interrupt does, through the extended LVT entry the firmware named for
/// the first such one, if the APIC has `extended_lvts` of them; one that
/// names another only counts, as in Linux.
pub fn arm_thresholds(
    msrs: &mut impl Msrs,
    banks: core::ops::Range<usize>,
    extended_lvts: u8,
) -> Thresholds {
    let mut found = Thresholds::default();
    for bank in banks.take_while(|&bank| bank < 32) {
        let register = MC0_MISC + 4 * bank as u32;
        let misc = msrs.read(register);
        if misc & (MISC_VALID | MISC_COUNTER) != MISC_VALID | MISC_COUNTER
            || misc & MISC_LOCKED != 0
        {
            continue;
        }
        let lvt = ((misc & MISC_LVT) >> MISC_LVT_SHIFT) as u8;
        let can = (misc & MISC_INTERRUPT != 0 || bank == 4)
            && lvt < MAX_EXTENDED_LVTS.min(extended_lvts)
            && *found.lvt.get_or_insert(lvt) == lvt;
        let mut armed = start(misc & !MISC_INTERRUPT_TYPE) | MISC_COUNTING;
        if can {
            armed |= MISC_APIC;
            found.interrupting |= 1 << bank;
        }
        msrs.write(register, armed);
        found.counting |= 1 << bank;
    }
    found
}

/// A counter set to fill at its [`THRESHOLD_LIMIT`]-th error: it counts
/// up and fills when it goes from 0xffe to 0xfff.
fn start(misc: u64) -> u64 {
    (misc & !(MISC_COUNT | MISC_FULL)) | ((COUNT_MAX - THRESHOLD_LIMIT) << MISC_COUNT_SHIFT)
}

/// AMD: the errors bank `bank`'s counter counted since it was armed, and
/// starts it again; 0 for a bank whose counter does not count. Its count
/// stops when it fills, so a full one counted at least
/// [`THRESHOLD_LIMIT`].
pub fn take_threshold(msrs: &mut impl Msrs, bank: usize) -> u64 {
    let register = MC0_MISC + 4 * bank as u32;
    let misc = msrs.read(register);
    if misc & MISC_COUNTING == 0 || misc & MISC_LOCKED != 0 {
        return 0;
    }
    let count = (misc & MISC_COUNT) >> MISC_COUNT_SHIFT;
    msrs.write(register, start(misc));
    count.saturating_sub(COUNT_MAX - THRESHOLD_LIMIT)
}

/// A memory controller's error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryError {
    /// The channel it was on, if the code says (0 to 14).
    pub channel: Option<u8>,
}

/// The memory controller's error a bank holds: on Zen's scalable banks,
/// any error of a memory controller's bank, the channel its instance ID
/// names ([`crate::smca::Info::channel`]); elsewhere, from the status's
/// compound error code ([`memory_error`]).
pub fn memory(error: &BankError) -> Option<MemoryError> {
    match error.smca {
        Some(info) => info.channel().map(|channel| MemoryError { channel: Some(channel) }),
        None => memory_error(error.status),
    }
}

/// The memory controller's error a status holds, from its compound error
/// code (0000 0000 1MMM CCCC, or Intel's extended 0000 0100 1MMM CCCC,
/// with the channel in CCCC; 1111 says none), filtering bit 12 aside.
/// AMD's Zen reports its memory errors in banks of their own instead
/// ([`memory`]).
pub fn memory_error(status: u64) -> Option<MemoryError> {
    let code = status as u16 & !0x1000;
    if code & 0xff80 != 0x0080 && code & 0xff80 != 0x0480 {
        return None;
    }
    let channel = (code & 0xf) as u8;
    Some(MemoryError { channel: (channel != 0xf).then_some(channel) })
}

/// The interrupts for corrected errors, counted to find a storm of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Storm {
    /// When the current window began.
    window: u64,
    /// The interrupts in it.
    interrupts: u32,
    /// During a storm: when a read last found a corrected error.
    last_error: Option<u64>,
}

impl Storm {
    /// No storm, and no interrupt yet.
    pub const fn new() -> Storm {
        Storm { window: 0, interrupts: 0, last_error: None }
    }

    /// Whether a storm is on: the interrupts are off, and the banks are
    /// read every [`STORM_READ_NS`].
    pub fn on(&self) -> bool {
        self.last_error.is_some()
    }

    /// Counts `count` interrupts at `now_ns`; true when they start a
    /// storm: more than [`STORM_INTERRUPTS`] within a window.
    pub fn interrupts(&mut self, now_ns: u64, count: u32) -> bool {
        if self.on() || count == 0 {
            return false;
        }
        if now_ns.saturating_sub(self.window) >= STORM_WINDOW_NS {
            self.window = now_ns;
            self.interrupts = 0;
        }
        self.interrupts = self.interrupts.saturating_add(count);
        if self.interrupts <= STORM_INTERRUPTS {
            return false;
        }
        self.last_error = Some(now_ns);
        true
    }

    /// A read of the banks at `now_ns`, during a storm, that found a
    /// corrected error or not; true when it ends the storm.
    pub fn read(&mut self, now_ns: u64, corrected: bool) -> bool {
        let Some(last) = self.last_error else { return false };
        if corrected {
            self.last_error = Some(now_ns);
            return false;
        }
        if now_ns.saturating_sub(last) < STORM_QUIET_NS {
            return false;
        }
        *self = Storm { window: now_ns, interrupts: 0, last_error: None };
        true
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::vec::Vec;

    use super::*;

    /// A machine's banks as its CPUs see them: each CPU has its own
    /// registers, but for those of the banks shared by all.
    struct Machine {
        shared: Vec<usize>,
        /// Banks whose IA32_MCi_CTL2 keeps CMCI off.
        no_cmci: Vec<usize>,
        registers: HashMap<(usize, u32), u64>,
    }

    struct Cpu<'a> {
        machine: &'a mut Machine,
        cpu: usize,
    }

    impl Machine {
        fn new(shared: &[usize], no_cmci: &[usize]) -> Machine {
            Machine {
                shared: shared.to_vec(),
                no_cmci: no_cmci.to_vec(),
                registers: HashMap::new(),
            }
        }

        fn cpu(&mut self, cpu: usize) -> Cpu<'_> {
            Cpu { machine: self, cpu }
        }

        fn key(&self, cpu: usize, msr: u32) -> (usize, u32) {
            let bank = match msr {
                MC0_CTL2..0x2a0 => (msr - MC0_CTL2) as usize,
                _ => ((msr - MC0_MISC) / 4) as usize,
            };
            (if self.shared.contains(&bank) { usize::MAX } else { cpu }, msr)
        }

        fn get(&self, cpu: usize, msr: u32) -> u64 {
            self.registers.get(&self.key(cpu, msr)).copied().unwrap_or(0)
        }

        fn set(&mut self, cpu: usize, msr: u32, value: u64) {
            let key = self.key(cpu, msr);
            self.registers.insert(key, value);
        }

        /// Bank `bank` corrects an error on `cpu`: its counter counts it, if
        /// it counts and is not full, and fills at 0xfff.
        fn correct(&mut self, cpu: usize, bank: usize) {
            let register = MC0_MISC + 4 * bank as u32;
            let misc = self.get(cpu, register);
            if misc & MISC_COUNTING == 0 || misc & MISC_FULL != 0 {
                return;
            }
            let count = ((misc & MISC_COUNT) >> MISC_COUNT_SHIFT) + 1;
            let full = if count == COUNT_MAX { MISC_FULL } else { 0 };
            self.set(cpu, register, (misc & !MISC_COUNT) | (count << MISC_COUNT_SHIFT) | full);
        }
    }

    impl Msrs for Cpu<'_> {
        fn read(&mut self, msr: u32) -> u64 {
            self.machine.get(self.cpu, msr)
        }

        fn write(&mut self, msr: u32, mut value: u64) {
            let bank = msr.wrapping_sub(MC0_CTL2) as usize;
            if (MC0_CTL2..0x2a0).contains(&msr) && self.machine.no_cmci.contains(&bank) {
                value &= !CTL2_CMCI;
            }
            self.machine.set(self.cpu, msr, value);
        }
    }

    #[test]
    fn the_first_cpu_to_turn_cmci_on_in_a_shared_bank_claims_it() {
        // Banks 0 to 3 are each core's, 4 and 5 the package's; 6 has no
        // CMCI, and the firmware took 7 (CMCI on, threshold 3).
        let mut machine = Machine::new(&[4, 5, 6, 7], &[6]);
        machine.set(0, MC0_CTL2 + 7, CTL2_CMCI | 3);
        machine.set(0, MC0_CTL2 + 2, 0x55 << 16 | 9);
        assert_eq!(claim_cmci(&mut machine.cpu(0), 0..8), 0b11_1111);
        assert_eq!(claim_cmci(&mut machine.cpu(1), 0..8), 0b1111);
        assert_eq!(claim_cmci(&mut machine.cpu(2), 0..4), 0b1111);
        for cpu in 0..3 {
            for bank in 0..6 {
                let ctl2 = machine.get(cpu, MC0_CTL2 + bank);
                assert_eq!(ctl2 & (CTL2_CMCI | CTL2_THRESHOLD), CTL2_CMCI | 1);
            }
        }
        // Other bits stay, and so does what the firmware took.
        assert_eq!(machine.get(0, MC0_CTL2 + 2), 0x55 << 16 | CTL2_CMCI | 1);
        assert_eq!(machine.get(0, MC0_CTL2 + 6), 1);
        assert_eq!(machine.get(1, MC0_CTL2 + 7), CTL2_CMCI | 3);
        // Banks past 31 are left alone.
        assert_eq!(claim_cmci(&mut machine.cpu(3), 30..40), 0b11 << 30);
        // A CPU that lets its banks go leaves the shared ones for another.
        release_cmci(&mut machine.cpu(0), 0b11_1111);
        assert_eq!(machine.get(0, MC0_CTL2 + 2), 0x55 << 16);
        assert_eq!(claim_cmci(&mut machine.cpu(1), 0..8), 0b11_0000);
    }

    #[test]
    fn cmci_counts_corrected_errors_in_the_status() {
        let error = |status| BankError { status, address: None, misc: None, smca: None };
        // 5 errors since the bank was read, a memory read error on channel 1.
        let counted = error(0x9c00_0000_0000_0091 | 5 << STATUS_COUNT_SHIFT);
        assert_eq!(errors(&counted, true), 5);
        assert_eq!(errors(&counted, false), 1);
        // A count of 0 still stands for the error itself.
        assert_eq!(errors(&error(0x9c00_0000_0000_0091), true), 1);
        // An uncorrected error is one, whatever those bits hold (AMD's
        // poison and deferred bits are among them).
        assert_eq!(errors(&error(0xbc00_0000_0000_0091 | 5 << STATUS_COUNT_SHIFT), true), 1);
        assert_eq!(errors(&error(0x9c00_0000_0000_0091 | STATUS_COUNT << 38), true), 0x7fff);
    }

    /// MCA_MISC0 as the firmware leaves it: present, with a counter that
    /// can interrupt through extended LVT entry `lvt`.
    fn misc(lvt: u64) -> u64 {
        MISC_VALID | MISC_COUNTER | MISC_INTERRUPT | lvt << MISC_LVT_SHIFT
    }

    #[test]
    fn amd_counters_count_and_interrupt_through_the_firmwares_lvt_entry() {
        let mut machine = Machine::new(&[4], &[]);
        let at = |bank: u32| MC0_MISC + 4 * bank;
        machine.set(0, at(0), misc(1));
        // No counter; no register; locked.
        machine.set(0, at(1), MISC_VALID);
        machine.set(0, at(2), MISC_COUNTER | MISC_INTERRUPT);
        machine.set(0, at(3), misc(1) | MISC_LOCKED);
        // The northbridge's: its counter interrupts without saying so.
        machine.set(0, at(4), MISC_VALID | MISC_COUNTER | 1 << MISC_LVT_SHIFT);
        // Another LVT entry; one that cannot interrupt; one past the APIC's.
        machine.set(0, at(5), misc(2));
        machine.set(0, at(6), MISC_VALID | MISC_COUNTER | 1 << MISC_LVT_SHIFT);
        machine.set(0, at(7), misc(3) | MISC_APIC | 0x123 << MISC_COUNT_SHIFT | MISC_FULL);
        let found = arm_thresholds(&mut machine.cpu(0), 0..8, 3);
        assert_eq!(
            found,
            Thresholds { counting: 0b1111_0001, interrupting: 0b1_0001, lvt: Some(1) }
        );
        let armed = |lvt: u64, apic: u64| {
            misc(0) & !MISC_INTERRUPT
                | lvt << MISC_LVT_SHIFT
                | MISC_COUNTING
                | apic
                | (COUNT_MAX - THRESHOLD_LIMIT) << MISC_COUNT_SHIFT
        };
        assert_eq!(machine.get(0, at(0)), armed(1, MISC_APIC) | MISC_INTERRUPT);
        assert_eq!(machine.get(0, at(1)), MISC_VALID);
        assert_eq!(machine.get(0, at(3)), misc(1) | MISC_LOCKED);
        assert_eq!(machine.get(0, at(4)), armed(1, MISC_APIC));
        assert_eq!(machine.get(0, at(5)), armed(2, 0) | MISC_INTERRUPT);
        assert_eq!(machine.get(0, at(6)), armed(1, 0));
        // Its count and fullness cleared, and its interrupt off.
        assert_eq!(machine.get(0, at(7)), armed(3, 0) | MISC_INTERRUPT);
        // A CPU whose APIC has no extended LVT entries only counts.
        machine.set(1, at(0), misc(1));
        let found = arm_thresholds(&mut machine.cpu(1), 0..1, 0);
        assert_eq!(found, Thresholds { counting: 1, interrupting: 0, lvt: None });
    }

    #[test]
    fn an_amd_counter_counts_until_it_fills_and_starts_again_when_taken() {
        let mut machine = Machine::new(&[], &[]);
        machine.set(0, MC0_MISC, misc(1));
        arm_thresholds(&mut machine.cpu(0), 0..1, 4);
        assert_eq!(take_threshold(&mut machine.cpu(0), 0), 0);
        for _ in 0..7 {
            machine.correct(0, 0);
        }
        assert_eq!(take_threshold(&mut machine.cpu(0), 0), 7);
        assert_eq!(take_threshold(&mut machine.cpu(0), 0), 0);
        // It fills at the limit, stops counting, and starts again empty.
        for _ in 0..THRESHOLD_LIMIT + 10 {
            machine.correct(0, 0);
        }
        assert_ne!(machine.get(0, MC0_MISC) & MISC_FULL, 0);
        assert_eq!(take_threshold(&mut machine.cpu(0), 0), THRESHOLD_LIMIT);
        assert_eq!(machine.get(0, MC0_MISC) & MISC_FULL, 0);
        machine.correct(0, 0);
        assert_eq!(take_threshold(&mut machine.cpu(0), 0), 1);
        // A counter that was never armed, or is locked, is not touched.
        machine.set(0, MC0_MISC + 4, misc(1) | 5 << MISC_COUNT_SHIFT);
        assert_eq!(take_threshold(&mut machine.cpu(0), 1), 0);
        let locked = misc(1) | MISC_COUNTING | MISC_LOCKED | 5 << MISC_COUNT_SHIFT;
        machine.set(0, MC0_MISC + 8, locked);
        assert_eq!(take_threshold(&mut machine.cpu(0), 2), 0);
        assert_eq!(machine.get(0, MC0_MISC + 4), misc(1) | 5 << MISC_COUNT_SHIFT);
        assert_eq!(machine.get(0, MC0_MISC + 8), locked);
    }

    #[test]
    fn memory_errors_name_their_channel() {
        assert_eq!(memory_error(0x9c00_0000_0000_0091), Some(MemoryError { channel: Some(1) }));
        assert_eq!(memory_error(0x9c00_0000_0000_10a2), Some(MemoryError { channel: Some(2) }));
        assert_eq!(memory_error(0xbc00_0000_0000_048e), Some(MemoryError { channel: Some(14) }));
        assert_eq!(memory_error(0x9c00_0000_0000_009f), Some(MemoryError { channel: None }));
        // A cache's, a bus's on memory, and model-specific codes are not.
        for code in [0x0134, 0x0e0f, 0x0800, 0x0100, 0x0005, 0x0880 | 0x8000] {
            assert_eq!(memory_error(0x9c00_0000_0000_0000 | code), None, "{code:#x}");
        }

        // Zen's are its memory controllers' banks', whatever the code.
        let zen = |status, ipid| BankError {
            status,
            address: None,
            misc: None,
            smca: Some(crate::smca::Info { ipid, synd: None, lsb_in_status: false }),
        };
        let umc = zen(0x9c00_0000_0000_0000, 0x0000_0096_0025_0f00);
        assert_eq!(memory(&umc), Some(MemoryError { channel: Some(2) }));
        assert_eq!(memory(&zen(0x9c00_0000_0000_0091, 0x0000_00b0_0000_0000)), None);
        let legacy = BankError { smca: None, ..umc };
        assert_eq!(
            memory(&BankError { status: 0x9c00_0000_0000_0091, ..legacy }).unwrap().channel,
            Some(1)
        );
    }

    const SECOND: u64 = 1_000_000_000;

    #[test]
    fn more_than_15_interrupts_a_second_start_a_storm_that_30_quiet_seconds_end() {
        let mut storm = Storm::new();
        // 15 in a second are not a storm, nor 15 in each of many.
        assert!(!storm.interrupts(SECOND, 15));
        for second in 2..10 {
            for _ in 0..15 {
                assert!(!storm.interrupts(second * SECOND + 1, 1));
            }
        }
        assert!(!storm.on());
        assert!(!storm.read(10 * SECOND, false));
        // The 16th within one is.
        assert!(!storm.interrupts(20 * SECOND, 10));
        assert!(storm.interrupts(20 * SECOND + SECOND / 2, 6));
        assert!(storm.on());
        // Interrupts that came meanwhile change nothing.
        assert!(!storm.interrupts(21 * SECOND, 100));
        // Errors keep it on; 30 s without one end it.
        assert!(!storm.read(30 * SECOND, true));
        assert!(!storm.read(59 * SECOND, false));
        assert!(storm.read(60 * SECOND, false));
        assert!(!storm.on());
        // Afterwards, counting starts again.
        assert!(!storm.read(61 * SECOND, false));
        assert!(!storm.interrupts(60 * SECOND + 1, 15));
        assert!(storm.interrupts(60 * SECOND + 2, 1));
    }
}
