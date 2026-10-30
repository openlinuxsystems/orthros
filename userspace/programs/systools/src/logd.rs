// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! logd(8): keeps the system's log (ADR 0210).
//!
//! The kernel log is the one place everything is written to: the kernel's
//! own records, the lines programs write to it, and the lines of the
//! services' output, which Spark copies there after each service's name.
//! logd reads every record from `/devices/kmsg` as it comes, and appends
//! it as a line of JSON to the file of its day (in UTC) under
//! `/system/state/log`, so the log outlives the kernel's ring and the
//! boot. A record of Spark's process, number 1, whose text starts with a
//! name and `: ` is that service's line: the kernel sets the process, so
//! no other program can pass its lines off as a service's.
//!
//! Programs read the log through logd, never from the files: log(1)
//! sends a request, a line of JSON, to `/system/live/log/query` and
//! reads the records that match, a line of JSON each, until logd closes
//! the connection, or for as long as it follows the log.
//!
//! Started again, logd goes on from the last record of this boot it kept,
//! so no record is kept twice; a record the kernel's ring lost before
//! logd read it is counted in a record of logd's own.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::args::{Arg, Args};
use crate::dmesg;
use crate::proc;
use crate::util::{self, Fail, fail};

/// Where the log is kept.
pub const DIRECTORY: &str = "/system/state/log";
/// Where logd answers requests.
pub const QUERY: &str = "/system/live/log/query";
/// The boot's number, made by the first logd of each boot.
const BOOT_FILE: &str = "/system/live/log/boot";
/// The name of the day file logd writes to, for a logd started again to
/// find where it left off.
const WRITING: &str = "/system/live/log/writing";
/// Where the kernel log is read.
const KMSG: &str = "/devices/kmsg";
/// Spark's process, whose lines name the services.
const SPARK: u64 = 1;
/// How long written records wait, at most, before they are flushed to
/// the disk.
const SYNC_AFTER: Duration = Duration::from_secs(5);
/// The most requests answered at once; more are refused, so that their
/// threads fit in logd's memory.
const CLIENTS_MAX: usize = 32;
/// A day, in nanoseconds.
const DAY_NS: u64 = 86_400 * 1_000_000_000;

/// One record of the log, as a line of a day's file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// When it was made: RFC 3339, in UTC, to the microsecond.
    pub time: String,
    /// The boot it was made in.
    pub boot: String,
    /// Its number in the kernel's sequence of that boot.
    pub seq: u64,
    /// error, warning, info or debug.
    pub level: String,
    /// kernel, service or program.
    pub from: String,
    /// The service's name, or the program's; `kernel` for the kernel's.
    pub name: String,
    /// The process of the program that wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<u64>,
    /// What it says.
    pub text: String,
}

impl Record {
    /// The record of kernel log line `line` of boot `boot`, which started
    /// at `boot_ns` since the epoch.
    fn of(line: &dmesg::Line, boot: &str, boot_ns: u64) -> Record {
        let level = match line.level {
            0..=3 => "error",
            4 => "warning",
            7 => "debug",
            _ => "info",
        };
        let (from, name, process, text) = match line.process {
            // Spark's line: `name: text`, a service's or Spark's own.
            Some(SPARK) => {
                let text = after_stamp(&line.text);
                match text.split_once(": ") {
                    Some((name, rest)) if is_name(name) => {
                        ("service", name.to_string(), None, rest.to_string())
                    }
                    _ => ("service", "spark".to_string(), None, text.to_string()),
                }
            }
            Some(process) => {
                let (name, text) = stamp(&line.text);
                ("program", name.to_string(), Some(process), text.to_string())
            }
            None => ("kernel", "kernel".to_string(), None, line.text.clone()),
        };
        Record {
            time: rfc3339(boot_ns + line.usec * 1000),
            boot: boot.to_string(),
            seq: line.seq,
            level: level.to_string(),
            from: from.to_string(),
            name,
            process,
            text,
        }
    }

    /// A record of logd's own.
    fn own(text: String, boot: &str, seq: u64, level: &str) -> Record {
        Record {
            time: rfc3339(util::now_ns()),
            boot: boot.to_string(),
            seq,
            level: level.to_string(),
            from: "service".to_string(),
            name: "logd".to_string(),
            process: None,
            text,
        }
    }

    /// Its time, in nanoseconds since the epoch.
    pub fn time_ns(&self) -> u64 {
        parse_rfc3339(&self.time).unwrap_or(0)
    }

