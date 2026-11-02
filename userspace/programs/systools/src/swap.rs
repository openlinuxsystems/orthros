// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! mkswap(8), swapon(8) and swapoff(8), with util-linux's options and
//! messages (ADR 0152): mkswap writes the header of a Linux swap area, and
//! swapon and swapoff turn one on and off with radix-block(2)'s `SWAP_ON`
//! and `SWAP_OFF` on its device, which `/processes/swaps` then lists.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;

use radix_abi::errors::Kernel;
use radix_abi::{Status, block, file};

use crate::args::{Arg, Args};
use crate::sys::{self, SYS};
use crate::util::{self, Fail};

/// A page, the only size of page an area is made for.
const PAGE: usize = 4096;

/// What ends a swap area's first page, and where its fields are in it.
const MAGIC: &[u8; 10] = b"SWAPSPACE2";
const VERSION_AT: usize = 1024;
const UUID_AT: usize = VERSION_AT + 12;
const LABEL_AT: usize = UUID_AT + 16;

/// The fewest pages an area may have, as util-linux's
/// `MIN_SWAPSPACE_SIZE`.
const MIN_PAGES: u64 = 10;

/// Where Btrfs keeps the copies of its superblock, and where the magic is
/// in each: mkswap clears them, as util-linux's wipes every signature.
const BTRFS_COPIES: [u64; 3] = [64 << 10, 64 << 20, 256 << 30];
const BTRFS_MAGIC: &[u8; 8] = b"_BHRfS_M";
const BTRFS_MAGIC_AT: u64 = 0x40;

/// The areas on, as the kernel lists them.
const SWAPS: &str = "/processes/swaps";

