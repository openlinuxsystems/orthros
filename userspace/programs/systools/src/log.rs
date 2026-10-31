// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! log(1): the system's log, as logd(8) keeps it (ADR 0210): the
//! records of this boot, or of others, of some services or programs,
//! of a level, between two times, matching a pattern, the last few, and
//! new ones as they come.
//!
//! journalctl(1) is the same, taking journalctl's options and keeping
//! its defaults (every boot unless `-b` or `-k`), for the scripts and the
//! habits that come from Linux: `-u` names a service (`.service` may
//! follow it), as `-t` names a program. `--audit`, or journalctl's
//! `_TRANSPORT=audit`, reads the audit log's security events (ADR 0211),
//! and journalctl's `--vacuum-size` and `--vacuum-time` trim the log as
//! logtrim(8) does.
//!
//! Root reads every record; anyone else, only the records of their own
//! programs, as logd(8) answers them (ADR 0252), and is told so unless
//! `-q`. An administrator reads the rest with `elevate log`.

use std::ffi::OsStr;
use std::io::{self, Write};

use crate::args::{Arg, Args};
use crate::logd::{self, Boot, Record, Request, Usage};
use crate::logtrim;
use crate::util::{self, Fail, fail, usage};

/// How each record is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Output {
    /// `Oct 30 21:25:01 host name[process]: text`.
    Short,
    /// The time as ISO 8601, with its offset from UTC.
    ShortIso,
    /// The time to the microsecond.
    ShortPrecise,
    /// The text alone.
    Cat,
    /// The record as logd keeps it, a line of JSON.
    Json,
    /// The same, spread over lines.
    JsonPretty,
}

impl Output {
    fn named(name: &str) -> Result<Output, Fail> {
        Ok(match name {
            "short" => Output::Short,
            "short-iso" => Output::ShortIso,
            "short-precise" => Output::ShortPrecise,
            "cat" => Output::Cat,
            "json" => Output::Json,
            "json-pretty" => Output::JsonPretty,
            _ => {
                return Err(usage(format!(
                    "unknown output '{name}': short, short-iso, short-precise, cat, json or \
                     json-pretty"
                )));
            }
        })
    }

    fn is_json(self) -> bool {
        matches!(self, Output::Json | Output::JsonPretty)
    }
}

/// What is asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Records,
    ListBoots,
    DiskUsage,
    /// journalctl's `--vacuum-*`.
    Vacuum,
}

/// What one of journalctl's matches, `FIELD=value`, picks.
#[derive(Debug, PartialEq, Eq)]
enum Match {
    /// A service's records (`_SYSTEMD_UNIT=`, `UNIT=`).
    Service(String),
    /// A service's or a program's, by name (`SYSLOG_IDENTIFIER=`,
    /// `_COMM=`).
    Name(String),
    /// The audit log's (`_TRANSPORT=audit`).
    Audit,
}

/// How the records are shown.
struct Show {
    output: Output,
    utc: bool,
    /// The host's name, before each record; none without it.
    host: Option<String>,
    /// Errors in red and warnings in yellow.
    color: bool,
}

/// log(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    run(args, false)
}

/// journalctl(1).
pub fn journalctl(args: &mut Args) -> Result<i32, Fail> {
    run(args, true)
}