    /// Its level's number: 3 for errors to 7 for debugging.
    pub fn level_number(&self) -> u8 {
        level_number(&self.level).unwrap_or(6)
    }
}

/// A level's number by its name or syslog(3)'s: error 3, warning 4,
/// info 6, debug 7, and the names syslog(3) has besides.
pub fn level_number(name: &str) -> Option<u8> {
    Some(match name {
        "emerg" | "emergency" | "0" => 0,
        "alert" | "1" => 1,
        "crit" | "critical" | "2" => 2,
        "err" | "error" | "3" => 3,
        "warn" | "warning" | "4" => 4,
        "notice" | "5" => 5,
        "info" | "6" => 6,
        "debug" | "7" => 7,
        _ => return None,
    })
}

/// Whether `name` can be a service's: what Spark accepts for a service
/// file's name, without blanks or `:`.
fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'))
}

/// The text of a program's line after the kernel's `name[thread]: `.
fn after_stamp(text: &str) -> &str {
    stamp(text).1
}

/// The name the kernel put before a program's line, `name[thread]: ` from
/// the kernel log capability or `name: ` from `/devices/kmsg`, and the
/// text after it.
fn stamp(text: &str) -> (&str, &str) {
    match text.split_once(": ") {
        Some((head, rest)) => match head.split_once('[') {
            Some((name, thread)) if thread.ends_with(']') => (name, rest),
            _ => (head, rest),
        },
        None => ("?", text),
    }
}

/// The civil date of `days` since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// Days since 1970-01-01 of a civil date (`days_from_civil`).
pub fn days(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `ns` since the epoch as RFC 3339 in UTC, to the microsecond.
pub fn rfc3339(ns: u64) -> String {
    let secs = (ns / 1_000_000_000) as i64;
    let (year, month, day) = civil(secs.div_euclid(86_400));
    let rest = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        ns % 1_000_000_000 / 1000
    )
}

/// Nanoseconds since the epoch of an RFC 3339 time in UTC, as
/// [`rfc3339`] writes it.
pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    let (time, fraction) = time.split_once('.').unwrap_or((time, "0"));
    let mut time = time.splitn(3, ':');
    let h: u64 = time.next()?.parse().ok()?;
    let m: u64 = time.next()?.parse().ok()?;
    let s: u64 = time.next()?.parse().ok()?;
    let digits = fraction.len().min(9);
    let fraction: u64 = fraction.get(..digits)?.parse().ok()?;
    let ns = fraction * 10u64.pow(9 - digits as u32);
    let days = u64::try_from(days(year, month, day)).ok()?;
    Some(((days * 86_400 + h * 3600 + m * 60 + s) * 1_000_000_000) + ns)
}

/// The name of the file of the day `ns` since the epoch falls on.
fn day_file(ns: u64) -> String {
    let (year, month, day) = civil((ns / 1_000_000_000 / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}.jsonl")
}

/// The log's day files in `dir`, oldest first.
pub fn day_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(is_day_file))
        .collect();
    files.sort();
    files
}

/// This boot's number: the one logd made at its first start since boot,
/// or a new one, 16 random bytes in hexadecimal.
fn boot_number() -> io::Result<String> {
    if let Ok(text) = fs::read_to_string(BOOT_FILE) {
        let text = text.trim();
        if text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(text.to_string());
        }
    }
    let mut bytes = [0u8; 16];
    File::open("/devices/urandom")?.read_exact(&mut bytes)?;
    let number = bytes.iter().fold(String::new(), |mut text, b| {
        let _ = write!(text, "{b:02x}");
        text
    });
    fs::write(BOOT_FILE, format!("{number}\n"))?;
    Ok(number)
}

/// Whether `name` is a day file's: `YYYY-MM-DD.jsonl`.
fn is_day_file(name: &str) -> bool {
    name.len() == 16
        && name.as_bytes()[10..] == *b".jsonl"
        && name[..10].bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        })
}

