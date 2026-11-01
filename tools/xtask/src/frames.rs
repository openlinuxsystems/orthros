// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Holding the kernel to its stack frame budget (ADR 0058).
//!
//! A kernel thread, and a system call on its thread's kernel stack, runs
//! on 16 KiB. One function with a large frame (an array of a few hundred
//! entries is enough) can take a deep call past the guard page, and only
//! the calls that happen to go that deep find out. So the kernel is built
//! with `-Zemit-stack-sizes`, which makes LLVM write every function's
//! address and frame size into a `.stack_sizes` section, and after linking
//! `ob` refuses a kernel in which any function has a frame over [`BUDGET`],
//! but for the few in [`ALLOWED`].

use crate::elf::{Elf, read_u64};
use crate::{Result, bail, ksyms};

/// The largest stack frame a kernel function may have: an eighth of a
/// kernel thread's stack.
pub const BUDGET: u64 = 2048;

/// The functions allowed a larger frame, by name, and why. None of them
/// runs on a kernel thread's 16 KiB stack in a system call.
const ALLOWED: &[(&str, &str)] = &[
    ("radix_kernel_main", "runs once, on the boot CPU's 64 KiB stack"),
    ("radix_arch_entry", "runs once, on the boot CPU's 64 KiB stack"),
    (
        "<radix_arch_x86_64::Arch as radix_hal::Platform>::init",
        "runs once, on the boot CPU's 64 KiB stack",
    ),
    ("radix::user::loader::start", "starts Spark, once, on the boot CPU's 64 KiB stack"),
    ("radix::fs::disk::virtio::bind", "sets up a disk at boot, on the boot CPU's 64 KiB stack"),
    (
        "radix::net::bind",
        "sets up a network card and its stack at boot, on the boot CPU's 64 KiB stack",
    ),
    ("radix::net::bind_card", "sets up a network card at boot, on the boot CPU's 64 KiB stack"),
    ("radix_ena::start", "sets up an ENA card for radix::net::bind_card, at boot"),
    ("radix::net::attach", "builds a network card's stack at boot, on the boot CPU's 64 KiB stack"),
    (
        "radix::net::start_loopback",
        "sets up the loopback interface and its stack at boot, on the boot CPU's 64 KiB stack",
    ),
    (
        "radix::fs::disk::nvme::bind",
        "sets up a controller and its disks at boot, on the boot CPU's 64 KiB stack",
    ),
    (
        "radix::fs::disk::nvme::try_bind",
        "sets up a controller and its disks at boot, also behind a VMD, on the boot CPU's 64 KiB \
         stack",
    ),
    ("__rustc::rust_begin_unwind", "the panic handler: the stack is not used again"),
    ("radix::captest::run", "a boot test, run from the boot CPU's 64 KiB stack"),
    ("radix::schedtest::balance", "a boot test, run from the boot CPU's 64 KiB stack"),
];

/// Reads a ULEB128 number: its value and how many bytes it took.
fn uleb128(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &byte) in bytes.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// The records of a `.stack_sizes` section, as (function address, frame
/// size): each is the address in 8 bytes, then the size in ULEB128.
fn records(data: &[u8]) -> Option<Vec<(u64, u64)>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let address = read_u64(data, at)?;
        let (size, used) = uleb128(data.get(at + 8..)?)?;
        at += 8 + used;
        out.push((address, size));
    }
    Some(out)
}

/// The functions over the budget and not allowed it, largest first, given
/// every function's (address, size, name) and the frame records.
fn over_budget(functions: &[(u64, u64, String)], records: &[(u64, u64)]) -> Vec<(u64, String)> {
    let mut over: Vec<(u64, String)> = records
        .iter()
        .filter(|&&(_, frame)| frame > BUDGET)
        .filter_map(|&(address, frame)| {
            let at = functions.partition_point(|f| f.0 <= address).checked_sub(1)?;
            let (start, _, name) = &functions[at];
            (*start == address).then(|| (frame, name.clone()))
        })
        .filter(|(_, name)| !ALLOWED.iter().any(|(allowed, _)| allowed == name))
        .collect();
    over.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    over
}

/// Checks every function's frame in the linked kernel `bytes`. Returns how
/// many frames it checked.
pub fn check(bytes: &[u8]) -> Result<usize> {
    let Some(elf) = Elf::parse(bytes) else {
        bail!("the kernel is not a 64-bit little-endian ELF file");
    };
    let Some(section) = elf.section(".stack_sizes") else {
        bail!(
            "the kernel has no .stack_sizes section; it must be built with -Zemit-stack-sizes \
             (tools/xtask/src/kernel.rs)"
        );
    };
    let Some(records) = elf.data(section).and_then(records) else {
        bail!("the kernel's .stack_sizes section cannot be read");
    };
    let Some(text) = elf.section(".text") else {
        bail!("the kernel has no .text section");
    };
    let functions = ksyms::functions(&elf, text.addr)?;
    let over = over_budget(&functions, &records);
    if !over.is_empty() {
        let list: Vec<String> =
            over.iter().map(|(frame, name)| format!("  {frame:>6} bytes  {name}")).collect();
        bail!(
            "{} kernel function(s) have a stack frame over the budget of {BUDGET} bytes:\n{}\n\
             Put large arrays on the heap, or build them in place a slot at a time (ADR 0058); \
             code that runs only at boot or in a boot test can be listed in ALLOWED in \
             tools/xtask/src/frames.rs instead",
            over.len(),
            list.join("\n")
        );
    }
    Ok(records.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uleb128_reads_one_and_more_bytes() {
        assert_eq!(uleb128(&[0x08]), Some((8, 1)));
        assert_eq!(uleb128(&[0x80, 0x10]), Some((2048, 2)));
        assert_eq!(uleb128(&[0xe5, 0x8e, 0x26, 0xff]), Some((624_485, 3)));
        assert_eq!(uleb128(&[0x80, 0x80]), None);
    }

    #[test]
    fn records_are_address_then_size() {
        let mut data = Vec::new();
        data.extend_from_slice(&0xffff_ffff_8010_0000u64.to_le_bytes());
        data.push(0x28);
        data.extend_from_slice(&0xffff_ffff_8010_0040u64.to_le_bytes());
        data.extend_from_slice(&[0x88, 0x30]);
        assert_eq!(
            records(&data),
            Some(vec![(0xffff_ffff_8010_0000, 0x28), (0xffff_ffff_8010_0040, 0x1808)])
        );
        // A record cut short.
        assert_eq!(records(&data[..12]), None);
    }

    #[test]
    fn only_unlisted_functions_over_the_budget_are_reported() {
        let functions = vec![
            (0x1000, 0x40, "radix::small".to_string()),
            (0x1040, 0x40, "radix::big".to_string()),
            (0x1080, 0x40, "radix_kernel_main".to_string()),
            (0x10c0, 0x40, "radix::bigger".to_string()),
        ];
        let records = [
            (0x1000, 64),
            (0x1040, BUDGET + 8),
            (0x1080, 16_000),
            (0x10c0, 3 * BUDGET),
            // An address that is no function's start says nothing.
            (0x10c8, 10 * BUDGET),
        ];
        assert_eq!(
            over_budget(&functions, &records),
            vec![(3 * BUDGET, "radix::bigger".to_string()), (BUDGET + 8, "radix::big".to_string())]
        );
    }
}
