// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! lsmem(1): the machine's RAM in memory blocks, from the firmware's
//! memory map in `/kernel/firmware/memmap` (radix-sysfs(5)), laid out as
//! util-linux's lsmem lays it out (ADR 0144). Radix does not take memory
//! on or off line, so every block is online.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::args::{Arg, Args};
use crate::util::{Fail, fail, usage};

/// Where the kernel shows the firmware's memory map.
const MEMMAP: &str = "/kernel/firmware/memmap";

/// The memory map's name for RAM, as Linux's.
const RAM: &str = "System RAM";

/// Linux on x86-64 makes blocks of 128 MiB, or of 2 GiB on a machine
/// with RAM above 64 GiB.
fn block_size(ram: &[(u64, u64)]) -> u64 {
    let end = ram.iter().map(|&(_, last)| last.saturating_add(1)).max().unwrap_or(0);
    if end >= 64 << 30 { 2 << 30 } else { 128 << 20 }
}

/// The RAM ranges in the memory map under `dir`, first and last bytes.
fn ram(dir: &Path) -> io::Result<Vec<(u64, u64)>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let read = |name: &str| fs::read_to_string(path.join(name)).unwrap_or_default();
        if read("type").trim_end() != RAM {
            continue;
        }
        let number = |name: &str| {
            let text = read(name);
            let text = text.trim();
            u64::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16).ok()
        };
        if let (Some(start), Some(end)) = (number("start"), number("end"))
            && start <= end
        {
            found.push((start, end));
        }
    }
    found.sort_unstable();
    Ok(found)
}

/// The blocks of `size` bytes that hold any of `ram`, in order.
fn blocks(ram: &[(u64, u64)], size: u64) -> Vec<u64> {
    let mut blocks: Vec<u64> = Vec::new();
    for &(start, end) in ram {
        for block in start / size..=end / size {
            if blocks.last() != Some(&block) {
                blocks.push(block);
            }
        }
    }
    blocks.dedup();
    blocks
}

/// Runs of consecutive blocks: the first and the last of each, or each
/// block on its own (`-a`).
fn runs(blocks: &[u64], each: bool) -> Vec<(u64, u64)> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for &block in blocks {
        match runs.last_mut() {
            Some((_, last)) if !each && *last + 1 == block => *last = block,
            _ => runs.push((block, block)),
        }
    }
    runs
}

/// `bytes` as util-linux writes sizes with one letter: whole, or with
/// one decimal, in the largest unit of 1024 that leaves at least 1.
fn human(bytes: u64) -> String {
    let units = ["B", "K", "M", "G", "T", "P", "E"];
    let (mut unit, mut scale) = (0, 1u128);
    let bytes = u128::from(bytes);
    while unit + 1 < units.len() && bytes >= scale * 1024 {
        scale *= 1024;
        unit += 1;
    }
    let tenths = (bytes * 10 + scale / 2) / scale;
    if tenths.is_multiple_of(10) {
        format!("{}{}", tenths / 10, units[unit])
    } else {
        format!("{}.{}{}", tenths / 10, tenths % 10, units[unit])
    }
}

/// When the summary is shown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Summary {
    Always,
    Never,
    Only,
}

/// What lsmem prints for `ram`.
fn lines(
    ram: &[(u64, u64)],
    each: bool,
    bytes: bool,
    headings: bool,
    summary: Summary,
) -> Vec<String> {
    let size = block_size(ram);
    let blocks = blocks(ram, size);
    let show = |amount: u64| if bytes { amount.to_string() } else { human(amount) };
    let mut out = Vec::new();
    if summary != Summary::Only {
        let rows: Vec<[String; 4]> = runs(&blocks, each)
            .into_iter()
            .map(|(first, last)| {
                let range = format!("0x{:016x}-0x{:016x}", first * size, (last + 1) * size - 1);
                let block =
                    if first == last { first.to_string() } else { format!("{first}-{last}") };
                [range, show((last - first + 1) * size), "online".to_string(), block]
            })
            .collect();
        let heading = ["RANGE", "SIZE", "STATE", "BLOCK"].map(str::to_string);
        let mut widths = heading.clone().map(|h| h.len());
        for row in &rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.len());
            }
        }
        let format = |row: &[String; 4]| {
            format!(
                "{:<w0$} {:>w1$} {:>w2$} {:>w3$}",
                row[0],
                row[1],
                row[2],
                row[3],
                w0 = widths[0],
                w1 = widths[1],
                w2 = widths[2],
                w3 = widths[3]
            )
        };
        if headings {
            out.push(format(&heading));
        }
        out.extend(rows.iter().map(format));
    }
    if summary != Summary::Never {
        if summary != Summary::Only {
            out.push(String::new());
        }
        let total = blocks.len() as u64 * size;
        let width = if bytes { 15 } else { 5 };
        for (label, amount) in [
            ("Memory block size:", size),
            ("Total online memory:", total),
            ("Total offline memory:", 0),
        ] {
            out.push(format!("{label:<23} {:>width$}", show(amount)));
        }
    }
    out
}