/// The sequence number of the last record of boot `boot` the log in `dir`
/// keeps: the last of this boot's lines in day file `name`, the one a
/// logd of this boot wrote to last, which `WRITING` names.
fn last_kept(dir: &Path, name: &str, boot: &str) -> Option<u64> {
    if !is_day_file(name) {
        return None;
    }
    let mut file = File::open(dir.join(name)).ok()?;
    // Read back from the end, a piece at a time: the record looked for
    // is almost always the last line.
    let mut end = file.metadata().ok()?.len();
    let mut carried: Vec<u8> = Vec::new();
    while end > 0 {
        let start = end.saturating_sub(64 * 1024);
        let mut piece = vec![0u8; (end - start) as usize];
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut piece).ok()?;
        piece.extend_from_slice(&carried);
        // The piece's first line may have begun before it.
        let whole = match start {
            0 => 0,
            _ => piece.iter().position(|&b| b == b'\n').map_or(piece.len(), |at| at + 1),
        };
        for line in piece[whole..].split(|&b| b == b'\n').rev() {
            if let Ok(record) = serde_json::from_slice::<Record>(line)
                && record.boot == boot
            {
                return Some(record.seq);
            }
        }
        carried = piece[..whole].to_vec();
        end = start;
    }
    None
}

/// What the writer and the requests share: how many records were written,
/// which followers wait on.
struct Shared {
    written: Mutex<u64>,
    changed: Condvar,
}

/// Set by SIGTERM: logd flushes what it wrote and ends.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// The day file records are appended to.
struct Writer {
    dir: PathBuf,
    name: String,
    file: Option<File>,
    /// When the first record not flushed to the disk yet was written.
    dirty: Option<Instant>,
}

impl Writer {
    /// Appends `record` to the file of its day.
    fn append(&mut self, record: &Record) -> io::Result<()> {
        let name = day_file(record.time_ns());
        if self.file.is_none() || name != self.name {
            self.sync();
            self.file = None;
            let mut file = OpenOptions::new()
                .create(true)
                .read(true)
                .append(true)
                .mode(0o640)
                .open(self.dir.join(&name))?;
            // A line a logd that ended was writing is ended, so that the
            // next one stands on its own.
            let len = file.metadata()?.len();
            if len > 0 {
                let mut last = [0u8; 1];
                file.seek(SeekFrom::Start(len - 1))?;
                file.read_exact(&mut last)?;
                if last[0] != b'\n' {
                    file.write_all(b"\n")?;
                }
            }
            let _ = fs::write(WRITING, format!("{name}\n"));
            self.file = Some(file);
            self.name = name;
        }
        let mut line = serde_json::to_string(record).map_err(io::Error::other)?;
        line.push('\n');
        if let Some(file) = self.file.as_mut() {
            file.write_all(line.as_bytes())?;
            self.dirty.get_or_insert_with(Instant::now);
        }
        Ok(())
    }

    /// Flushes what was written to the disk.
    fn sync(&mut self) {
        if self.dirty.is_some()
            && let Some(file) = &self.file
        {
            let _ = file.sync_data();
        }
        self.dirty = None;
    }

    /// Flushes what was written to the disk if the first of it has waited
    /// long enough.
    fn sync_due(&mut self) {
        if self.dirty.is_some_and(|since| since.elapsed() >= SYNC_AFTER) {
            self.sync();
        }
    }
}