/// The size of the device or file `file`, in bytes.
fn size_of(file: &File) -> io::Result<u64> {
    let meta = file.metadata()?;
    if !meta.file_type().is_block_device() {
        return Ok(meta.len());
    }
    let mut bytes = [0u8; 8];
    control(file, block::GET_SIZE_BYTES, &mut bytes).map_err(sys::io_error)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Block request `request` on `device`, with `value` as its argument.
fn control(device: &File, request: u32, value: &mut [u8]) -> Result<(), Status> {
    let address = value.as_mut_ptr() as u64;
    let slot = sys::slot_of(device.as_raw_fd());
    file::control(&SYS, slot, request, address, address, value.len() as u64).map(drop)
}

/// A UUID as it is written: 8-4-4-4-12 hexadecimal digits.
fn uuid_text(bytes: &[u8]) -> String {
    let mut text = String::new();
    for (at, byte) in bytes.iter().enumerate() {
        if matches!(at, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The UUID written `text`, as 16 bytes.
fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let digits: Vec<u8> = text.bytes().filter(|&b| b != b'-').collect();
    let dashes: Vec<usize> = text.match_indices('-').map(|(at, _)| at).collect();
    if digits.len() != 32 || (dashes != [8, 13, 18, 23]) {
        return None;
    }
    let mut uuid = [0u8; 16];
    for (at, pair) in digits.chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        uuid[at] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(uuid)
}

/// A random UUID, of version 4.
fn random_uuid() -> io::Result<[u8; 16]> {
    let mut uuid = [0u8; 16];
    File::open("/devices/urandom")?.read_exact(&mut uuid)?;
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    Ok(uuid)
}

/// `bytes` as util-linux's `size_to_human_string` writes them: in the
/// largest unit of 1024 under which they are, with one decimal if it is
/// not 0, and the unit as one letter (`K`) or three (`KiB`).
fn human(bytes: u64, three: bool) -> String {
    let mut exp = 0;
    while exp < 60 && bytes >= 1u64 << (exp + 10) {
        exp += 10;
    }
    let letter = ["B", "K", "M", "G", "T", "P", "E"][exp / 10];
    let suffix = if three && exp > 0 { format!("{letter}iB") } else { letter.to_string() };
    let mut whole = bytes >> exp;
    let mut tenth = 0;
    if exp > 0 {
        let frac = bytes & ((1u64 << exp) - 1);
        tenth = ((frac >> (exp - 10)) + 50) / 100;
        if tenth == 10 {
            whole += 1;
            tenth = 0;
        }
    }
    let space = if three { " " } else { "" };
    if tenth > 0 {
        format!("{whole}.{tenth}{space}{suffix}")
    } else {
        format!("{whole}{space}{suffix}")
    }
}

/// Whether `page`, a disk's first, holds a partition table that mkswap
/// keeps: GPT's header in its second sector, or an MBR that is no boot
/// sector of a filesystem (which starts with a jump).
fn partition_table(page: &[u8]) -> Option<&'static str> {
    if &page[512..520] == b"EFI PART" {
        return Some("gpt");
    }
    let mbr = page[510] == 0x55 && page[511] == 0xaa && !matches!(page[0], 0xeb | 0xe9);
    mbr.then_some("dos")
}

/// mkswap(8).
pub fn mkswap(args: &mut Args) -> Result<i32, Fail> {
    let (mut label, mut uuid, mut force, mut quiet) = (None, None, false, false);
    let mut operands = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('L') => label = Some(args.text()?),
            Arg::Long(l) if l == "label" => label = Some(args.text()?),
            Arg::Short('U') => uuid = Some(args.text()?),
            Arg::Long(l) if l == "uuid" => uuid = Some(args.text()?),
            Arg::Short('f') => force = true,
            Arg::Long(l) if l == "force" => force = true,
            Arg::Short('q') => quiet = true,
            Arg::Long(l) if l == "quiet" => quiet = true,
            Arg::Short('p') => page_size(&args.text()?)?,
            Arg::Long(l) if l == "pagesize" => page_size(&args.text()?)?,
            Arg::Short('v') => swap_version(&args.text()?)?,
            Arg::Long(l) if l == "swapversion" => swap_version(&args.text()?)?,
            Arg::Short('c') => return Err(util::fail("-c is not supported yet".into())),
            Arg::Long(l) if l == "check" => {
                return Err(util::fail("--check is not supported yet".into()));
            }
            Arg::Operand(operand) => operands.push(operand),
            other => return Err(args.unknown(&other)),
        }
    }
    let Some(path) = operands.first().cloned() else {
        return Err(util::usage("error: Nowhere to set up swap on?".into()));
    };
    let shown = path.to_string_lossy().into_owned();
    let asked_kib = match operands.get(1) {
        Some(size) => Some(size.to_string_lossy().parse::<u64>().map_err(|_| {
            util::fail(format!("invalid block count argument: '{}'", size.display()))
        })?),
        None => None,
    };
    if operands.len() > 2 {
        return Err(util::usage("only one device may be specified".into()));
    }
    let uuid = match uuid {
        Some(text) => {
            parse_uuid(&text).ok_or_else(|| util::fail("error: parsing UUID failed".into()))?
        }
        None => random_uuid().map_err(|e| util::failed(&"/devices/urandom", e))?,
    };
    let mut name = [0u8; 16];
    if let Some(label) = &label {
        if label.len() > name.len() {
            eprintln!("mkswap: Label was truncated.");
        }
        let len = label.len().min(name.len());
        name[..len].copy_from_slice(&label.as_bytes()[..len]);
    }

    let mut device =
        OpenOptions::new().read(true).write(true).open(&path).map_err(|e| {
            util::fail(format!("cannot open {shown}: {}", util::strip_os_error(&e)))
        })?;
    let bytes = size_of(&device).map_err(|e| util::failed(&shown, e))?;
    let device_kib = bytes / 1024;
    let pages = match asked_kib {
        Some(kib) if kib > device_kib && !force => {
            return Err(util::fail(format!(
                "error: size {kib} KiB is larger than device size {device_kib} KiB"
            )));
        }
        Some(kib) => kib * 1024 / PAGE as u64,
        None => bytes / PAGE as u64,
    };
    if pages < MIN_PAGES {
        return Err(util::fail(format!(
            "error: swap area needs to be at least {} KiB",
            MIN_PAGES * PAGE as u64 / 1024
        )));
    }

    // The first page as it is, for what it keeps of it.
    let mut page = vec![0u8; PAGE];
    device.read_exact(&mut page).map_err(|e| util::failed(&shown, e))?;
    let old_swap = &page[PAGE - MAGIC.len()..] == MAGIC;
    match partition_table(&page).filter(|_| !force) {
        Some(kind) => {
            eprintln!("mkswap: {shown}: warning: don't erase bootbits sectors");
            eprintln!("        ({kind} partition table detected). Use -f to force.");
        }
        None => page[..VERSION_AT].fill(0),
    }
    page[VERSION_AT..].fill(0);
    let last_page = u32::try_from(pages - 1).unwrap_or(u32::MAX);
    page[VERSION_AT..VERSION_AT + 4].copy_from_slice(&1u32.to_le_bytes());
    page[VERSION_AT + 4..VERSION_AT + 8].copy_from_slice(&last_page.to_le_bytes());
    page[UUID_AT..UUID_AT + 16].copy_from_slice(&uuid);
    page[LABEL_AT..LABEL_AT + 16].copy_from_slice(&name);
    page[PAGE - MAGIC.len()..].copy_from_slice(MAGIC);
    if old_swap && !quiet {
        eprintln!("mkswap: {shown}: warning: wiping old swap signature.");
    }
    wipe_btrfs(&mut device, bytes, &shown, quiet)?;
    let write = |device: &mut File| -> io::Result<()> {
        device.seek(SeekFrom::Start(0))?;
        device.write_all(&page)?;
        device.sync_all()
    };
    write(&mut device).map_err(|e| {
        util::fail(format!("unable to write signature page: {}", util::strip_os_error(&e)))
    })?;
    // The links of /devices/disk follow the area's label and UUID.
    crate::links::refresh_quietly();
    if !quiet {
        let size = (u64::from(last_page)) * PAGE as u64;
        let mut out = io::stdout().lock();
        let _ = writeln!(
            out,
            "Setting up swapspace version 1, size = {} ({size} bytes)",
            human(size, true)
        );
        let named = match &label {
            Some(label) => format!("LABEL={label}"),
            None => "no label".to_string(),
        };
        let _ = writeln!(out, "{named}, UUID={}", uuid_text(&uuid));
    }
    Ok(0)
}

