// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! rdisk(8): Dukovo's partitioner (ADR 0300). It reads and writes GPTs
//! and MBRs with the disk engine, as fdisk does, at a prompt of fdisk's
//! letters; with `--script` it applies a layout file (rdisk-layout(5)),
//! as the installer does, and `--dump` writes one of the disk's table.
//! Nothing is written until `w`, or the script asks, and never with
//! `--dry-run`, which says what would be. A disk in use, or a partition
//! in use that the new table would change or take away, is refused before
//! anything is written; once the table is, the kernel reads it again
//! (radix-block(2)'s REREAD_PARTITIONS, ADR 0301) and the links in
//! `/devices/disk` follow.

use std::ffi::OsString;
use std::io::{self, BufRead, Read as _, Write};

use disk::layout;
use disk::size::{self, Amount};
use disk::types::{self, TYPES};
use disk::{End, Found, Kind, New, Rng, Scheme, Table};

use crate::args::{Arg, Args};
use crate::blockdev::{self, Opened};
use crate::util::{self, Fail};
use crate::{links, probe, sys};

/// What rdisk was asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Interactive,
    List,
    Script,
    Dump,
    Reread,
}

/// rdisk(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let (mut mode, mut dry_run, mut sector) = (Mode::Interactive, false, None);
    let mut script: Option<OsString> = None;
    let mut operands = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('l') => mode = Mode::List,
            Arg::Long(l) if l == "list" => mode = Mode::List,
            Arg::Long(l) if l == "script" => {
                mode = Mode::Script;
                script = Some(args.value()?);
            }
            Arg::Long(l) if l == "dump" => mode = Mode::Dump,
            Arg::Long(l) if l == "reread" => mode = Mode::Reread,
            Arg::Short('n') => dry_run = true,
            Arg::Long(l) if l == "dry-run" => dry_run = true,
            Arg::Long(l) if l == "sector-size" => sector = Some(args.number()?),
            Arg::Long(l) if l == "list-types" => {
                list_types(&mut io::stdout().lock(), None)?;
                return Ok(0);
            }
            Arg::Short('h') => return help(),
            Arg::Long(l) if l == "help" => return help(),
            Arg::Operand(operand) => operands.push(operand),
            other => return Err(args.unknown(&other)),
        }
    }
    let mut out = io::stdout().lock();
    if mode == Mode::List {
        return list(&mut out, &operands, sector);
    }
    let [path] = operands.as_slice() else {
        return Err(util::usage("say the one disk (or image file) to partition".into()));
    };
    let write = matches!(mode, Mode::Interactive | Mode::Script | Mode::Reread) && !dry_run;
    let opened = blockdev::open(path, write, sector)?;
    if write && !opened.writable {
        return Err(util::fail(format!("{} refuses writes", opened.shown)));
    }
    if opened.name.as_deref().is_some_and(|name| probe::block_text(name, "partition").is_some()) {
        return Err(util::fail(format!(
            "{} is a partition; rdisk partitions the disk it is on",
            opened.shown
        )));
    }
    match mode {
        Mode::Dump => {
            let found = read(&opened)?;
            let Some(table) = found.table else {
                return Err(util::fail(format!("{} has no partition table", opened.shown)));
            };
            let text = layout::dump_text(&table).map_err(util::fail)?;
            out.write_all(text.as_bytes())?;
            Ok(0)
        }
        Mode::Reread => {
            reread(&mut out, &opened)?;
            Ok(0)
        }
        Mode::Script => {
            let source = script.unwrap_or_default();
            let text = if source == "-" {
                let mut text = String::new();
                io::stdin()
                    .read_to_string(&mut text)
                    .map_err(|e| util::failed(&"standard input", e))?;
                text
            } else {
                std::fs::read_to_string(&source).map_err(|e| util::failed(&source.display(), e))?
            };
            run_script(&mut out, &opened, &text, dry_run)
        }
        Mode::Interactive | Mode::List => {
            let stdin = io::stdin();
            let mut input = stdin.lock();
            interactive(&mut input, &mut out, &opened, dry_run)
        }
    }
}

