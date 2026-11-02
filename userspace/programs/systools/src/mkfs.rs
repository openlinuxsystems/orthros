// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! mkfs(8), mkfs-fat(8), mkfs-exfat(8) and fsck-fat(8) (ADR 0302): FAT
//! and exFAT filesystems made and checked by the disk engine, with the
//! options of dosfstools' mkfs.fat and fsck.fat and of exfatprogs'
//! mkfs.exfat, by whose names they run too. mkfs runs the tool of the
//! kind `-t` names.

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::Command;

use disk::{Rng, exfat, fat, fatck};

use crate::args::{Arg, Args};
use crate::blockdev::{self, Opened};
use crate::util::{self, Fail};
use crate::{links, probe, swap};

/// fsck(8)'s exit statuses, which fsck-fat gives as e2fsck does.
const FIXED: i32 = 1;
const LEFT: i32 = 4;
const OPERATIONAL: i32 = 8;
const USAGE: i32 = 16;

/// The kinds mkfs makes itself, and the tool that makes each.
const KINDS: [(&str, &str); 5] = [
    ("fat", "mkfs-fat"),
    ("vfat", "mkfs-fat"),
    ("msdos", "mkfs-fat"),
    ("exfat", "mkfs-exfat"),
    ("swap", "mkswap"),
];

/// mkfs(8): `mkfs -t KIND [OPTION...] DEVICE`, which runs mkfs-KIND with
/// the options after the kind.
pub fn mkfs(args: &mut Args) -> Result<i32, Fail> {
    let items = args.rest();
    let (mut kind, mut rest, mut verbose) = (None, Vec::new(), false);
    let mut items = items.into_iter();
    while let Some(item) = items.next() {
        let text = item.to_string_lossy().into_owned();
        if text == "-t" || text == "--type" {
            kind = Some(items.next().ok_or_else(|| util::usage("-t needs a kind".into()))?);
        } else if let Some(named) = text.strip_prefix("--type=") {
            kind = Some(OsString::from(named));
        } else if let Some(named) = text.strip_prefix("-t").filter(|n| !n.is_empty()) {
            kind = Some(OsString::from(named));
        } else if text == "-V" || text == "--verbose" {
            verbose = true;
        } else if text == "-h" || text == "--help" {
            println!("usage: mkfs -t fat|exfat|swap [OPTION...] DEVICE [SIZE]");
            return Ok(0);
        } else {
            rest.push(item);
        }
    }
    let Some(kind) = kind else {
        return Err(util::usage("say which filesystem with -t: fat, exfat or swap".into()));
    };
    let kind = kind.to_string_lossy().into_owned();
    if let Some((_, tool)) = KINDS.iter().find(|(k, _)| *k == kind) {
        if verbose {
            eprintln!("mkfs: running {tool}");
        }
        let mut args = Args::new(tool, rest);
        let run = match *tool {
            "mkfs-fat" => mkfs_fat,
            "mkfs-exfat" => mkfs_exfat,
            _ => swap::mkswap,
        };
        return run(&mut args).map_err(|fail| {
            if fail.message.is_empty() {
                fail
            } else {
                util::fail_with(fail.code, format!("{tool}: {}", fail.message))
            }
        });
    }
    // A kind another program makes: mkfs-KIND, or mkfs.KIND.
    for program in [format!("mkfs-{kind}"), format!("mkfs.{kind}")] {
        match Command::new(&program).args(&rest).status() {
            Ok(status) => return Ok(status.code().unwrap_or(1)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(util::failed(&program, err)),
        }
    }
    Err(util::fail(format!("cannot make a {kind} filesystem: there is no mkfs-{kind}")))
}

/// Opens the device or file `operand` that a filesystem is made on: to
/// write, and not while something uses it. A whole disk with partitions
/// is refused, unless `whole`, as mkfs.fat's -I has it: the filesystem
/// would take the place of its table.
fn target(
    operand: &OsString,
    sector: Option<u32>,
    whole: bool,
    doing: &str,
) -> Result<Opened, Fail> {
    let opened = blockdev::open(operand, true, sector)?;
    if !opened.writable {
        return Err(util::fail(format!("{} refuses writes", opened.shown)));
    }
    opened.refuse_in_use(doing)?;
    if whole {
        return Ok(opened);
    }
    let partitioned = match &opened.name {
        Some(name) if probe::block_text(name, "partition").is_none() => {
            !blockdev::partitions_of(name).is_empty()
        }
        Some(_) => false,
        None => opened
            .device()
            .ok()
            .and_then(|d| disk::read(&d).ok())
            .is_some_and(|f| f.table.is_some_and(|t| !t.partitions.is_empty())),
    };
    if partitioned {
        return Err(util::fail(format!(
            "{} has partitions; a filesystem on the whole of it would take the table's place \
             (-I makes it anyway)",
            opened.shown
        )));
    }
    Ok(opened)
}

/// Where a partition starts on its disk, in its own sectors: what a FAT
/// or exFAT volume's boot sector calls its hidden sectors.
fn hidden_of(opened: &Opened) -> Option<u64> {
    let name = opened.name.as_ref()?;
    let start: u64 = probe::block_text(name, "start")?.parse().ok()?;
    Some(start * 512 / u64::from(opened.sector))
}

/// The time a reproducible build asks for, SOURCE_DATE_EPOCH, if any.
fn source_date() -> Option<u64> {
    std::env::var("SOURCE_DATE_EPOCH").ok()?.trim().parse().ok()
}

/// A volume ID as mkfs.fat's -i takes it: hexadecimal, of 32 bits.
fn volume_id(text: &str) -> Result<u32, Fail> {
    let digits = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).unwrap_or(text);
    let digits: String = digits.chars().filter(|&c| c != '-').collect();
    u32::from_str_radix(&digits, 16)
        .map_err(|_| util::usage(format!("volume ID must be a hexadecimal number, not '{text}'")))
}