/// logd(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut dir = PathBuf::from(DIRECTORY);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Long(l) if l == "directory" => dir = PathBuf::from(args.value()?),
            other => return Err(args.unknown(&other)),
        }
    }
    // SAFETY: setting a signal's disposition touches no memory; no other
    // thread runs yet. A reader that goes away must not end logd: the
    // write to it fails instead.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    // SAFETY: all zeros is a valid sigaction: numbers, a signal set and a
    // function address.
    let mut action: libc::sigaction = unsafe { util::zeroed() };
    action.sa_sigaction = on_term as *const () as usize;
    // SAFETY: the handler only stores to an atomic.
    unsafe { libc::sigaction(libc::SIGTERM, &raw const action, std::ptr::null_mut()) };
    for path in [dir.as_path(), Path::new(QUERY).parent().unwrap_or(Path::new("/"))] {
        fs::create_dir_all(path).map_err(|e| util::failed(&path.display(), e))?;
    }
    let boot = boot_number().map_err(|e| util::failed(&BOOT_FILE, e))?;
    let boot_ns = proc::stat().map_or(0, |s| s.boot_time_ns);
    let mut kmsg = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(KMSG)
        .map_err(|e| util::failed(&KMSG, e))?;
    // None at logd's first start since boot.
    let after =
        fs::read_to_string(WRITING).ok().and_then(|name| last_kept(&dir, name.trim(), &boot));

    let _ = fs::remove_file(QUERY);
    let listener = UnixListener::bind(QUERY).map_err(|e| util::failed(&QUERY, e))?;
    let shared = Arc::new(Shared { written: Mutex::new(0), changed: Condvar::new() });
    let answering = Answering { dir: dir.clone(), boot: boot.clone(), boot_ns };
    {
        let shared = shared.clone();
        let answering = Arc::new(answering);
        thread::spawn(move || {
            static CLIENTS: AtomicUsize = AtomicUsize::new(0);
            for mut stream in listener.incoming().flatten() {
                if CLIENTS.fetch_add(1, Ordering::AcqRel) >= CLIENTS_MAX {
                    CLIENTS.fetch_sub(1, Ordering::AcqRel);
                    let _ = refuse(&mut stream, "logd is busy; try again".into());
                    continue;
                }
                let (shared, answering) = (shared.clone(), answering.clone());
                // Small stacks: an answer reads a line at a time.
                let spawned = thread::Builder::new().stack_size(256 * 1024).spawn(move || {
                    serve(&stream, &shared, &answering);
                    CLIENTS.fetch_sub(1, Ordering::AcqRel);
                });
                if spawned.is_err() {
                    CLIENTS.fetch_sub(1, Ordering::AcqRel);
                }
            }
        });
    }
    match after {
        Some(seq) => eprintln!("logd: keeping the log in {}, after record {seq}", dir.display()),
        None => eprintln!("logd: keeping the log in {}", dir.display()),
    }

    let mut writer = Writer { dir, name: String::new(), file: None, dirty: None };
    let mut buffer = vec![0u8; 4096];
    let mut next = after.map_or(0, |seq| seq + 1);
    let mut failing = false;
    loop {
        if STOP.load(Ordering::Relaxed) {
            writer.sync();
            return Ok(0);
        }
        match kmsg.read(&mut buffer) {
            Ok(0) => wait_readable(&kmsg, Duration::from_secs(1)),
            Ok(n) => {
                let Some(line) = dmesg::parse(&buffer[..n]) else { continue };
                if line.seq < next {
                    continue;
                }
                let mut records = Vec::with_capacity(2);
                if line.seq > next {
                    // Numbered as the last one lost, so that a logd started
                    // again goes on from the record after them.
                    let text = format!(
                        "{} records of the kernel log were lost before logd read them",
                        line.seq - next
                    );
                    records.push(Record::own(text, &boot, line.seq - 1, "warning"));
                }
                records.push(Record::of(&line, &boot, boot_ns));
                next = line.seq + 1;
                match records.iter().try_for_each(|record| writer.append(record)) {
                    Ok(()) => failing = false,
                    Err(err) => {
                        // Said once, not for each record: what logd says
                        // comes back to it as a record.
                        if !failing {
                            eprintln!("<3>logd: cannot write to {}: {err}", writer.dir.display());
                        }
                        failing = true;
                        writer.file = None;
                        continue;
                    }
                }
                *shared.written.lock().unwrap_or_else(PoisonError::into_inner) += 1;
                shared.changed.notify_all();
                writer.sync_due();
            }
            // Records were lost from the ring; the next one read shows
            // how many.
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait_readable(&kmsg, Duration::from_secs(1));
                writer.sync_due();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(util::failed(&KMSG, e)),
        }
    }
}

/// Waits up to `wait` for `file` to have something to read, or a signal.
fn wait_readable(file: &File, wait: Duration) {
    let mut entry = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd for the call.
    unsafe { libc::poll(&raw mut entry, 1, wait.as_millis() as libc::c_int) };
}

/// A request to logd, as log(1) sends it: a line of JSON.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    /// What it asks for: `records` (the default), `boots` or `usage`.
    pub ask: String,
    /// The boots: `all`, or the number of one, or how far back from this
    /// one (`0`, `-1`); this boot when empty.
    pub boot: String,
    /// Only these services, and the records of `names`.
    pub services: Vec<String>,
    /// Only these services or programs, by name (a program's is the name
    /// it gave its thread), and the records of `services`.
    pub names: Vec<String>,
    /// Only the kernel's own records.
    pub kernel: bool,
    /// Only records of this level or a more urgent one (its number).
    pub level: Option<u8>,
    /// Only records made at or after this time (nanoseconds since the epoch).
    pub since: Option<u64>,
    /// Only records made before this time.
    pub until: Option<u64>,
    /// Only records whose text an extended regular expression matches,
    /// ignoring case unless it has a capital letter, as journalctl's
    /// `--grep` does.
    pub grep: Option<String>,
    /// Only the last this many.
    pub lines: Option<usize>,
    /// Then new records as they come, until the connection closes.
    pub follow: bool,
}