fn help() -> Result<i32, Fail> {
    println!(
        "usage: rdisk [--dry-run] DISK              partition DISK at a prompt (m lists the letters)\n\
         \x20      rdisk -l [DISK...]                list the tables of DISK, or of every disk\n\
         \x20      rdisk --script FILE [--dry-run] DISK   apply the layout file FILE (- for standard input)\n\
         \x20      rdisk --dump DISK                 write DISK's table as a layout file\n\
         \x20      rdisk --reread DISK               have the kernel read DISK's table again\n\
         \x20      rdisk --list-types                list the partition types\n\
         An image file takes --sector-size N (512 if not given). rdisk-layout(5) describes layout files."
    );
    Ok(0)
}

/// What is on the disk `opened`.
fn read(opened: &Opened) -> Result<Found, Fail> {
    let device = opened.device().map_err(|e| util::failed(&opened.shown, e))?;
    disk::read(&device).map_err(|err| util::fail(format!("{}: {err}", opened.shown)))
}

/// `-l`: the tables of `operands`, or of every disk.
fn list(out: &mut dyn Write, operands: &[OsString], sector: Option<u32>) -> Result<i32, Fail> {
    let named: Vec<OsString> = if operands.is_empty() {
        let disks = probe::devices().map_err(|e| util::failed(&probe::BLOCKS, e))?;
        disks
            .into_iter()
            .filter(|(name, part)| !part && !name.ends_with("-unlocked"))
            .map(|(name, _)| OsString::from(format!("/devices/{name}")))
            .collect()
    } else {
        operands.to_vec()
    };
    let mut status = 0;
    for (i, path) in named.iter().enumerate() {
        if i > 0 {
            writeln!(out)?;
        }
        let shown = match blockdev::open(path, false, sector).and_then(|opened| {
            let found = read(&opened)?;
            print_table(out, &opened, found.table.as_ref(), &found.notes)?;
            Ok(())
        }) {
            Ok(()) => continue,
            Err(fail) => fail.message,
        };
        eprintln!("rdisk: {shown}");
        status = 1;
    }
    Ok(status)
}

/// The name of partition `number` of the disk `opened`: `/devices/vda1`,
/// `/devices/nvme0n1p1`, or `disk.img1` for an image.
fn part_name(opened: &Opened, number: u32) -> String {
    let p = if opened.shown.ends_with(|c: char| c.is_ascii_digit()) { "p" } else { "" };
    match &opened.name {
        Some(name) => {
            let p = if name.ends_with(|c: char| c.is_ascii_digit()) { "p" } else { "" };
            format!("/devices/{name}{p}{number}")
        }
        None => format!("{}{p}{number}", opened.shown),
    }
}