/// Whether `value` names a boot: `all`, an offset (`0`, `-1`) or a
/// boot's number.
fn is_boot(value: &OsStr) -> bool {
    let value = value.to_string_lossy();
    value == "all"
        || value.strip_prefix('-').unwrap_or(&value).bytes().all(|b| b.is_ascii_digit())
            && !value.is_empty()
            && value != "-"
        || value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A level by name or number, or journalctl's range of them
/// (`err..warning`), of which the less urgent end counts.
fn level(text: &str) -> Result<u8, Fail> {
    let one = |name: &str| {
        logd::level_number(name).ok_or_else(|| {
            usage(format!("unknown level '{name}': error, warning, info, debug or 0-7"))
        })
    };
    match text.split_once("..") {
        Some((from, to)) => Ok(one(from)?.max(one(to)?)),
        None => one(text),
    }
}

/// A service's name, as journalctl's `-u` takes it, without `.service`.
fn unit(name: &str) -> String {
    name.strip_suffix(".service").unwrap_or(name).to_string()
}

fn run(args: &mut Args, journalctl: bool) -> Result<i32, Fail> {
    let mut request = Request::default();
    let mut boot: Option<String> = None;
    // How many of the last to show (None: all of them), if asked.
    let mut lines: Option<Option<usize>> = None;
    let mut pager_end = false;
    let mut since: Option<String> = None;
    let mut until: Option<String> = None;
    let mut output = Output::Short;
    let mut reverse = false;
    let mut utc = false;
    let mut hostname = true;
    let mut mode = Mode::Records;
    let mut quiet = false;
    let mut vacuum = Request { ask: "trim".into(), ..Request::default() };
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('f') => request.follow = true,
            Arg::Long(l) if l == "follow" => request.follow = true,
            Arg::Short('n') => lines = Some(lines_value(args, journalctl)?),
            Arg::Long(l) if l == "lines" => lines = Some(lines_value(args, journalctl)?),
            Arg::Short('s') if !journalctl => request.services.push(args.text()?),
            Arg::Long(l) if l == "service" && !journalctl => request.services.push(args.text()?),
            Arg::Short('u') if journalctl => request.services.push(unit(&args.text()?)),
            Arg::Long(l) if l == "unit" && journalctl => {
                request.services.push(unit(&args.text()?));
            }
            Arg::Short('t') if journalctl => request.names.push(args.text()?),
            Arg::Long(l) if l == "identifier" && journalctl => request.names.push(args.text()?),
            Arg::Short('k') => request.kernel = true,
            Arg::Long(l) if l == if journalctl { "dmesg" } else { "kernel" } => {
                request.kernel = true;
            }
            Arg::Short('p') => request.level = Some(level(&args.text()?)?),
            Arg::Long(l) if l == "priority" => request.level = Some(level(&args.text()?)?),
            Arg::Short('S') => since = Some(args.text()?),
            Arg::Long(l) if l == "since" => since = Some(args.text()?),
            Arg::Short('U') => until = Some(args.text()?),
            Arg::Long(l) if l == "until" => until = Some(args.text()?),
            Arg::Short('g') => request.grep = Some(args.text()?),
            Arg::Long(l) if l == "grep" => request.grep = Some(args.text()?),
            Arg::Short('b') => boot = Some(boot_value(args)),
            Arg::Long(l) if l == "boot" => boot = Some(boot_value(args)),
            Arg::Long(l) if l == "all" && !journalctl => boot = Some("all".into()),
            Arg::Short('r') => reverse = true,
            Arg::Long(l) if l == "reverse" => reverse = true,
            Arg::Short('o') => output = Output::named(&args.text()?)?,
            Arg::Long(l) if l == "output" => output = Output::named(&args.text()?)?,
            Arg::Long(l) if l == "json" && !journalctl => output = Output::Json,
            Arg::Long(l) if l == "utc" => utc = true,
            Arg::Long(l) if l == "no-hostname" => hostname = false,
            Arg::Long(l) if l == "list-boots" => mode = Mode::ListBoots,
            Arg::Long(l) if l == "disk-usage" => mode = Mode::DiskUsage,
            Arg::Long(l) if l == "audit" && !journalctl => request.audit = true,
            Arg::Long(l) if l == "vacuum-size" && journalctl => {
                vacuum.keep_bytes = Some(logtrim::size(&args.text()?)?);
                mode = Mode::Vacuum;
            }
            Arg::Long(l) if l == "vacuum-time" && journalctl => {
                vacuum.keep_days = Some(span_days(&args.text()?)?);
                mode = Mode::Vacuum;
            }
            Arg::Long(l) if l == "no-pager" => {}
            // journalctl's: the end of the log in a pager, which is the
            // last thousand here; and the ones that change nothing here.
            Arg::Short('e') if journalctl => pager_end = true,
            Arg::Long(l) if l == "pager-end" && journalctl => pager_end = true,
            Arg::Short('q') => quiet = true,
            Arg::Long(l) if l == "quiet" => quiet = true,
            Arg::Short('a' | 'l' | 'm' | 'x') if journalctl => {}
            Arg::Long(l)
                if journalctl && ["all", "full", "merge", "catalog", "system"].contains(&&*l) => {}
            Arg::Operand(operand) => {
                let operand = operand.to_string_lossy().into_owned();
                match journalctl.then(|| field_match(&operand)).transpose()? {
                    None => request.names.push(operand),
                    Some(Match::Service(service)) => request.services.push(service),
                    Some(Match::Name(name)) => request.names.push(name),
                    Some(Match::Audit) => request.audit = true,
                }
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let now = util::now_ns();
    request.since = since.map(|t| parse_time(&t, now, utc)).transpose()?;
    request.until = until.map(|t| parse_time(&t, now, utc)).transpose()?;
    request.boot = match boot {
        Some(boot) => boot,
        // journalctl reads every boot unless asked for the kernel's
        // records; log(1), this one.
        None if journalctl && !request.kernel => "all".into(),
        None => String::new(),
    };
    request.lines = match lines {
        Some(lines) => lines,
        None if pager_end => Some(1000),
        None if request.follow => Some(10),
        None => None,
    };
    if reverse && request.follow {
        return Err(usage("--reverse and --follow cannot go together".into()));
    }
    match mode {
        Mode::Records => {}
        Mode::ListBoots => return list_boots(utc),
        Mode::DiskUsage => return disk_usage(journalctl),
        Mode::Vacuum => {
            logtrim::report(&logtrim::ask(&vacuum)?, false, true);
            return Ok(0);
        }
    }

    // As journalctl's hint to a user outside its groups (ADR 0252).
    if util::own_user() != 0 && !quiet {
        eprintln!(
            "Hint: you see only the records of your own programs. An administrator sees all of \
             them with: elevate {}",
            if journalctl { "journalctl" } else { "log" }
        );
    }
    let show =
        Show { output, utc, host: hostname.then(util::hostname), color: util::is_terminal(1) };
    let out = io::stdout();
    let mut out = io::BufWriter::new(out.lock());
    let mut shown = 0u64;
    let mut held: Vec<String> = Vec::new();
    let mut last_boot: Option<String> = None;
    let many_boots = request.boot == "all";
    let follow = request.follow;
    logd::ask(&request, |line| {
        if reverse {
            held.push(line.to_string());
            return Ok(true);
        }
        let Ok(record) = serde_json::from_str::<Record>(line) else { return Ok(true) };
        if many_boots {
            separate_boots(&mut out, &mut last_boot, &record, show.output)?;
        }
        write_record(&mut out, &show, &record, line)?;
        shown += 1;
        if follow {
            out.flush()?;
        }
        Ok(true)
    })?;
    for line in held.iter().rev() {
        let Ok(record) = serde_json::from_str::<Record>(line) else { continue };
        if many_boots {
            separate_boots(&mut out, &mut last_boot, &record, show.output)?;
        }
        write_record(&mut out, &show, &record, line)?;
        shown += 1;
    }
    if shown == 0 && !show.output.is_json() {
        writeln!(out, "-- No entries --")?;
    }
    out.flush()?;
    Ok(0)
}

/// The value of `-n`: how many of the last records to show, or None for
/// `all`. journalctl's `-n` alone is the last ten.
fn lines_value(args: &mut Args, journalctl: bool) -> Result<Option<usize>, Fail> {
    let value = if journalctl {
        args.optional_value_where(|v| {
            v == "all" || v.as_encoded_bytes().iter().all(u8::is_ascii_digit)
        })
    } else {
        Some(args.value()?)
    };
    match value.as_ref().map(|v| v.to_string_lossy()) {
        None => Ok(Some(10)),
        Some(v) if v == "all" => Ok(None),
        Some(v) => {
            v.parse().map(Some).map_err(|_| usage(format!("'{v}' is not a number of lines")))
        }
    }
}

/// The value of `-b`: the boot it names, or this one.
fn boot_value(args: &mut Args) -> String {
    args.optional_value_where(is_boot)
        .map_or_else(|| "0".to_string(), |v| v.to_string_lossy().into_owned())
}

/// What one of journalctl's matches, `FIELD=value`, picks, of the fields
/// that name a service or a program, or the audit log.
fn field_match(text: &str) -> Result<Match, Fail> {
    match text.split_once('=') {
        Some(("_SYSTEMD_UNIT" | "UNIT", value)) => Ok(Match::Service(unit(value))),
        Some(("SYSLOG_IDENTIFIER" | "_COMM", value)) => Ok(Match::Name(value.to_string())),
        Some(("_TRANSPORT", "audit")) => Ok(Match::Audit),
        _ => Err(usage(format!(
            "'{text}': the log is searched by _SYSTEMD_UNIT=, SYSLOG_IDENTIFIER=, _COMM= or \
             _TRANSPORT=audit here"
        ))),
    }
}

/// The whole days of a time span as journalctl's `--vacuum-time` takes
/// it: `2weeks`, `10d`, `6months`, `1y`.
fn span_days(text: &str) -> Result<u64, Fail> {
    let bad = || usage(format!("'{text}' is not a time span"));
    let digits = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let number: u64 = text[..digits].parse().map_err(|_| bad())?;
    let seconds: u64 = match text[digits..].trim() {
        "" | "s" | "sec" | "second" | "seconds" => 1,
        "m" | "min" | "minute" | "minutes" => 60,
        "h" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "week" | "weeks" => 7 * 86_400,
        "M" | "month" | "months" => 2_629_800,
        "y" | "year" | "years" => 31_557_600,
        _ => return Err(bad()),
    };
    Ok(number.saturating_mul(seconds) / 86_400)
}

/// Writes journalctl's `-- Boot ... --` line before the first record of
/// each boot after the first.
fn separate_boots(
    out: &mut impl Write,
    last: &mut Option<String>,
    record: &Record,
    output: Output,
) -> io::Result<()> {
    if last.as_deref() != Some(record.boot.as_str()) {
        if last.is_some() && !output.is_json() {
            writeln!(out, "-- Boot {} --", record.boot)?;
        }
        *last = Some(record.boot.clone());
    }
    Ok(())
}

/// Writes `record`, which logd sent as `line`, as `show` asks.
fn write_record(
    out: &mut impl Write,
    show: &Show,
    record: &Record,
    line: &str,
) -> Result<(), Fail> {
    match show.output {
        Output::Json => writeln!(out, "{line}")?,
        Output::JsonPretty => {
            let text = serde_json::to_string_pretty(record).map_err(|e| fail(e.to_string()))?;
            writeln!(out, "{text}")?;
        }
        Output::Cat => writeln!(out, "{}", record.text)?,
        Output::Short | Output::ShortIso | Output::ShortPrecise => {
            let ns = record.time_ns();
            let seconds = (ns / 1_000_000_000) as i64;
            let time = match show.output {
                Output::ShortIso => util::time_text(seconds, "%Y-%m-%dT%H:%M:%S%z", show.utc),
                Output::ShortPrecise => format!(
                    "{}.{:06}",
                    util::time_text(seconds, "%b %d %H:%M:%S", show.utc),
                    ns % 1_000_000_000 / 1000
                ),
                _ => util::time_text(seconds, "%b %d %H:%M:%S", show.utc),
            };
            let who = match record.process {
                Some(process) => format!("{}[{process}]", record.name),
                None => record.name.clone(),
            };
            let (start, end) = match record.level_number() {
                _ if !show.color => ("", ""),
                0..=3 => ("\x1b[1;31m", "\x1b[0m"),
                4 => ("\x1b[1;33m", "\x1b[0m"),
                5 => ("\x1b[1m", "\x1b[0m"),
                _ => ("", ""),
            };
            match &show.host {
                Some(host) => writeln!(out, "{time} {host} {who}: {start}{}{end}", record.text)?,
                None => writeln!(out, "{time} {who}: {start}{}{end}", record.text)?,
            }
        }
    }
    Ok(())
}

/// `log --list-boots`: the boots the log keeps, as journalctl lists them,
/// numbered back from this one.
fn list_boots(utc: bool) -> Result<i32, Fail> {
    let mut boots: Vec<Boot> = Vec::new();
    let request = Request { ask: "boots".into(), ..Request::default() };
    logd::ask(&request, |line| {
        if let Ok(boot) = serde_json::from_str::<Boot>(line) {
            boots.push(boot);
        }
        Ok(true)
    })?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    writeln!(out, "IDX BOOT ID                          FIRST ENTRY                 LAST ENTRY")?;
    let count = boots.len();
    let time = |text: &str| {
        let seconds = (logd::parse_rfc3339(text).unwrap_or(0) / 1_000_000_000) as i64;
        util::time_text(seconds, "%a %Y-%m-%d %H:%M:%S %Z", utc)
    };
    for (i, boot) in boots.iter().enumerate() {
        let index = i as i64 - (count as i64 - 1);
        writeln!(out, "{index:>3} {} {} {}", boot.boot, time(&boot.first), time(&boot.last))?;
    }
    out.flush()?;
    Ok(0)
}

/// `log --disk-usage`: the space the log takes.
fn disk_usage(journalctl: bool) -> Result<i32, Fail> {
    let mut usage: Option<Usage> = None;
    let request = Request { ask: "usage".into(), ..Request::default() };
    logd::ask(&request, |line| {
        usage = serde_json::from_str(line).ok();
        Ok(false)
    })?;
    let usage = usage.ok_or_else(|| fail("logd did not say".into()))?;
    let size = util::human(usage.bytes, false);
    if journalctl {
        println!("Archived and active journals take up {size} in the file system.");
    } else {
        let files = if usage.files == 1 { "file" } else { "files" };
        let audit = util::human(usage.audit_bytes, false);
        println!(
            "The log takes up {size} in {} {files} in {}, {audit} of it the audit log.",
            usage.files,
            logd::DIRECTORY
        );
    }
    Ok(0)
}

/// A time as `--since` and `--until` take it, in nanoseconds since the
/// epoch, as journalctl takes it: `now`, `today`, `yesterday`,
/// `tomorrow`; `-1h`, `+30min`, `2 days ago`; `YYYY-MM-DD [HH:MM[:SS]]`
/// or `HH:MM[:SS]` (today), in local time unless `utc` or it ends with
/// ` UTC` or `Z`.
pub fn parse_time(text: &str, now: u64, utc: bool) -> Result<u64, Fail> {
    let bad = || usage(format!("'{text}' is not a time"));
    let text = text.trim();
    let midnight = |days_from_today: i64| -> Result<u64, Fail> {
        let seconds = (now / 1_000_000_000) as i64;
        let today = if utc {
            seconds - seconds.rem_euclid(86_400)
        } else {
            let (year, month, day) = local_date(seconds).ok_or_else(bad)?;
            local_seconds(year, month, day, 0, 0, 0).ok_or_else(bad)?
        };
        // A day later or earlier at midnight: noon, then back to midnight,
        // so a change of daylight saving time between is no matter.
        let noon = today + 12 * 3600 + days_from_today * 86_400;
        let start = if utc {
            noon - noon.rem_euclid(86_400)
        } else {
            let (year, month, day) = local_date(noon).ok_or_else(bad)?;
            local_seconds(year, month, day, 0, 0, 0).ok_or_else(bad)?
        };
        seconds_ns(start).ok_or_else(bad)
    };
    match text {
        "now" => return Ok(now),
        "today" => return midnight(0),
        "yesterday" => return midnight(-1),
        "tomorrow" => return midnight(1),
        _ => {}
    }
    if let Some(offset) = relative(text) {
        let ns = i128::from(now).checked_add(offset).ok_or_else(bad)?;
        return u64::try_from(ns.max(0)).map_err(|_| bad());
    }
    let (text, utc) = match text.strip_suffix(" UTC").or_else(|| text.strip_suffix('Z')) {
        Some(rest) => (rest, true),
        None => (text, utc),
    };
    let (date, time) = match text.split_once([' ', 'T']) {
        Some((date, time)) => (Some(date), Some(time)),
        None if text.contains(':') => (None, Some(text)),
        None => (Some(text), None),
    };
    let (hour, minute, second) = match time {
        Some(time) => {
            let mut parts = time.splitn(3, ':');
            let mut part = || -> Result<u32, Fail> {
                parts.next().map_or(Ok(0), |p| p.parse().map_err(|_| bad()))
            };
            (part()?, part()?, part()?)
        }
        None => (0, 0, 0),
    };
    if hour > 23 || minute > 59 || second > 60 {
        return Err(bad());
    }
    let (year, month, day) = match date {
        Some(date) => {
            let mut parts = date.splitn(3, '-');
            let mut part = || -> Result<i64, Fail> {
                parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())
            };
            let (year, month, day) = (part()?, part()?, part()?);
            // Years whose days count without overflowing.
            if !(0..=9999).contains(&year) {
                return Err(bad());
            }
            let month =
                u32::try_from(month).ok().filter(|m| (1..=12).contains(m)).ok_or_else(bad)?;
            let day = u32::try_from(day).ok().filter(|d| (1..=31).contains(d)).ok_or_else(bad)?;
            (year, month, day)
        }
        None => {
            let seconds = (now / 1_000_000_000) as i64;
            if utc {
                logd::civil(seconds.div_euclid(86_400))
            } else {
                local_date(seconds).ok_or_else(bad)?
            }
        }
    };
    let seconds = if utc {
        logd::days(year, month, day) * 86_400
            + i64::from(hour) * 3600
            + i64::from(minute) * 60
            + i64::from(second)
    } else {
        local_seconds(year, month, day, hour, minute, second).ok_or_else(bad)?
    };
    seconds_ns(seconds).ok_or_else(bad)
}

/// `seconds` since the epoch in nanoseconds, if it is after the epoch and
/// before 2554, when nanoseconds overflow.
fn seconds_ns(seconds: i64) -> Option<u64> {
    u64::try_from(seconds).ok()?.checked_mul(1_000_000_000)
}

/// A time relative to now, in nanoseconds: `-1h`, `+30min`, `2 days ago`;
/// a number alone is seconds.
fn relative(text: &str) -> Option<i128> {
    let (sign, rest) = if let Some(rest) = text.strip_prefix('-') {
        (-1, rest)
    } else if let Some(rest) = text.strip_prefix('+') {
        (1, rest)
    } else {
        (-1, text.strip_suffix(" ago")?)
    };
    let rest = rest.trim();
    let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let number: i128 = rest[..digits].parse().ok()?;
    let seconds: i128 = match rest[digits..].trim() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "week" | "weeks" => 7 * 86_400,
        _ => return None,
    };
    number.checked_mul(seconds * 1_000_000_000).map(|ns| sign * ns)
}