/// lsmem(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let (mut each, mut bytes, mut headings, mut summary) = (false, false, true, Summary::Always);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('a') => each = true,
            Arg::Long(l) if l == "all" => each = true,
            Arg::Short('b') => bytes = true,
            Arg::Long(l) if l == "bytes" => bytes = true,
            Arg::Short('n') => headings = false,
            Arg::Long(l) if l == "noheadings" => headings = false,
            Arg::Long(l) if l == "summary" => {
                summary = match args.optional_value().as_deref().map(|v| v.to_string_lossy()) {
                    None => Summary::Only,
                    Some(when) if when == "only" => Summary::Only,
                    Some(when) if when == "always" => Summary::Always,
                    Some(when) if when == "never" => Summary::Never,
                    Some(when) => {
                        return Err(usage(format!("unsupported --summary argument '{when}'")));
                    }
                };
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let ram = ram(Path::new(MEMMAP))
        .map_err(|error| fail(format!("{MEMMAP}: {}", crate::util::strip_os_error(&error))))?;
    let mut out = io::stdout().lock();
    for line in lines(&ram, each, bytes, headings, summary) {
        writeln!(out, "{line}")?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QEMU's RAM of 1 GiB under OVMF: holes that its blocks cover.
    const ONE_GIB: &[(u64, u64)] = &[
        (0, 0x9_ffff),
        (0x10_0000, 0x7f_ffff),
        (0x80_8000, 0x3eeb_bfff),
        (0x3f00_0000, 0x3fff_ffff),
    ];

    #[test]
    fn blocks_hold_any_ram() {
        assert_eq!(block_size(ONE_GIB), 128 << 20);
        assert_eq!(
            lines(ONE_GIB, false, false, true, Summary::Always),
            [
                "RANGE                                 SIZE  STATE BLOCK",
                "0x0000000000000000-0x000000003fffffff   1G online   0-7",
                "",
                "Memory block size:       128M",
                "Total online memory:       1G",
                "Total offline memory:      0B",
            ]
        );
        // 3 GiB below the 32-bit hole and the rest above 4 GiB, as with
        // -m 8G.
        let split = [(0, 0xbfff_ffff), (0x1_0000_0000, 0x2_3fff_ffff)];
        let out = lines(&split, false, false, false, Summary::Never);
        assert_eq!(
            out,
            [
                "0x0000000000000000-0x00000000bfffffff   3G online  0-23",
                "0x0000000100000000-0x000000023fffffff   5G online 32-71",
            ]
        );
        let each = lines(&split[..1], true, true, false, Summary::Never);
        assert_eq!(each.len(), 24);
        assert_eq!(each[1], "0x0000000008000000-0x000000000fffffff 134217728 online     1");
        let only = lines(&split, false, true, true, Summary::Only);
        assert_eq!(only[1], "Total online memory:         8589934592");
    }

    #[test]
    fn big_machines_have_big_blocks() {
        let framework = [(0, 0x7fff_ffff), (0x1_0000_0000, 0x20_7fff_ffff)];
        assert_eq!(block_size(&framework), 2 << 30);
        assert_eq!(
            lines(&framework, false, false, true, Summary::Only)[0],
            "Memory block size:         2G"
        );
    }

    #[test]
    fn sizes() {
        assert_eq!(human(0), "0B");
        assert_eq!(human(128 << 20), "128M");
        assert_eq!(human(3 << 29), "1.5G");
        assert_eq!(human(1 << 40), "1T");
    }

    #[test]
    fn reads_the_memory_map() {
        let dir = std::env::temp_dir().join(format!("lsmem-test-{}", std::process::id()));
        for (n, start, end, kind) in [
            ("0", "0x0", "0x9ffff", "System RAM"),
            ("1", "0xf0000", "0xfffff", "Reserved"),
            ("10", "0x100000", "0x3fffffff", "System RAM"),
        ] {
            let path = dir.join(n);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("start"), format!("{start}\n")).unwrap();
            fs::write(path.join("end"), format!("{end}\n")).unwrap();
            fs::write(path.join("type"), format!("{kind}\n")).unwrap();
        }
        let found = ram(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, [(0, 0x9_ffff), (0x10_0000, 0x3fff_ffff)]);
    }
}