/// Prints the disk and its table, as fdisk's `p` does.
fn print_table(
    out: &mut dyn Write,
    opened: &Opened,
    table: Option<&Table>,
    notes: &[String],
) -> io::Result<()> {
    let sectors = opened.size / u64::from(opened.sector);
    writeln!(
        out,
        "Disk {}: {}, {} bytes, {sectors} sectors",
        opened.shown,
        size::human(opened.size),
        opened.size
    )?;
    writeln!(out, "Sector size: {} bytes", opened.sector)?;
    for note in notes {
        writeln!(out, "Note: {note}")?;
    }
    let Some(table) = table else {
        return writeln!(out, "Partition table: none");
    };
    match table.scheme {
        Scheme::Gpt => {
            writeln!(out, "Partition table: gpt")?;
            writeln!(out, "Disk identifier: {}", table.disk_guid)?;
            writeln!(
                out,
                "First usable sector: {}, last usable sector: {}",
                table.first_usable, table.last_usable
            )?;
        }
        Scheme::Mbr => {
            writeln!(out, "Partition table: dos")?;
            writeln!(out, "Disk identifier: 0x{:08x}", table.signature)?;
        }
    }
    if table.partitions.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    let gpt = table.scheme == Scheme::Gpt;
    let named = gpt && table.partitions.iter().any(|p| !p.name.is_empty());
    let rows: Vec<[String; 7]> = table
        .partitions
        .iter()
        .map(|p| {
            [
                part_name(opened, p.number),
                if p.bootable { "*".into() } else { String::new() },
                p.first.to_string(),
                p.last.to_string(),
                p.sectors().to_string(),
                size::human(p.sectors() * u64::from(table.sector_size)),
                p.kind.name().to_string(),
            ]
        })
        .collect();
    let heads = ["Device", "Boot", "Start", "End", "Sectors", "Size", "Type"];
    let mut widths = heads.map(str::len);
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(util::width(cell));
        }
    }
    let line = |out: &mut dyn Write, cells: [&str; 7], name: Option<&str>| -> io::Result<()> {
        let mut text = format!("{:<w$}", cells[0], w = widths[0]);
        if !gpt {
            text.push_str(&format!(" {:<w$}", cells[1], w = widths[1]));
        }
        for (i, cell) in cells.iter().enumerate().take(6).skip(2) {
            text.push_str(&format!(" {cell:>w$}", w = widths[i]));
        }
        text.push_str(&format!(" {}", cells[6]));
        if let Some(name) = name {
            let pad = widths[6].saturating_sub(util::width(cells[6]));
            text.push_str(&format!("{:pad$} {name}", ""));
        }
        writeln!(out, "{}", text.trim_end())
    };
    line(out, heads, named.then_some("Name"))?;
    for (row, p) in rows.iter().zip(&table.partitions) {
        let cells = [&*row[0], &*row[1], &*row[2], &*row[3], &*row[4], &*row[5], &*row[6]];
        line(out, cells, named.then_some(p.name.as_str()))?;
    }
    Ok(())
}

/// The partition types, as `l` and `--list-types` list them: those of
/// `scheme`, or of both.
fn list_types(out: &mut dyn Write, scheme: Option<Scheme>) -> io::Result<()> {
    writeln!(out, "{:<18} {:<30} {:<6} {:<5}", "Alias", "Name", "GPT", "MBR")?;
    for t in TYPES {
        let fits = match scheme {
            Some(Scheme::Gpt) => t.gpt.is_some() || t.alias.starts_with("fat"),
            Some(Scheme::Mbr) => t.mbr.is_some() || t.alias == "dukovo-root",
            None => true,
        };
        if !fits {
            continue;
        }
        let gpt = t.code.map_or_else(
            || if t.gpt.is_some() { "yes".into() } else { "-".into() },
            |c| format!("{c:04x}"),
        );
        let mbr = t.mbr.map_or_else(|| "-".into(), |b| format!("{b:02x}"));
        writeln!(out, "{:<18} {:<30} {gpt:<6} {mbr:<5}", t.alias, t.name)?;
    }
    writeln!(out, "A type may also be given as a GUID (GPT) or as a byte in hexadecimal (MBR).")
}

/// What uses the partitions of the disk `opened` that `table` changes
/// or takes away, as the kernel has them: none for an image file.
///
/// # Errors
///
/// Saying which, if any is in use, or the disk is.
fn refuse_in_use(opened: &Opened, table: &Table) -> Result<(), Fail> {
    let Some(name) = &opened.name else { return Ok(()) };
    let uses = opened.uses();
    if uses != 0 {
        return Err(util::fail(format!(
            "{}; rdisk writes no table on a disk in use",
            blockdev::in_use(name, uses)
        )));
    }
    let unit = u64::from(table.sector_size) / 512;
    for (number, part) in blockdev::partitions_of(name) {
        let (Some(start), Some(sectors)) = (
            probe::block_text(&part, "start").and_then(|s| s.parse::<u64>().ok()),
            probe::block_text(&part, "size").and_then(|s| s.parse::<u64>().ok()),
        ) else {
            continue;
        };
        let stays = table
            .get(number)
            .is_some_and(|p| p.first * unit == start && p.sectors() * unit == sectors);
        if stays {
            continue;
        }
        let Ok(opened) = blockdev::open(&OsString::from(format!("/devices/{part}")), false, None)
        else {
            continue;
        };
        let uses = opened.uses();
        if uses != 0 {
            return Err(util::fail(format!(
                "{}; the new table changes it, so rdisk writes nothing",
                blockdev::in_use(&part, uses)
            )));
        }
    }
    Ok(())
}