/// The local date of `seconds` since the epoch.
fn local_date(seconds: i64) -> Option<(i64, u32, u32)> {
    let time = seconds as libc::time_t;
    // SAFETY: an all-zero tm is valid; localtime_r fills it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    if unsafe { libc::localtime_r(&raw const time, &raw mut tm) }.is_null() {
        return None;
    }
    Some((i64::from(tm.tm_year) + 1900, (tm.tm_mon + 1) as u32, tm.tm_mday as u32))
}

/// Seconds since the epoch of a local date and time.
fn local_seconds(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<i64> {
    // SAFETY: an all-zero tm is valid.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = i32::try_from(year - 1900).ok()?;
    tm.tm_mon = month as i32 - 1;
    tm.tm_mday = day as i32;
    tm.tm_hour = hour as i32;
    tm.tm_min = minute as i32;
    tm.tm_sec = second as i32;
    // Whether daylight saving time is in effect then, mktime finds.
    tm.tm_isdst = -1;
    // SAFETY: `tm` is valid for the call.
    let seconds = unsafe { libc::mktime(&raw mut tm) };
    (seconds != -1).then_some(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_793_395_501_123_456_000; // 2026-10-30T21:25:01.123456Z

    #[test]
    fn times() {
        let at = |text: &str| parse_time(text, NOW, true).unwrap();
        assert_eq!(at("now"), NOW);
        assert_eq!(at("today"), 1_793_318_400_000_000_000);
        assert_eq!(at("yesterday"), 1_793_318_400_000_000_000 - 86_400_000_000_000);
        assert_eq!(at("tomorrow"), 1_793_318_400_000_000_000 + 86_400_000_000_000);
        assert_eq!(at("-1h"), NOW - 3_600_000_000_000);
        assert_eq!(at("+30min"), NOW + 1_800_000_000_000);
        assert_eq!(at("2 days ago"), NOW - 2 * 86_400_000_000_000);
        assert_eq!(at("2026-10-30"), 1_793_318_400_000_000_000);
        assert_eq!(at("2026-10-30 21:25"), 1_793_395_500_000_000_000);
        assert_eq!(at("2026-10-30T21:25:01Z"), 1_793_395_501_000_000_000);
        assert_eq!(parse_time("2026-10-30 21:25:01 UTC", NOW, false).unwrap(), at("21:25:01"));
        // Past what nanoseconds since the epoch hold, refused.
        let huge = "99999999999999999999999999999999999 days ago";
        for bad in ["soon", "2026-13-01", "25:00", "-1fortnight", "2026-10", "2600-01-01", huge] {
            assert!(parse_time(bad, NOW, true).is_err(), "{bad}");
        }
    }

    #[test]
    fn options() {
        assert_eq!(level("err").unwrap(), 3);
        assert_eq!(level("warning").unwrap(), 4);
        assert_eq!(level("err..warning").unwrap(), 4);
        assert!(level("loud").is_err());
        assert_eq!(unit("volumed.service"), "volumed");
        assert_eq!(
            field_match("_SYSTEMD_UNIT=logd.service").unwrap(),
            Match::Service("logd".into())
        );
        assert_eq!(field_match("_COMM=nukesh").unwrap(), Match::Name("nukesh".into()));
        assert_eq!(field_match("_TRANSPORT=audit").unwrap(), Match::Audit);
        assert!(field_match("_PID=1").is_err() && field_match("_TRANSPORT=kernel").is_err());
        assert_eq!(span_days("2weeks").unwrap(), 14);
        assert_eq!(span_days("6months").unwrap(), 182);
        assert_eq!(span_days("1y").unwrap(), 365);
        assert_eq!(span_days("12h").unwrap(), 0);
        assert!(span_days("soon").is_err() && span_days("3fortnights").is_err());
        for boot in ["all", "0", "-1", "3", "00112233445566778899aabbccddeeff"] {
            assert!(is_boot(OsStr::new(boot)), "{boot}");
        }
        for not in ["-f", "-", "", "x", "--follow"] {
            assert!(!is_boot(OsStr::new(not)), "{not}");
        }
    }

    #[test]
    fn records_shown() {
        let record = Record {
            time: "2026-10-30T21:25:01.123456Z".into(),
            boot: "b".into(),
            seq: 3,
            level: "error".into(),
            from: "program".into(),
            name: "logtest".into(),
            process: Some(42),
            user: Some(1001),
            audit: false,
            text: "marker".into(),
        };
        let show = |output| Show { output, utc: true, host: Some("box".into()), color: false };
        let shown = |output| {
            let mut out = Vec::new();
            write_record(&mut out, &show(output), &record, "{line}").unwrap();
            String::from_utf8(out).unwrap()
        };
        assert_eq!(shown(Output::Short), "Oct 30 21:25:01 box logtest[42]: marker\n");
        assert_eq!(shown(Output::ShortPrecise), "Oct 30 21:25:01.123456 box logtest[42]: marker\n");
        assert_eq!(shown(Output::ShortIso), "2026-10-30T21:25:01+0000 box logtest[42]: marker\n");
        assert_eq!(shown(Output::Cat), "marker\n");
        assert_eq!(shown(Output::Json), "{line}\n");
        let mut out = Vec::new();
        let mut last = None;
        separate_boots(&mut out, &mut last, &record, Output::Short).unwrap();
        separate_boots(
            &mut out,
            &mut last,
            &Record { boot: "c".into(), ..record.clone() },
            Output::Short,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "-- Boot c --\n");
    }
}
