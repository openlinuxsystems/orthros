// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! scrub(8): reads every block of a Btrfs filesystem and checks it, as
//! Linux's `btrfs scrub start -B` does, with its summary (ADR 0150),
//! through the directory interface (radix-directory(2)).

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use radix_abi::directory::{self, ScrubProgress as P};
use radix_abi::errors::Kernel;

use crate::args::{Arg, Args};
use crate::sys::{self, SYS};
use crate::util::{self, Fail};

/// The exit status of a scrub that found errors no copy mends, as Linux's.
const UNCORRECTABLE: i32 = 3;

/// `bytes` as btrfs-progs prints a size: two decimals, in powers of 1024.
fn size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2}{}", units[unit])
}

/// A UUID as it is written: 8-4-4-4-12 hexadecimal digits.
fn uuid(bytes: &[u8]) -> String {
    let hex = bytes.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// scrub(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let usage = || util::usage("say start, and the path of a Btrfs filesystem".into());
    let Some(Arg::Operand(action)) = args.next()? else { return Err(usage()) };
    if action != "start" {
        return Err(usage());
    }
    let (mut read_only, mut quiet, mut path) = (false, false, None);
    while let Some(arg) = args.next()? {
        match arg {
            // It always waits for the scrub, as -B asks.
            Arg::Short('B') => {}
            Arg::Short('r') => read_only = true,
            Arg::Short('q') => quiet = true,
            Arg::Operand(operand) if path.is_none() => path = Some(PathBuf::from(operand)),
            other => return Err(args.unknown(&other)),
        }
    }
    let Some(path) = path else { return Err(usage()) };
    let dir = File::open(&path).map_err(|e| util::failed(&path.display(), e))?;
    let mut out = io::stdout();
    if !quiet {
        let _ = writeln!(out, "Starting scrub on devid 1");
        let _ = out.flush();
    }
    let started = util::now_ns();
    let mut progress = [0u8; P::SIZE];
    match directory::scrub(&SYS, sys::slot_of(dir.as_raw_fd()), !read_only, &mut progress) {
        Ok(()) => {}
        Err(status) if Kernel::from_status(status) == Some(Kernel::NotSupported) => {
            return Err(util::fail(format!("not a btrfs filesystem: {}", path.display())));
        }
        Err(status) => return Err(util::failed(&path.display(), sys::io_error(status))),
    }
    let seconds = (util::now_ns() - started) / 1_000_000_000;
    let word = |at: usize| u64::from_le_bytes(progress[at..at + 8].try_into().unwrap_or([0; 8]));
    let bytes = word(P::OFFSET_DATA_BYTES) + word(P::OFFSET_TREE_BYTES);
    let corrected = word(P::OFFSET_CORRECTED_ERRORS);
    let uncorrectable = word(P::OFFSET_UNCORRECTABLE_ERRORS);
    if !quiet {
        let fsid = uuid(&progress[P::OFFSET_FSID..P::OFFSET_FSID + 16]);
        let started = util::local_time((started / 1_000_000_000) as i64, "%a %b %e %H:%M:%S %Y");
        let _ = writeln!(out, "scrub done for {fsid}");
        let _ = writeln!(out, "Scrub started:    {started}");
        let _ = writeln!(out, "Status:           finished");
        let duration = format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60);
        let _ = writeln!(out, "Duration:         {duration}");
        let _ = writeln!(out, "Total to scrub:   {}", size(bytes));
        let _ = writeln!(out, "Rate:             {}/s", size(bytes / seconds.max(1)));
        let errors = [
            ("read", word(P::OFFSET_READ_ERRORS)),
            ("super", word(P::OFFSET_SUPER_ERRORS)),
            ("verify", word(P::OFFSET_VERIFY_ERRORS)),
            ("csum", word(P::OFFSET_CSUM_ERRORS)),
        ];
        let mut line = String::from("Error summary:   ");
        if errors.iter().all(|&(_, count)| count == 0) && uncorrectable == 0 {
            line.push_str(" no errors found");
        } else {
            for (name, count) in errors.into_iter().filter(|&(_, count)| count > 0) {
                line.push_str(&format!(" {name}={count}"));
            }
            line.push_str(&format!("\n  Corrected:      {corrected}"));
            line.push_str(&format!("\n  Uncorrectable:  {uncorrectable}"));
            line.push_str(&format!("\n  Unverified:     {}", word(P::OFFSET_UNVERIFIED_ERRORS)));
        }
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
    if uncorrectable > 0 {
        eprintln!("scrub: there are {uncorrectable} uncorrectable errors");
        return Ok(UNCORRECTABLE);
    }
    if corrected > 0 {
        eprintln!("scrub: errors detected during scrubbing, {corrected} corrected");
    }
    Ok(0)
}