/// Writes `table` to the disk `opened`, or with `dry_run` says what it
/// would write; then has the kernel read it and the links follow.
fn write_table(
    out: &mut dyn Write,
    opened: &Opened,
    table: &Table,
    dry_run: bool,
) -> Result<(), Fail> {
    let problems = table.verify();
    if let Some(bad) = problems.iter().find(|p| p.contains("overlap") || p.contains("not within")) {
        return Err(util::fail(format!("the table is not written: {bad}")));
    }
    refuse_in_use(opened, table)?;
    let device = opened.device().map_err(|e| util::failed(&opened.shown, e))?;
    let writes = disk::plan(&device, table).map_err(|e| util::failed(&opened.shown, e))?;
    if dry_run {
        writeln!(out, "With --dry-run, nothing is written. Writing the table would write:")?;
        for write in &writes {
            writeln!(out, "  {}", write.what)?;
        }
        return Ok(());
    }
    disk::apply(&device, &writes).map_err(|e| {
        util::fail(format!(
            "cannot write the table to {}: {}",
            opened.shown,
            util::strip_os_error(&e)
        ))
    })?;
    writeln!(out, "The partition table is written.")?;
    if opened.is_device() {
        reread(out, opened)?;
    }
    Ok(())
}

/// Has the kernel read the disk's table again, and the links follow.
fn reread(out: &mut dyn Write, opened: &Opened) -> Result<(), Fail> {
    if !opened.is_device() {
        return Err(util::fail(format!(
            "{} is a file; the kernel reads tables of disks",
            opened.shown
        )));
    }
    match opened.reread() {
        Ok(()) => {
            writeln!(out, "The kernel has read it; its partitions are in /devices.")?;
            links::refresh_quietly();
            Ok(())
        }
        Err(status) => Err(util::fail(format!(
            "the kernel still has the old table: {}; it reads the new one at the next boot, or \
             with `rdisk --reread {}` once nothing uses the disk",
            util::strip_os_error(&sys::io_error(status)),
            opened.shown
        ))),
    }
}

/// `--script`: applies the layout `text` to the disk `opened`.
fn run_script(
    out: &mut dyn Write,
    opened: &Opened,
    text: &str,
    dry_run: bool,
) -> Result<i32, Fail> {
    let found = read(opened)?;
    let layout = layout::parse(text).map_err(|why| util::fail(format!("the layout: {why}")))?;
    let sectors = opened.size / u64::from(opened.sector);
    let table =
        layout::apply(&layout, opened.sector, sectors, found.table.as_ref(), &mut Rng::system())
            .map_err(|why| util::fail(format!("the layout: {why}")))?;
    if let Some(old) = found.table.as_ref().filter(|t| !layout.keep && !t.partitions.is_empty()) {
        writeln!(
            out,
            "The {} with {} partitions on {} is replaced.",
            old.scheme.name().to_uppercase(),
            old.partitions.len(),
            opened.shown
        )?;
    }
    for signature in &found.signatures {
        writeln!(
            out,
            "{} holds a {} signature, which writing the table erases.",
            opened.shown, signature.name
        )?;
    }
    write_table(out, opened, &table, dry_run)?;
    writeln!(out)?;
    print_table(out, opened, Some(&table), &[])?;
    Ok(0)
}

/// The prompt's state: the disk, and the table as it is changed.
struct Session<'a> {
    opened: &'a Opened,
    table: Option<Table>,
    rng: Rng,
    changed: bool,
}

/// What the person typed at a prompt: `None` at the end of the input.
fn ask(input: &mut dyn BufRead, out: &mut dyn Write, prompt: &str) -> io::Result<Option<String>> {
    write!(out, "{prompt}")?;
    out.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        writeln!(out)?;
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}

/// A sector as typed at a prompt: a number of sectors, or a size with
/// its unit (`1M`, `4096s`).
fn sector_of(text: &str, sector: u32) -> Option<u64> {
    if text.bytes().all(|b| b.is_ascii_digit()) {
        return text.parse().ok();
    }
    size::parse(text)?.sectors(sector)
}