/// Clears the magic of each copy of a Btrfs superblock on `device`, of
/// `bytes` bytes, saying so unless `quiet`.
fn wipe_btrfs(device: &mut File, bytes: u64, shown: &str, quiet: bool) -> Result<(), Fail> {
    let mut said = false;
    for copy in BTRFS_COPIES {
        let at = copy + BTRFS_MAGIC_AT;
        if at + BTRFS_MAGIC.len() as u64 > bytes {
            break;
        }
        let mut magic = [0u8; 8];
        let read = device.seek(SeekFrom::Start(at)).and_then(|_| device.read_exact(&mut magic));
        if read.is_err() || &magic != BTRFS_MAGIC {
            continue;
        }
        if !said && !quiet {
            eprintln!("mkswap: {shown}: warning: wiping old btrfs signature.");
            said = true;
        }
        device
            .seek(SeekFrom::Start(at))
            .and_then(|_| device.write_all(&[0; 8]))
            .map_err(|e| util::failed(&shown, e))?;
    }
    Ok(())
}

fn page_size(text: &str) -> Result<(), Fail> {
    if text == "4096" {
        Ok(())
    } else {
        Err(util::fail(format!("error: the page size {text} is not supported: only 4096")))
    }
}

fn swap_version(text: &str) -> Result<(), Fail> {
    if text == "1" {
        Ok(())
    } else {
        Err(util::fail(format!("swapspace version {text} is not supported")))
    }
}