/// mkfs-fat(8), and mkfs.fat and mkfs.vfat.
pub fn mkfs_fat(args: &mut Args) -> Result<i32, Fail> {
    let mut options = fat::Options::default();
    let (mut sector, mut whole, mut verbose, mut create) = (None, false, false, false);
    let mut cluster_sectors: Option<u32> = None;
    let mut operands = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('F') => {
                let bits: u8 = args.number()?;
                if ![12, 16, 32].contains(&bits) {
                    return Err(util::usage(format!("FAT size is 12, 16 or 32, not {bits}")));
                }
                options.bits = Some(bits);
            }
            Arg::Short('n') => options.label = Some(args.text()?),
            Arg::Short('i') => options.volume_id = Some(volume_id(&args.text()?)?),
            Arg::Short('s') => cluster_sectors = Some(args.number()?),
            Arg::Short('S') => sector = Some(args.number()?),
            Arg::Short('f') => options.fats = Some(args.number()?),
            Arg::Short('r') => options.root_entries = Some(args.number()?),
            Arg::Short('h') => options.hidden = Some(args.number()?),
            Arg::Short('I') => whole = true,
            Arg::Short('C') => create = true,
            Arg::Short('v') => verbose = true,
            Arg::Short('c') => {
                return Err(util::fail("-c (a bad block check) is not supported".into()));
            }
            Arg::Long(l) if l == "invariant" => {
                options.volume_id.get_or_insert(0x1234_abcd);
                options.now = Some(source_date().unwrap_or(315_532_800));
            }
            Arg::Long(l) if l == "help" => {
                println!(
                    "usage: mkfs-fat [-F 12|16|32] [-n LABEL] [-i VOLID] [-s SECTORS] [-I] DEVICE [KIB]"
                );
                return Ok(0);
            }
            Arg::Operand(operand) => operands.push(operand),
            other => return Err(args.unknown(&other)),
        }
    }
    if options.now.is_none() {
        options.now = source_date();
    }
    let (Some(path), true) = (operands.first(), operands.len() <= 2) else {
        return Err(util::usage("say the device or file to make the filesystem on".into()));
    };
    let kib = match operands.get(1) {
        Some(blocks) => Some(blocks.to_string_lossy().parse::<u64>().map_err(|_| {
            util::usage(format!("'{}' is not a number of KiB", blocks.to_string_lossy()))
        })?),
        None => None,
    };
    if create {
        let Some(kib) = kib else { return Err(util::usage("-C needs the size, in KiB".into())) };
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path);
        file.and_then(|f| f.set_len(kib * 1024)).map_err(|e| util::failed(&path.display(), e))?;
    }
    let opened = target(path, sector, whole, "a new filesystem on")?;
    if options.hidden.is_none() {
        options.hidden = hidden_of(&opened);
    }
    options.cluster_bytes = cluster_sectors.map(|n| n * opened.sector);
    let mut device = opened.device().map_err(|e| util::failed(&opened.shown, e))?;
    if let Some(kib) = kib.filter(|_| !create) {
        let sectors = kib * 1024 / u64::from(opened.sector);
        if sectors > device.sectors() {
            return Err(util::fail(format!("{} is smaller than {kib} KiB", opened.shown)));
        }
        device = device.part(0, sectors).map_err(|e| util::failed(&opened.shown, e))?;
    }
    let made = fat::format(&device, &options, &mut Rng::system()).map_err(util::fail)?;
    links::refresh_quietly();
    if verbose {
        let mut out = io::stdout().lock();
        writeln!(
            out,
            "{}: {} sectors of {} bytes",
            opened.shown,
            device.sectors(),
            made.sector_size
        )?;
        writeln!(
            out,
            "{} reserved sectors, {} sectors a table, data from sector {}",
            made.reserved,
            made.fat_sectors,
            u64::from(made.reserved) + 2 * u64::from(made.fat_sectors)
        )?;
    }
    println!("mkfs-fat: made {} on {}", made, opened.shown);
    Ok(0)
}