/// What logd answers a request with when it cannot: a line of JSON.
#[derive(Serialize, Deserialize)]
pub struct Refusal {
    pub error: String,
}

/// A boot the log keeps records of, for `log --list-boots`.
#[derive(Serialize, Deserialize)]
pub struct Boot {
    pub boot: String,
    pub first: String,
    pub last: String,
    pub records: u64,
}

/// How much the log takes, for `log --disk-usage`.
#[derive(Serialize, Deserialize)]
pub struct Usage {
    pub bytes: u64,
    pub files: u64,
}

/// What every request is answered from.
struct Answering {
    dir: PathBuf,
    boot: String,
    /// When this boot started, in nanoseconds since the epoch.
    boot_ns: u64,
}

/// Sends `error` as the answer.
fn refuse(out: &mut impl Write, error: String) -> io::Result<()> {
    let mut line = serde_json::to_vec(&Refusal { error }).map_err(io::Error::other)?;
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()
}

/// Answers the request on `stream`.
fn serve(stream: &UnixStream, shared: &Shared, answering: &Answering) {
    let mut line = String::new();
    // A request is one short line; one that never ends is cut off.
    if BufReader::new(stream.take(64 * 1024)).read_line(&mut line).is_err() {
        return;
    }
    let mut out = io::BufWriter::new(stream);
    let request: Request = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(err) => {
            let _ = refuse(&mut out, format!("not a request: {err}"));
            return;
        }
    };
    let answered = match request.ask.as_str() {
        "" | "records" => records(&mut out, &request, shared, answering),
        "boots" => boots(&mut out, &answering.dir),
        "usage" => usage(&mut out, &answering.dir),
        other => Err(Answer::Refused(format!("logd does not know '{other}'"))),
    };
    match answered {
        Ok(()) => {
            let _ = out.flush();
        }
        Err(Answer::Refused(error)) => {
            let _ = refuse(&mut out, error);
        }
        Err(Answer::Gone) => {}
    }
}

/// Why a request was not answered in full.
enum Answer {
    /// It cannot be: the reason.
    Refused(String),
    /// The asker went away.
    Gone,
}

impl From<io::Error> for Answer {
    fn from(_: io::Error) -> Answer {
        Answer::Gone
    }
}

/// Calls `f` with each record of the day files `files`, in order, from
/// `start` bytes into the first; returns the file read last and where its
/// last whole line ends, for a follower to go on from.
fn each_record(
    files: &[PathBuf],
    start: u64,
    mut f: impl FnMut(Record) -> Result<(), Answer>,
) -> Result<Option<(PathBuf, u64)>, Answer> {
    let mut end = None;
    for (i, path) in files.iter().enumerate() {
        let Ok(mut file) = File::open(path) else { continue };
        let mut offset = if i == 0 { start } else { 0 };
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            // A line still being written is read once it is whole.
            if n == 0 || !line.ends_with('\n') {
                break;
            }
            offset += n as u64;
            if let Ok(record) = serde_json::from_str::<Record>(&line) {
                f(record)?;
            }
        }
        end = Some((path.clone(), offset));
    }
    Ok(end)
}

/// The boots the log in `dir` keeps, oldest first: each one's number,
/// first and last record and count.
fn list_boots(dir: &Path) -> Result<Vec<Boot>, Answer> {
    let mut boots: Vec<Boot> = Vec::new();
    each_record(&day_files(dir), 0, |record| {
        match boots.iter_mut().find(|b| b.boot == record.boot) {
            Some(boot) => {
                boot.last = record.time;
                boot.records += 1;
            }
            None => boots.push(Boot {
                boot: record.boot,
                first: record.time.clone(),
                last: record.time,
                records: 1,
            }),
        }
        Ok(())
    })?;
    boots.sort_by(|a, b| a.first.cmp(&b.first));
    Ok(boots)
}

