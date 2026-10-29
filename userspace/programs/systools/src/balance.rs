// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! balance(8): moves a Btrfs filesystem's chunks to new ones, converting
//! them to another profile if asked, as Linux's `btrfs balance start`
//! does (ADR 0159), through the directory interface (radix-directory(2)).

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use radix_abi::directory::{self, BalanceKinds as K};
use radix_abi::errors::Kernel;

use crate::args::{Arg, Args};
use crate::sys::{self, SYS};
use crate::util::{self, Fail};

/// What a balance does with one kind of chunk: whether it moves them, the
/// profile it converts them to (0 for none), and whether chunks of that
/// profile stay.
#[derive(Clone, Copy, Default)]
struct Filter {
    on: bool,
    convert: u64,
    soft: bool,
}

/// The block group flag of a profile, by its name; single is Linux's
/// `BTRFS_AVAIL_ALLOC_BIT_SINGLE`.
fn profile(name: &str) -> Option<u64> {
    Some(match name {
        "single" => 1 << 48,
        "raid0" => 1 << 3,
        "raid1" => 1 << 4,
        "dup" => 1 << 5,
        "raid10" => 1 << 6,
        "raid5" => 1 << 7,
        "raid6" => 1 << 8,
        "raid1c3" => 1 << 9,
        "raid1c4" => 1 << 10,
        _ => return None,
    })
}

/// The filter of `-d`, `-m` or `-s` and what is joined to it: `convert=`
/// and `soft`, comma-separated.
fn filter(value: Option<std::ffi::OsString>) -> Result<Filter, Fail> {
    let mut filter = Filter { on: true, ..Filter::default() };
    let Some(value) = value else { return Ok(filter) };
    let text = value.to_string_lossy();
    for part in text.split(',').filter(|p| !p.is_empty()) {
        match part.split_once('=') {
            Some(("convert", name)) => {
                filter.convert = profile(name).ok_or_else(|| {
                    util::usage(format!("unrecognized profile in convert filter: {name}"))
                })?;
            }
            None if part == "soft" => filter.soft = true,
            _ => return Err(util::usage(format!("unrecognized filter: {part}"))),
        }
    }
    if filter.soft && filter.convert == 0 {
        return Err(util::usage("the soft filter needs convert".into()));
    }
    Ok(filter)
}

/// balance(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let usage = || util::usage("say start, any filters and a path".into());
    let Some(action) = args.subcommand() else { return Err(usage()) };
    if action != "start" {
        return Err(usage());
    }
    let (mut data, mut metadata, mut system) = (None, None, None);
    let (mut force, mut full, mut path) = (false, false, None);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('d') => data = Some(filter(args.joined_value())?),
            Arg::Short('m') => metadata = Some(filter(args.joined_value())?),
            Arg::Short('s') => system = Some(filter(args.joined_value())?),
            Arg::Short('f') => force = true,
            Arg::Long(long) if long == "force" => force = true,
            Arg::Long(long) if long == "full-balance" => full = true,
            // It always waits for the balance, as without --background.
            Arg::Short('v') => {}
            Arg::Long(long) if long == "verbose" => {}
            Arg::Operand(operand) if path.is_none() => path = Some(PathBuf::from(operand)),
            other => return Err(args.unknown(&other)),
        }
    }
    let Some(path) = path else { return Err(usage()) };
    if system.is_some() && !force {
        return Err(util::fail(
            "Refusing to explicitly operate on system chunks.\nPass --force if you really want to \
             do that."
                .into(),
        ));
    }
    // System chunks go as metadata does, unless -s says otherwise.
    if system.is_none() {
        system = metadata;
    }
    let (data, metadata, system) = match (data, metadata, system) {
        (None, None, None) => {
            if !full {
                let _ = writeln!(
                    io::stderr(),
                    "WARNING:\n\n\tFull balance without filters requested. This operation is \
                     very\n\tintense and takes potentially very long. It is recommended to\n\tuse \
                     the balance filters to narrow down the scope of balance."
                );
            }
            let every = Filter { on: true, ..Filter::default() };
            (every, every, every)
        }
        (d, m, s) => (d.unwrap_or_default(), m.unwrap_or_default(), s.unwrap_or_default()),
    };
    let mut kinds = K::NONE;
    for (filter, on, soft) in [
        (data, K::DATA, K::SOFT_DATA),
        (metadata, K::METADATA, K::SOFT_METADATA),
        (system, K::SYSTEM, K::SOFT_SYSTEM),
    ] {
        if filter.on {
            kinds = kinds.union(on);
        }
        if filter.soft {
            kinds = kinds.union(soft);
        }
    }
    let shown = path.display().to_string();
    let dir = sys::open_control(&path).map_err(|e| util::failed(&shown, e))?;
    let slot = sys::slot_of(dir.as_raw_fd());
    match directory::balance(&SYS, slot, kinds, data.convert, metadata.convert, system.convert) {
        Ok(done) => {
            let _ = writeln!(
                io::stdout(),
                "Done, had to relocate {} out of {} chunks",
                done.moved,
                done.considered
            );
            Ok(0)
        }
        Err(status) => {
            let why = match Kernel::from_status(status) {
                Some(Kernel::NotSupported) => "Operation not supported".to_string(),
                Some(Kernel::InvalidArgument) => "Invalid argument".to_string(),
                _ => util::strip_os_error(&sys::io_error(status)),
            };
            Err(util::fail(format!(
                "error during balancing '{shown}': {why}\nThere may be more info in syslog - try \
                 dmesg | tail"
            )))
        }
    }
}