const HELP: &str = "\
  m  this list
  p  print the table
  F  list the free space
  l  list the partition types
  n  add a new partition
  d  delete a partition
  t  change a partition's type
  c  change a partition's name (GPT)
  u  change a partition's UUID (GPT)
  a  mark a partition bootable, or not (MBR)
  g  start a new, empty GPT
  o  start a new, empty MBR
  v  verify the table
  w  write the table to the disk, and quit
  q  quit without writing";

/// rdisk at its prompt, reading `input`.
fn interactive(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    opened: &Opened,
    dry_run: bool,
) -> Result<i32, Fail> {
    let found = read(opened)?;
    writeln!(
        out,
        "Welcome to rdisk. Nothing is written to {} until you write it with w.",
        opened.shown
    )?;
    if dry_run {
        writeln!(out, "With --dry-run, w says what it would write and writes nothing.")?;
    }
    for note in &found.notes {
        writeln!(out, "Note: {note}")?;
    }
    for signature in &found.signatures {
        writeln!(
            out,
            "{} holds a {} signature, which writing a table erases.",
            opened.shown, signature.name
        )?;
    }
    let mut session = Session { opened, table: found.table, rng: Rng::system(), changed: false };
    if session.table.is_none() {
        let sectors = opened.size / u64::from(opened.sector);
        let table = Table::new(Scheme::Gpt, opened.sector, sectors, &mut session.rng)
            .map_err(|e| util::fail(format!("{}: {}", opened.shown, e.0)))?;
        writeln!(
            out,
            "{} has no partition table; rdisk starts a new GPT (o starts an MBR instead).",
            opened.shown
        )?;
        session.table = Some(table);
        session.changed = true;
    }
    writeln!(out)?;
    loop {
        let Some(command) = ask(input, out, "Command (m for help): ")? else {
            writeln!(out, "Nothing was written.")?;
            return Ok(0);
        };
        let done = match command.as_str() {
            "" => Ok(false),
            "m" => writeln!(out, "{HELP}").map(|()| false).map_err(Fail::from),
            "p" => print_table(out, opened, session.table.as_ref(), &[])
                .map(|()| false)
                .map_err(Fail::from),
            "q" => {
                writeln!(out, "Nothing was written.")?;
                return Ok(0);
            }
            "w" => {
                let table = session.table.as_ref().ok_or_else(|| util::fail("no table".into()))?;
                match write_table(out, opened, table, dry_run) {
                    Ok(()) => return Ok(0),
                    Err(fail) => {
                        writeln!(out, "{}", fail.message)?;
                        // A table written that the kernel did not take is
                        // still written.
                        if fail.message.starts_with("the kernel still has") {
                            return Ok(1);
                        }
                        Ok(false)
                    }
                }
            }
            other => session.command(input, out, other).map(|()| false),
        };
        match done {
            Ok(_) => {}
            Err(fail) => writeln!(out, "{}", fail.message)?,
        }
    }
}