/// A cluster size as mkfs.exfat's -c takes it: bytes, or with K or M.
fn cluster_size(text: &str) -> Result<u32, Fail> {
    let amount = disk::size::parse(text).and_then(|amount| match amount {
        disk::size::Amount::Bytes(bytes) => u32::try_from(bytes).ok(),
        disk::size::Amount::Sectors(_) => None,
    });
    amount.ok_or_else(|| util::usage(format!("'{text}' is not a cluster size")))
}

/// mkfs-exfat(8), and mkfs.exfat.
pub fn mkfs_exfat(args: &mut Args) -> Result<i32, Fail> {
    let mut options = exfat::Options::default();
    let (mut quiet, mut whole) = (false, false);
    let mut operands = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('L') => options.label = Some(args.text()?),
            Arg::Long(l) if l == "volume-label" => options.label = Some(args.text()?),
            Arg::Short('c') => options.cluster_bytes = Some(cluster_size(&args.text()?)?),
            Arg::Long(l) if l == "cluster-size" => {
                options.cluster_bytes = Some(cluster_size(&args.text()?)?);
            }
            Arg::Long(l) if l == "volume-serial" => {
                options.volume_id = Some(volume_id(&args.text()?)?);
            }
            Arg::Long(l) if l == "invariant" => {
                options.volume_id.get_or_insert(0x1234_abcd);
            }
            Arg::Short('I') => whole = true,
            Arg::Short('q') => quiet = true,
            Arg::Long(l) if l == "quiet" => quiet = true,
            Arg::Short('v') => {}
            Arg::Long(l) if l == "verbose" => {}
            Arg::Short('f') => {
                return Err(util::fail("-f (a full format) is not supported".into()));
            }
            Arg::Short('h') => {
                println!("usage: mkfs-exfat [-L LABEL] [-c SIZE] [-q] DEVICE");
                return Ok(0);
            }
            Arg::Operand(operand) => operands.push(operand),
            other => return Err(args.unknown(&other)),
        }
    }
    let [path] = operands.as_slice() else {
        return Err(util::usage("say the one device or file to make the filesystem on".into()));
    };
    let opened = target(path, None, whole, "a new filesystem on")?;
    options.hidden = hidden_of(&opened);
    let device = opened.device().map_err(|e| util::failed(&opened.shown, e))?;
    let made = exfat::format(&device, &options, &mut Rng::system()).map_err(util::fail)?;
    links::refresh_quietly();
    if !quiet {
        println!("mkfs-exfat: made {} on {}", made, opened.shown);
    }
    Ok(0)
}