/// An area `/processes/swaps` lists: its device, kind, size and use in
/// KiB, and priority.
struct Listed {
    name: String,
    kind: String,
    size: u64,
    used: u64,
    priority: i64,
}

/// The areas on.
fn listed() -> Result<Vec<Listed>, Fail> {
    let text = fs::read_to_string(SWAPS).map_err(|e| util::failed(&SWAPS, e))?;
    Ok(text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(Listed {
                name: fields.next()?.to_string(),
                kind: fields.next()?.to_string(),
                size: fields.next()?.parse().ok()?,
                used: fields.next()?.parse().ok()?,
                priority: fields.next()?.parse().ok()?,
            })
        })
        .collect())
}

/// The UUID and label in the header of the area on `name`.
fn header_names(name: &str) -> (String, String) {
    let mut page = vec![0u8; PAGE];
    let read = File::open(name).and_then(|mut device| device.read_exact(&mut page));
    if read.is_err() || &page[PAGE - MAGIC.len()..] != MAGIC {
        return (String::new(), String::new());
    }
    let label = &page[LABEL_AT..LABEL_AT + 16];
    let len = label.iter().position(|&b| b == 0).unwrap_or(label.len());
    (uuid_text(&page[UUID_AT..UUID_AT + 16]), String::from_utf8_lossy(&label[..len]).into_owned())
}

/// swapon(8).
pub fn swapon(args: &mut Args) -> Result<i32, Fail> {
    let (mut priority, mut if_exists, mut verbose, mut summary) = (None, false, false, false);
    let (mut show, mut headings, mut raw, mut bytes) = (None, true, false, false);
    let mut devices = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('p') => priority = Some(args.text()?),
            Arg::Long(l) if l == "priority" => priority = Some(args.text()?),
            Arg::Short('e') => if_exists = true,
            Arg::Long(l) if l == "ifexists" => if_exists = true,
            Arg::Short('v') => verbose = true,
            Arg::Long(l) if l == "verbose" => verbose = true,
            Arg::Short('s') => summary = true,
            Arg::Long(l) if l == "summary" => summary = true,
            Arg::Long(l) if l == "show" => {
                let columns = args.optional_value().map(|c| c.to_string_lossy().into_owned());
                show = Some(columns.unwrap_or_else(|| "NAME,TYPE,SIZE,USED,PRIO".into()));
            }
            Arg::Long(l) if l == "noheadings" => headings = false,
            Arg::Long(l) if l == "raw" => raw = true,
            Arg::Long(l) if l == "bytes" => bytes = true,
            Arg::Short('d') => {
                return Err(util::fail("discard: Operation not supported".into()));
            }
            Arg::Long(l) if l == "discard" => {
                let _ = args.optional_value();
                return Err(util::fail("discard: Operation not supported".into()));
            }
            Arg::Short('a') => {
                return Err(util::fail("-a is not supported: there is no /etc/fstab".into()));
            }
            Arg::Long(l) if l == "all" => {
                return Err(util::fail("--all is not supported: there is no /etc/fstab".into()));
            }
            Arg::Operand(operand) => devices.push(operand),
            other => return Err(args.unknown(&other)),
        }
    }
    if summary {
        let text = fs::read(SWAPS).map_err(|e| util::failed(&SWAPS, e))?;
        io::stdout().lock().write_all(&text)?;
        return Ok(0);
    }
    if devices.is_empty() || show.is_some() {
        let columns = show.unwrap_or_else(|| "NAME,TYPE,SIZE,USED,PRIO".into());
        print_show(&columns, headings, raw, bytes)?;
        if devices.is_empty() {
            return Ok(0);
        }
    }
    let flags = match &priority {
        Some(text) => {
            let priority: u32 = text
                .parse()
                .ok()
                .filter(|&p| p <= block::SWAP_PRIORITY_MASK)
                .ok_or_else(|| util::fail(format!("failed to parse priority: '{text}'")))?;
            block::SWAP_PREFER | priority
        }
        None => 0,
    };
    let mut status = 0;
    for path in devices {
        let shown = path.to_string_lossy().into_owned();
        let device = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(device) => device,
            Err(e) if if_exists && e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                eprintln!("swapon: cannot find the device for {shown}");
                status = 1;
                continue;
            }
            Err(e) => {
                eprintln!("swapon: {shown}: swapon failed: {}", util::strip_os_error(&e));
                status = 1;
                continue;
            }
        };
        let mut page = vec![0u8; PAGE];
        let read = (&device).read_exact(&mut page);
        if read.is_err() || &page[PAGE - MAGIC.len()..] != MAGIC {
            eprintln!("swapon: {shown}: read swap header failed");
            status = 1;
            continue;
        }
        if verbose {
            println!("swapon {shown}");
        }
        let mut value = flags.to_le_bytes();
        if let Err(s) = control(&device, block::SWAP_ON, &mut value) {
            let reason = if Kernel::from_status(s) == Some(Kernel::InvalidArgument) {
                "Invalid argument".to_string()
            } else {
                util::strip_os_error(&sys::io_error(s))
            };
            eprintln!("swapon: {shown}: swapon failed: {reason}");
            status = 1;
        }
    }
    Ok(status)
}