impl Session<'_> {
    fn table(&mut self) -> &mut Table {
        self.table.as_mut().expect("a session always has a table")
    }

    /// The number of a partition there is, asked for: the only one, or
    /// the last, by default.
    fn which(&mut self, input: &mut dyn BufRead, out: &mut dyn Write) -> Result<Option<u32>, Fail> {
        let numbers: Vec<u32> = self.table().partitions.iter().map(|p| p.number).collect();
        let Some(&last) = numbers.last() else {
            writeln!(out, "There are no partitions yet.")?;
            return Ok(None);
        };
        if numbers.len() == 1 {
            writeln!(out, "Selected partition {last}")?;
            return Ok(Some(last));
        }
        let listed: Vec<String> = numbers.iter().map(u32::to_string).collect();
        let prompt = format!("Partition number ({}, default {last}): ", listed.join(","));
        let Some(text) = ask(input, out, &prompt)? else { return Ok(None) };
        if text.is_empty() {
            return Ok(Some(last));
        }
        match text.parse::<u32>() {
            Ok(n) if numbers.contains(&n) => Ok(Some(n)),
            _ => {
                writeln!(out, "There is no partition {text}.")?;
                Ok(None)
            }
        }
    }

    /// A letter other than those that end the session.
    fn command(
        &mut self,
        input: &mut dyn BufRead,
        out: &mut dyn Write,
        letter: &str,
    ) -> Result<(), Fail> {
        let opened = self.opened;
        let sector = opened.sector;
        let sectors = opened.size / u64::from(sector);
        match letter {
            "l" => {
                let scheme = self.table().scheme;
                list_types(out, Some(scheme))?;
            }
            "F" => {
                let table = self.table();
                let free = table.free();
                let bytes = table.unallocated() * u64::from(sector);
                writeln!(
                    out,
                    "Unpartitioned space: {} ({} sectors)",
                    size::human(bytes),
                    table.unallocated()
                )?;
                if !free.is_empty() {
                    writeln!(
                        out,
                        "{:>12} {:>12} {:>12} {:>10}",
                        "Start", "End", "Sectors", "Size"
                    )?;
                }
                for (first, last) in free {
                    let n = last - first + 1;
                    writeln!(
                        out,
                        "{first:>12} {last:>12} {n:>12} {:>10}",
                        size::human(n * u64::from(sector))
                    )?;
                }
            }
            "g" | "o" => {
                let scheme = if letter == "g" { Scheme::Gpt } else { Scheme::Mbr };
                let table = Table::new(scheme, sector, sectors, &mut self.rng)
                    .map_err(|e| util::fail(e.0))?;
                match scheme {
                    Scheme::Gpt => {
                        writeln!(out, "Started a new, empty GPT (disk GUID {}).", table.disk_guid)?;
                    }
                    Scheme::Mbr => writeln!(
                        out,
                        "Started a new, empty MBR (disk identifier 0x{:08x}).",
                        table.signature
                    )?,
                }
                self.table = Some(table);
                self.changed = true;
            }
            "n" => self.add(input, out)?,
            "d" => {
                if let Some(number) = self.which(input, out)? {
                    self.table().delete(number).map_err(|e| util::fail(e.0))?;
                    writeln!(out, "Partition {number} is deleted.")?;
                    self.changed = true;
                }
            }
            "t" => {
                let Some(number) = self.which(input, out)? else { return Ok(()) };
                let Some(text) = ask(input, out, "Partition type or alias (l lists them): ")?
                else {
                    return Ok(());
                };
                if text == "l" || text == "L" {
                    let scheme = self.table().scheme;
                    list_types(out, Some(scheme))?;
                    return Ok(());
                }
                let scheme = self.table().scheme;
                let Some(kind) = types::parse(&text, scheme) else {
                    writeln!(
                        out,
                        "'{text}' is not a type of a {}; l lists them.",
                        scheme.name().to_uppercase()
                    )?;
                    return Ok(());
                };
                let was = self.table().get(number).map_or("", |p| p.kind.name());
                self.table().set_kind(number, kind).map_err(|e| util::fail(e.0))?;
                writeln!(out, "Partition {number}'s type is now '{}' (was '{was}').", kind.name())?;
                self.changed = true;
            }
            "c" => {
                let Some(number) = self.which(input, out)? else { return Ok(()) };
                let Some(name) = ask(input, out, "New name: ")? else { return Ok(()) };
                self.table().set_name(number, &name).map_err(|e| util::fail(e.0))?;
                writeln!(out, "Partition {number} is named '{name}' now.")?;
                self.changed = true;
            }
            "u" => {
                let Some(number) = self.which(input, out)? else { return Ok(()) };
                let Some(text) = ask(input, out, "New UUID (empty for a new random one): ")? else {
                    return Ok(());
                };
                let guid = match text.as_str() {
                    "" => disk::Guid::random(&mut self.rng),
                    text => disk::Guid::from_text(text)
                        .ok_or_else(|| util::fail(format!("'{text}' is not a UUID")))?,
                };
                self.table().set_guid(number, guid).map_err(|e| util::fail(e.0))?;
                writeln!(out, "Partition {number}'s UUID is {guid} now.")?;
                self.changed = true;
            }
            "a" => {
                let Some(number) = self.which(input, out)? else { return Ok(()) };
                let now = !self.table().get(number).is_some_and(|p| p.bootable);
                self.table().set_bootable(number, now).map_err(|e| util::fail(e.0))?;
                let state = if now { "bootable" } else { "not bootable" };
                writeln!(out, "Partition {number} is {state} now.")?;
                self.changed = true;
            }
            "v" => {
                let table = self.table();
                let problems = table.verify();
                for problem in &problems {
                    writeln!(out, "{problem}")?;
                }
                if problems.is_empty() {
                    writeln!(out, "No problems found.")?;
                }
                writeln!(out, "{} unpartitioned sectors of {sector} bytes.", table.unallocated())?;
            }
            other => writeln!(out, "'{other}' is not a command; m lists them.")?,
        }
        Ok(())
    }

    /// `n`: adds a partition, asking for its number, first sector and end.
    fn add(&mut self, input: &mut dyn BufRead, out: &mut dyn Write) -> Result<(), Fail> {
        let sector = self.opened.sector;
        let table = self.table();
        let Some(default_number) = table.free_number() else {
            writeln!(out, "The table has no room for another partition.")?;
            return Ok(());
        };
        let free = table.free();
        let align = table.alignment();
        let start = free.iter().find_map(|&(a, b)| {
            let aligned = a.div_ceil(align) * align;
            (aligned <= b).then_some(aligned)
        });
        let Some(default_first) = start else {
            writeln!(out, "There is no free space left on a whole MiB.")?;
            return Ok(());
        };
        let (low, high) = (table.first_usable, table.last_usable);
        let slots = table.slots();
        let prompt = format!("Partition number (1-{slots}, default {default_number}): ");
        let Some(text) = ask(input, out, &prompt)? else { return Ok(()) };
        let number = match text.as_str() {
            "" => default_number,
            text => text.parse().map_err(|_| util::fail(format!("'{text}' is not a number")))?,
        };
        let prompt = format!("First sector ({low}-{high}, default {default_first}): ");
        let Some(text) = ask(input, out, &prompt)? else { return Ok(()) };
        let first = match text.as_str() {
            "" => default_first,
            text => sector_of(text, sector)
                .ok_or_else(|| util::fail(format!("'{text}' is not a sector")))?,
        };
        let room = self.table().free().into_iter().find(|&(a, b)| a <= first && first <= b);
        let room_end = room.map_or(high, |(_, b)| b);
        let prompt = format!(
            "Last sector, +/-sectors or +/-size{{K,M,G,T,P}} ({first}-{room_end}, default {room_end}): "
        );
        let Some(text) = ask(input, out, &prompt)? else { return Ok(()) };
        let end = if text.is_empty() {
            End::Last(room_end)
        } else if let Some(more) = text.strip_prefix('+') {
            let count = sector_count(more, sector)?;
            End::Sectors(count)
        } else if let Some(less) = text.strip_prefix('-') {
            let count = sector_count(less, sector)?;
            End::Last(room_end.saturating_sub(count))
        } else {
            End::Last(
                sector_of(&text, sector)
                    .ok_or_else(|| util::fail(format!("'{text}' is not a sector")))?,
            )
        };
        let scheme = self.table().scheme;
        let kind = types::parse("linux", scheme).unwrap_or(Kind::Mbr(0x83));
        let mut new = New::of(kind, end);
        new.number = Some(number);
        new.first = Some(first);
        let table = self.table.as_mut().expect("a session always has a table");
        let number = table.add(new, &mut self.rng).map_err(|e| util::fail(e.0))?;
        let made = self.table().get(number).map_or(0, disk::Partition::sectors);
        writeln!(
            out,
            "Partition {number} is added: '{}', {}.",
            kind.name(),
            size::human(made * u64::from(sector))
        )?;
        self.changed = true;
        Ok(())
    }
}