/// fsck-fat(8), and fsck.fat and fsck.vfat: checks a FAT volume, and
/// mends it with -a. Its exit status is fsck(8)'s: 0 if all is well, 1 if
/// what was wrong is mended, 4 if something wrong is left, 8 if the
/// volume could not be checked, and 16 for a mistake in the command.
pub fn fsck_fat(args: &mut Args) -> Result<i32, Fail> {
    let as_fsck = |fail: Fail| Fail { code: if fail.usage { USAGE } else { fail.code }, ..fail };
    let (mut repair, mut verify, mut verbose) = (false, false, false);
    let mut operands = Vec::new();
    while let Some(arg) = args.next().map_err(as_fsck)? {
        match arg {
            Arg::Short('a' | 'p' | 'y' | 'r') => repair = true,
            Arg::Short('n') => repair = false,
            Arg::Short('V') => verify = true,
            Arg::Short('v') => verbose = true,
            // Changes are written as they are made, as -w asks.
            Arg::Short('w') => {}
            Arg::Long(l) if l == "help" => {
                println!("usage: fsck-fat [-n | -a] [-V] [-v] DEVICE");
                return Ok(0);
            }
            Arg::Operand(operand) => operands.push(operand),
            other => return Err(as_fsck(args.unknown(&other))),
        }
    }
    let [path] = operands.as_slice() else {
        return Err(Fail {
            code: USAGE,
            message: "say the one device or file to check".into(),
            usage: true,
        });
    };
    let opened = blockdev::open(path, repair, None).map_err(|f| Fail { code: OPERATIONAL, ..f })?;
    if repair {
        if !opened.writable {
            return Err(util::fail_with(OPERATIONAL, format!("{} refuses writes", opened.shown)));
        }
        if opened.uses() != 0 {
            let name = opened.name.as_deref().unwrap_or(&opened.shown);
            return Err(util::fail_with(
                OPERATIONAL,
                format!("{}; unmount it before mending it", blockdev::in_use(name, opened.uses())),
            ));
        }
    }
    let device = opened
        .device()
        .map_err(|e| util::fail_with(OPERATIONAL, format!("{}: {e}", opened.shown)))?;
    let report = fatck::check(&device, fatck::Options { repair })
        .map_err(|why| util::fail_with(OPERATIONAL, format!("{}: {why}", opened.shown)))?;
    let mut out = io::stdout().lock();
    if verbose {
        let label = report.label.as_deref().map_or_else(String::new, |l| format!(", label {l}"));
        writeln!(
            out,
            "{}: FAT{}, {} clusters of {} bytes{label}",
            opened.shown, report.bits, report.clusters, report.cluster_bytes
        )?;
    }
    for (problem, mended) in &report.problems {
        match (mended, repair) {
            (true, _) => writeln!(out, "{problem} Mended.")?,
            (false, true) => writeln!(out, "{problem} It cannot be mended.")?,
            (false, false) => writeln!(out, "{problem}")?,
        }
    }
    if !repair && !report.problems.is_empty() {
        writeln!(out, "Nothing was changed; fsck-fat -a mends what can be.")?;
    }
    let mut status = match (report.problems.is_empty(), report.clean()) {
        (true, _) => 0,
        (false, true) => FIXED,
        (false, false) => LEFT,
    };
    if verify && repair && status == FIXED {
        let again = fatck::check(&device, fatck::Options::default())
            .map_err(|why| util::fail_with(OPERATIONAL, format!("{}: {why}", opened.shown)))?;
        for (problem, _) in &again.problems {
            writeln!(out, "after mending, still: {problem}")?;
        }
        if !again.problems.is_empty() {
            status = LEFT;
        }
    }
    writeln!(
        out,
        "{}: {} files, {}/{} clusters",
        opened.shown, report.files, report.used, report.clusters
    )?;
    if repair && !report.problems.is_empty() {
        links::refresh_quietly();
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_ids_and_cluster_sizes() {
        assert_eq!(volume_id("1234abcd").unwrap(), 0x1234_abcd);
        assert_eq!(volume_id("0x1234ABCD").unwrap(), 0x1234_abcd);
        assert_eq!(volume_id("1234-ABCD").unwrap(), 0x1234_abcd);
        assert!(volume_id("xyz").is_err());
        assert_eq!(cluster_size("4K").unwrap(), 4096);
        assert_eq!(cluster_size("131072").unwrap(), 131_072);
        assert_eq!(cluster_size("1M").unwrap(), 1 << 20);
        assert!(cluster_size("8s").is_err());
    }
}