/// The boot a request's `boot` names, or None for all of them.
fn which_boot(request: &Request, answering: &Answering) -> Result<Option<String>, Answer> {
    match request.boot.as_str() {
        "" | "0" => Ok(Some(answering.boot.clone())),
        "all" => Ok(None),
        offset if offset.starts_with('-') => {
            let back: usize = offset[1..]
                .parse()
                .map_err(|_| Answer::Refused(format!("'{offset}' is not a boot")))?;
            let boots = list_boots(&answering.dir)?;
            let at = boots.iter().position(|b| b.boot == answering.boot).unwrap_or(boots.len());
            at.checked_sub(back)
                .and_then(|i| boots.get(i))
                .map(|b| Some(b.boot.clone()))
                .ok_or_else(|| Answer::Refused(format!("the log keeps no boot {offset}")))
        }
        number if number.len() == 32 && number.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Ok(Some(number.to_ascii_lowercase()))
        }
        other => Err(Answer::Refused(format!("'{other}' is not a boot"))),
    }
}

/// The day files that can hold records `request` asks for: from the day
/// of `since`, or the day before this boot's for this boot's records (a
/// clock set back after boot can put them there), to the day of `until`.
fn files_for(request: &Request, boot: Option<&str>, answering: &Answering) -> Vec<PathBuf> {
    let mut from = request.since.map(day_file);
    if boot == Some(answering.boot.as_str()) {
        from = from.max(Some(day_file(answering.boot_ns.saturating_sub(DAY_NS))));
    }
    let to = request.until.map(day_file);
    day_files(&answering.dir)
        .into_iter()
        .filter(|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            from.as_deref().is_none_or(|from| name >= from)
                && to.as_deref().is_none_or(|to| name <= to)
        })
        .collect()
}

/// What a request for records chooses.
struct Choice<'a> {
    request: &'a Request,
    /// The boot (None: any).
    boot: Option<String>,
    grep: Option<util::Regex>,
}

impl Choice<'_> {
    /// Whether `record` is one asked for.
    fn wants(&self, record: &Record) -> bool {
        let request = self.request;
        self.boot.as_ref().is_none_or(|b| record.boot == *b)
            && (request.services.is_empty() && request.names.is_empty()
                || record.from == "service" && request.services.contains(&record.name)
                || request.names.contains(&record.name))
            && (!request.kernel || record.from == "kernel")
            && request.level.is_none_or(|level| record.level_number() <= level)
            && (request.since.is_none() && request.until.is_none() || {
                let time = record.time_ns();
                request.since.is_none_or(|since| time >= since)
                    && request.until.is_none_or(|until| time < until)
            })
            && self.grep.as_ref().is_none_or(|grep| grep.is_match(&record.text))
    }
}