/// A count of sectors as typed after `+` or `-`: sectors, or a size.
fn sector_count(text: &str, sector: u32) -> Result<u64, Fail> {
    let count = if text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        match size::parse(text) {
            Some(amount @ Amount::Bytes(_)) => Some(amount.sectors_up(sector)),
            Some(amount) => amount.sectors(sector),
            None => None,
        }
    };
    count.filter(|&n| n > 0).ok_or_else(|| util::fail(format!("'{text}' is not a size")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image file of `mib` MiB, opened as rdisk opens one.
    fn image(mib: u64) -> (std::path::PathBuf, Opened) {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("rdisk-test-{}-{n}.img", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(mib << 20).unwrap();
        let opened = blockdev::open(path.as_os_str(), true, None).unwrap();
        (path, opened)
    }

    fn run(opened: &Opened, typed: &str) -> String {
        let mut out = Vec::new();
        interactive(&mut typed.as_bytes(), &mut out, opened, false).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_blank_disk_gets_a_gpt_at_the_prompt() {
        let (path, opened) = image(64);
        let said = run(
            &opened,
            "n\n\n\n+1M\nt\nbios-boot\nn\n\n\n+16M\nt\n2\nefi\nn\n\n\n\nc\n3\nDukovo\np\nw\n",
        );
        assert!(said.contains("has no partition table; rdisk starts a new GPT"), "{said}");
        assert!(said.contains("The partition table is written."), "{said}");
        let device = opened.device().unwrap();
        let table = disk::read(&device).unwrap().table.unwrap();
        let parts: Vec<(u32, u64, u64, &str)> =
            table.partitions.iter().map(|p| (p.number, p.first, p.last, p.kind.name())).collect();
        assert_eq!(
            parts,
            vec![
                (1, 2048, 4095, "BIOS boot"),
                (2, 4096, 36863, "EFI System"),
                (3, 36864, 131_038, "Linux filesystem")
            ]
        );
        assert_eq!(table.get(3).unwrap().name, "Dukovo");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nothing_is_written_until_w() {
        let (path, opened) = image(16);
        let said = run(&opened, "o\nn\n\n\n\na\nq\n");
        assert!(said.contains("Started a new, empty MBR"), "{said}");
        assert!(said.contains("Partition 1 is bootable now."), "{said}");
        assert!(said.ends_with("Nothing was written.\n"), "{said}");
        assert!(disk::read(&opened.device().unwrap()).unwrap().table.is_none());
        // The end of the input quits as q does.
        let said = run(&opened, "n\n");
        assert!(said.ends_with("Nothing was written.\n"), "{said}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mistakes_are_said_and_the_prompt_goes_on() {
        let (path, opened) = image(16);
        let said = run(&opened, "x\nd\nn\n\n9\n\nt\nnot-a-type\nq\n");
        assert!(said.contains("'x' is not a command; m lists them."), "{said}");
        assert!(said.contains("There are no partitions yet."), "{said}");
        assert!(said.contains("sector 9 is not free"), "{said}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_script_writes_a_layout_and_dry_run_writes_nothing() {
        let (path, opened) = image(64);
        let layout = r#"{"table": "gpt", "partitions": [
            {"type": "efi", "size": "16M", "name": "EFI"},
            {"type": "dukovo-root", "size": "rest"}]}"#;
        let mut out = Vec::new();
        run_script(&mut out, &opened, layout, true).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("With --dry-run, nothing is written."), "{said}");
        assert!(said.contains("the GPT's header, at sector 1"), "{said}");
        assert!(disk::read(&opened.device().unwrap()).unwrap().table.is_none());
        let mut out = Vec::new();
        run_script(&mut out, &opened, layout, false).unwrap();
        let table = disk::read(&opened.device().unwrap()).unwrap().table.unwrap();
        assert_eq!(table.partitions.len(), 2);
        assert_eq!(table.get(2).unwrap().kind.name(), "Dukovo root");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sectors_and_sizes_at_the_prompt() {
        assert_eq!(sector_of("2048", 512), Some(2048));
        assert_eq!(sector_of("1M", 512), Some(2048));
        assert_eq!(sector_of("8s", 4096), Some(8));
        assert_eq!(sector_count("1M", 512).unwrap(), 2048);
        assert_eq!(sector_count("100", 512).unwrap(), 100);
        assert!(sector_count("0", 512).is_err());
    }
}