/// swapon's `--show`, with `columns`.
fn print_show(columns: &str, headings: bool, raw: bool, bytes: bool) -> Result<(), Fail> {
    let columns: Vec<String> = columns.split(',').map(|c| c.trim().to_ascii_uppercase()).collect();
    for column in &columns {
        if !matches!(column.as_str(), "NAME" | "TYPE" | "SIZE" | "USED" | "PRIO" | "UUID" | "LABEL")
        {
            return Err(util::fail(format!("unknown column: {column}")));
        }
    }
    let size = |kib: u64| if bytes { (kib * 1024).to_string() } else { human(kib * 1024, false) };
    let mut rows = Vec::new();
    for area in listed()? {
        let (uuid, label) = if columns.iter().any(|c| c == "UUID" || c == "LABEL") {
            header_names(&area.name)
        } else {
            (String::new(), String::new())
        };
        let row: Vec<String> = columns
            .iter()
            .map(|column| match column.as_str() {
                "NAME" => area.name.clone(),
                "TYPE" => area.kind.clone(),
                "SIZE" => size(area.size),
                "USED" => size(area.used),
                "PRIO" => area.priority.to_string(),
                "UUID" => uuid.clone(),
                _ => label.clone(),
            })
            .collect();
        rows.push(row);
    }
    if rows.is_empty() {
        // util-linux prints nothing, headings neither, with no area on.
        return Ok(());
    }
    if headings {
        rows.insert(0, columns.clone());
    }
    let right = |column: &str| matches!(column, "SIZE" | "USED" | "PRIO");
    let widths: Vec<usize> = (0..columns.len())
        .map(|at| rows.iter().map(|row| row[at].len()).max().unwrap_or(0))
        .collect();
    let mut out = io::stdout().lock();
    for row in &rows {
        let mut line = String::new();
        for (at, cell) in row.iter().enumerate() {
            if at > 0 {
                line.push(' ');
            }
            let last = at + 1 == row.len();
            if raw {
                line.push_str(cell);
            } else if right(&columns[at]) {
                let _ = write!(line, "{cell:>width$}", width = widths[at]);
            } else if last {
                line.push_str(cell);
            } else {
                let _ = write!(line, "{cell:<width$}", width = widths[at]);
            }
        }
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// swapoff(8)'s exit statuses, as util-linux's.
const OFF_NO_MEMORY: i32 = 2;
const OFF_FAILED: i32 = 4;
const OFF_USAGE: i32 = 16;
const OFF_ALL_FAILED: i32 = 32;
const OFF_SOME_FAILED: i32 = 64;

/// swapoff(8).
pub fn swapoff(args: &mut Args) -> Result<i32, Fail> {
    let usage = |fail: Fail| if fail.usage { Fail { code: OFF_USAGE, ..fail } } else { fail };
    let (mut all, mut verbose) = (false, false);
    let mut devices = Vec::new();
    while let Some(arg) = args.next().map_err(usage)? {
        match arg {
            Arg::Short('a') => all = true,
            Arg::Long(l) if l == "all" => all = true,
            Arg::Short('v') => verbose = true,
            Arg::Long(l) if l == "verbose" => verbose = true,
            Arg::Operand(operand) => devices.push(operand.to_string_lossy().into_owned()),
            other => return Err(usage(args.unknown(&other))),
        }
    }
    if !all && devices.is_empty() {
        return Err(Fail { code: OFF_USAGE, ..util::usage("bad usage".into()) });
    }
    let mut failed = 0;
    let mut status = 0;
    for name in &devices {
        if let Some(code) = turn_off(name, verbose) {
            status |= code;
        }
    }
    if all {
        let areas = listed()?;
        for area in &areas {
            if let Some(code) = turn_off(&area.name, verbose) {
                status |= code;
                failed += 1;
            }
        }
        if failed > 0 {
            status |= if failed == areas.len() { OFF_ALL_FAILED } else { OFF_SOME_FAILED };
        }
    }
    Ok(status)
}

/// Turns off the area on `name`: the exit status of a failure, which it
/// reports.
fn turn_off(name: &str, verbose: bool) -> Option<i32> {
    if verbose {
        println!("swapoff {name}");
    }
    let device = match OpenOptions::new().read(true).write(true).open(name) {
        Ok(device) => device,
        Err(e) => {
            eprintln!("swapoff: {name}: swapoff failed: {}", util::strip_os_error(&e));
            return Some(OFF_FAILED);
        }
    };
    let Err(s) = control(&device, block::SWAP_OFF, &mut []) else { return None };
    let reason = if Kernel::from_status(s) == Some(Kernel::InvalidArgument) {
        "Invalid argument".to_string()
    } else {
        util::strip_os_error(&sys::io_error(s))
    };
    eprintln!("swapoff: {name}: swapoff failed: {reason}");
    Some(if Kernel::from_status(s) == Some(Kernel::NoMemory) { OFF_NO_MEMORY } else { OFF_FAILED })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_as_util_linux_writes_them() {
        // size_to_human_string, as util-linux 2.39's swapon and mkswap
        // print them.
        for (bytes, one, three) in [
            (0, "0B", "0 B"),
            (1023, "1023B", "1023 B"),
            (1024, "1K", "1 KiB"),
            (268_431_360, "256M", "256 MiB"),
            (1_610_612_736, "1.5G", "1.5 GiB"),
            (40_960, "40K", "40 KiB"),
            (1_073_737_728, "1024M", "1024 MiB"),
        ] {
            assert_eq!(human(bytes, false), one, "{bytes}");
            assert_eq!(human(bytes, true), three, "{bytes}");
        }
    }

    #[test]
    fn uuids_read_and_write_back() {
        let text = "8e2d7c7b-46a2-4e0c-9d7c-bd0a1f6e5a3e";
        let uuid = parse_uuid(text).unwrap();
        assert_eq!(uuid_text(&uuid), text);
        assert_eq!(parse_uuid("8e2d7c7b46a24e0c9d7cbd0a1f6e5a3e"), None);
        assert_eq!(parse_uuid("8e2d7c7b-46a2-4e0c-9d7c-bd0a1f6e5a3"), None);
    }
}