/// Writes `record` as a line of JSON.
fn send(out: &mut impl Write, record: &Record) -> Result<(), Answer> {
    serde_json::to_writer(&mut *out, record).map_err(|_| Answer::Gone)?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Answers a request for records: those kept, then, to a follower, new
/// ones as they are written.
fn records(
    out: &mut io::BufWriter<&UnixStream>,
    request: &Request,
    shared: &Shared,
    answering: &Answering,
) -> Result<(), Answer> {
    let grep = match &request.grep {
        Some(pattern) => {
            let ignore_case = !pattern.chars().any(char::is_uppercase);
            Some(util::Regex::new(pattern, ignore_case).map_err(Answer::Refused)?)
        }
        None => None,
    };
    let choice = Choice { request, boot: which_boot(request, answering)?, grep };
    let files = files_for(request, choice.boot.as_deref(), answering);
    let mut seen = *shared.written.lock().unwrap_or_else(PoisonError::into_inner);
    // For the last few, count them first, then skip the rest: two reads
    // of the files, but no more memory however many are asked for.
    let mut skip = 0;
    if let Some(lines) = request.lines {
        let mut count = 0usize;
        each_record(&files, 0, |record| {
            count += usize::from(choice.wants(&record));
            Ok(())
        })?;
        skip = count.saturating_sub(lines);
    }
    let end = each_record(&files, 0, |record| {
        if choice.wants(&record) {
            if skip > 0 {
                skip -= 1;
            } else {
                send(out, &record)?;
            }
        }
        Ok(())
    })?;
    out.flush()?;
    if !request.follow {
        return Ok(());
    }
    let (mut path, mut offset) = end.unwrap_or_default();
    loop {
        {
            let written = shared.written.lock().unwrap_or_else(PoisonError::into_inner);
            let (written, _) = shared
                .changed
                .wait_timeout_while(written, Duration::from_secs(10), |w| *w == seen)
                .unwrap_or_else(PoisonError::into_inner);
            seen = *written;
        }
        // New records are in the file followed, or in newer ones.
        let newer: Vec<PathBuf> =
            day_files(&answering.dir).into_iter().filter(|f| *f >= path).collect();
        let start = if newer.first() == Some(&path) { offset } else { 0 };
        if let Some(end) = each_record(&newer, start, |record| {
            if choice.wants(&record) {
                send(out, &record)?;
            }
            Ok(())
        })? {
            (path, offset) = end;
        }
        out.flush()?;
        // A follower that went away is found by the next write; one that
        // waits long is looked for here.
        if peer_gone(out.get_ref()) {
            return Err(Answer::Gone);
        }
    }
}

/// Whether the other end of `stream` closed it.
fn peer_gone(stream: &UnixStream) -> bool {
    let mut entry = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd for the call.
    let ready = unsafe { libc::poll(&raw mut entry, 1, 0) };
    if ready <= 0 {
        return false;
    }
    // Readable: the end, or bytes a follower should not have sent, which
    // are dropped.
    let mut byte = [0u8; 64];
    // SAFETY: a valid buffer of its length.
    let got = unsafe {
        libc::recv(stream.as_raw_fd(), byte.as_mut_ptr().cast(), byte.len(), libc::MSG_DONTWAIT)
    };
    got == 0 || got < 0 && io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock
}

/// Answers a request for the boots the log keeps.
fn boots(out: &mut io::BufWriter<&UnixStream>, dir: &Path) -> Result<(), Answer> {
    for boot in list_boots(dir)? {
        serde_json::to_writer(&mut *out, &boot).map_err(|_| Answer::Gone)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Answers a request for the space the log takes.
fn usage(out: &mut io::BufWriter<&UnixStream>, dir: &Path) -> Result<(), Answer> {
    let files = day_files(dir);
    let bytes = files.iter().filter_map(|f| fs::metadata(f).ok()).map(|m| m.len()).sum();
    serde_json::to_writer(&mut *out, &Usage { bytes, files: files.len() as u64 })
        .map_err(|_| Answer::Gone)?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Sends `request` to logd and calls `each` with each line of the answer
/// until logd closes the connection or `each` says to stop.
pub fn ask(
    request: &Request,
    mut each: impl FnMut(&str) -> Result<bool, Fail>,
) -> Result<(), Fail> {
    let mut stream = UnixStream::connect(QUERY).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            fail(format!("logd is not running ({QUERY}: {})", util::strip_os_error(&e)))
        }
        _ => util::failed(&QUERY, e),
    })?;
    let mut line = serde_json::to_string(request).map_err(|e| fail(e.to_string()))?;
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| util::failed(&QUERY, e))?;
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|e| util::failed(&QUERY, e))?;
        if let Ok(refusal) = serde_json::from_str::<Refusal>(&line) {
            return Err(fail(refusal.error));
        }
        if !each(&line)? {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, process: Option<u64>, level: u8) -> dmesg::Line {
        dmesg::Line {
            level,
            user: process.is_some(),
            seq: 7,
            process,
            usec: 2_500_000,
            text: text.to_string(),
        }
    }

    #[test]
    fn times() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000000Z");
        let ns = 1_793_395_501_123_456_000;
        assert_eq!(rfc3339(ns), "2026-10-30T21:25:01.123456Z");
        assert_eq!(parse_rfc3339(&rfc3339(ns)), Some(ns));
        assert_eq!(parse_rfc3339("2000-02-29T12:00:00Z"), Some(951_825_600_000_000_000));
        assert_eq!(parse_rfc3339("2000-02-29T12:00:00.12345678éZ"), None);
        assert_eq!(day_file(ns), "2026-10-30.jsonl");
        for day in [-1, 0, 59, 365, 11_016, 20_756, 100_000] {
            let (y, m, d) = civil(day);
            assert_eq!(days(y, m, d), day);
        }
    }

    #[test]
    fn who_wrote_a_record() {
        let boot = "00112233445566778899aabbccddeeff";
        let service = Record::of(&line("spark[1]: volumed: watching", Some(1), 6), boot, 1000);
        assert_eq!((service.from.as_str(), service.name.as_str()), ("service", "volumed"));
        assert_eq!(service.text, "watching");
        assert_eq!(service.time, "1970-01-01T00:00:02.500001Z");
        let spark = Record::of(&line("spark[1]: spark: started", Some(1), 6), boot, 0);
        assert_eq!((spark.name.as_str(), spark.text.as_str()), ("spark", "started"));
        // A program that names itself after a service is still a program.
        let fake = Record::of(&line("volumed[9]: volumed: lies", Some(42), 3), boot, 0);
        assert_eq!((fake.from.as_str(), fake.name.as_str()), ("program", "volumed"));
        assert_eq!((fake.process, fake.level.as_str()), (Some(42), "error"));
        assert_eq!(fake.text, "volumed: lies");
        let written = Record::of(&line("logtest: marker", Some(5), 4), boot, 0);
        assert_eq!((written.name.as_str(), written.text.as_str()), ("logtest", "marker"));
        let kernel = Record::of(&line("ahci: port 0 up", None, 6), boot, 0);
        assert_eq!((kernel.from.as_str(), kernel.text.as_str()), ("kernel", "ahci: port 0 up"));
        let json = serde_json::to_string(&kernel).unwrap();
        assert!(!json.contains("process"));
        assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), kernel);
    }

    #[test]
    fn requests_choose() {
        let boot = "b";
        let record = Record::of(&line("spark[1]: volumed: up 2 disks", Some(1), 4), boot, 0);
        let mut request = Request::default();
        fn choose<'a>(request: &'a Request, boot: Option<&str>, grep: Option<&str>) -> Choice<'a> {
            Choice {
                request,
                boot: boot.map(str::to_string),
                grep: grep.map(|g| util::Regex::new(g, true).unwrap()),
            }
        }
        assert!(choose(&request, Some(boot), None).wants(&record));
        assert!(!choose(&request, Some("other"), None).wants(&record));
        assert!(choose(&request, None, Some("UP [0-9]")).wants(&record));
        assert!(!choose(&request, None, Some("down")).wants(&record));
        request.names = vec!["netd".into()];
        assert!(!choose(&request, None, None).wants(&record));
        request.services = vec!["volumed".into()];
        assert!(choose(&request, None, None).wants(&record));
        // A program that named its thread after a service is not it.
        let program = Record { from: "program".into(), process: Some(9), ..record.clone() };
        assert!(!choose(&request, None, None).wants(&program));
        request.services.clear();
        request.names = vec!["volumed".into()];
        assert!(choose(&request, None, None).wants(&program));
        request.level = Some(3);
        assert!(!choose(&request, None, None).wants(&record));
        request.level = Some(4);
        assert!(choose(&request, None, None).wants(&record));
        request.until = Some(1);
        assert!(!choose(&request, None, None).wants(&record));
        request.until = None;
        request.kernel = true;
        assert!(!choose(&request, None, None).wants(&record));
        assert!(serde_json::from_str::<Request>(r#"{"bogus":1}"#).is_err());
    }

    #[test]
    fn going_on_after_a_restart() {
        let dir = std::env::temp_dir().join(format!("logd-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let name = "2026-10-30.jsonl";
        let mut text = String::new();
        let mut seq = 0;
        // Enough lines that the last of this boot's is read back from
        // pieces, after another boot's, with a line cut short at the end.
        for boot in ["old", "this"] {
            for _ in 0..2000 {
                let record = Record::of(&line("ahci: up", None, 6), boot, 0);
                text.push_str(&serde_json::to_string(&Record { seq, ..record }).unwrap());
                text.push('\n');
                seq += 1;
            }
        }
        text.push_str(r#"{"time":"2026-10-30T"#);
        fs::write(dir.join(name), &text).unwrap();
        assert_eq!(last_kept(&dir, name, "this"), Some(3999));
        assert_eq!(last_kept(&dir, name, "old"), Some(1999));
        assert_eq!(last_kept(&dir, name, "none"), None);
        assert_eq!(last_kept(&dir, "../x.jsonl", "this"), None);
        // The line cut short is ended before the next is appended.
        let mut writer = Writer { dir: dir.clone(), name: String::new(), file: None, dirty: None };
        let record = Record::of(&line("ahci: up", None, 6), "this", 1_793_395_501_000_000_000);
        writer.append(&Record { seq: 4000, ..record }).unwrap();
        writer.sync();
        assert_eq!(writer.name, name);
        assert_eq!(last_kept(&dir, name, "this"), Some(4000));
        let text = fs::read_to_string(dir.join(name)).unwrap();
        let last = r#"{"time":"2026-10-30T21:25:03.500000Z","boot":"this","seq":4000,"level":"info","from":"kernel","name":"kernel","text":"ahci: up"}"#;
        assert!(text.ends_with(&format!("T\n{last}\n")));
        fs::remove_dir_all(&dir).unwrap();
        assert!(is_day_file(name) && !is_day_file("2026-10-3x.jsonl"));
    }
}
