// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! last(1), lastb(1) and lastlog(8): the sign-in records authd(8) keeps
//! (ADR 0254, `accounts::signins`), shown as util-linux's last and lastb
//! show wtmp and btmp, and shadow's lastlog its file.
//!
//! Everyone reads their own account's records and the computer's starts
//! and stops; root reads everyone's, and the wrong passwords typed for
//! names that are no account's, so an administrator runs `elevate last`.

use std::fs;
use std::io::{self, Write};

use accounts::User;
use accounts::signins::{self, End, Line, Record};

use crate::args::{Arg, Args};
use crate::util::{self, Fail, fail, usage};
use crate::{log, proc, sessions};

/// How times are shown, as util-linux's `--time-format` names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Times {
    None,
    Short,
    Full,
    Iso,
}

impl Times {
    fn parse(text: &str) -> Result<Times, Fail> {
        match text {
            "notime" => Ok(Times::None),
            "short" => Ok(Times::Short),
            "full" => Ok(Times::Full),
            "iso" => Ok(Times::Iso),
            _ => Err(usage(format!("unknown time format: {text}"))),
        }
    }

    /// The strftime(3) format of a time, and its width.
    fn format(self) -> (&'static str, usize) {
        match self {
            Times::None => ("", 0),
            Times::Short => ("%a %b %e %H:%M", 16),
            Times::Full => ("%a %b %e %T %Y", 24),
            Times::Iso => ("%Y-%m-%dT%H:%M:%S%z", 24),
        }
    }
}

/// What last(1) and lastb(1) were asked to show.
struct Options {
    limit: Option<usize>,
    times: Times,
    /// `-x`: the computer's stops too.
    system: bool,
    /// Not `-R`: where each sign-in came from.
    hosts: bool,
    /// `-a`: where from, last.
    host_last: bool,
    /// `-w`: whole names.
    full_names: bool,
    since: Option<i64>,
    until: Option<i64>,
    present: Option<i64>,
    /// The names and terminals asked about.
    who: Vec<String>,
}

/// A time option's value, in seconds since the epoch, as journalctl and
/// log(1) take it.
fn time_value(args: &mut Args) -> Result<i64, Fail> {
    let text = args.text()?;
    Ok((log::parse_time(&text, util::now_ns(), false)? / 1_000_000_000) as i64)
}

fn options(args: &mut Args) -> Result<Options, Fail> {
    let mut o = Options {
        limit: None,
        times: Times::Short,
        system: false,
        hosts: true,
        host_last: false,
        full_names: false,
        since: None,
        until: None,
        present: None,
        who: Vec::new(),
    };
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('n') => o.limit = Some(args.number()?),
            Arg::Long(l) if l == "limit" => o.limit = Some(args.number()?),
            // `-5`, as `-n 5`.
            Arg::Short(digit) if digit.is_ascii_digit() => {
                let rest = args.joined_value().map(|v| v.to_string_lossy().into_owned());
                let text = format!("{digit}{}", rest.unwrap_or_default());
                o.limit =
                    Some(text.parse().map_err(|_| usage(format!("invalid option -- '{text}'")))?);
            }
            Arg::Short('F') => o.times = Times::Full,
            Arg::Long(l) if l == "fulltimes" => o.times = Times::Full,
            Arg::Long(l) if l == "time-format" => o.times = Times::parse(&args.text()?)?,
            Arg::Short('x') => o.system = true,
            Arg::Long(l) if l == "system" => o.system = true,
            Arg::Short('R') => o.hosts = false,
            Arg::Long(l) if l == "nohostname" => o.hosts = false,
            Arg::Short('a') => o.host_last = true,
            Arg::Long(l) if l == "hostlast" => o.host_last = true,
            Arg::Short('w') => o.full_names = true,
            Arg::Long(l) if l == "fullnames" => o.full_names = true,
            Arg::Short('s') => o.since = Some(time_value(args)?),
            Arg::Long(l) if l == "since" => o.since = Some(time_value(args)?),
            Arg::Short('t') => o.until = Some(time_value(args)?),
            Arg::Long(l) if l == "until" => o.until = Some(time_value(args)?),
            Arg::Short('p') => o.present = Some(time_value(args)?),
            Arg::Long(l) if l == "present" => o.present = Some(time_value(args)?),
            // Addresses are kept as they came: there is no name to look
            // up or to turn back into an address.
            Arg::Short('d' | 'i') => {}
            Arg::Long(l) if l == "dns" || l == "ip" => {}
            Arg::Operand(who) => o.who.push(who.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    Ok(o)
}

/// The records of the file at `path`: none if there is none, and `None`
/// if it may not be read.
fn read(path: &str) -> Result<Option<Vec<Record>>, Fail> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(signins::parse(&text))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Some(Vec::new())),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => Ok(None),
        Err(err) => Err(util::failed(&path, err)),
    }
}

/// The records the caller may read, and whether they are everyone's: the
/// computer's starts and stops, and, to root, who lists the directory,
/// every account's and the unknown names', or to anyone else their own.
fn records() -> Result<(Vec<Record>, bool), Fail> {
    let mut records = read(signins::SYSTEM)?.unwrap_or_default();
    match fs::read_dir(signins::DIR) {
        Ok(entries) => {
            let mut paths: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| {
                    n == "unknown" || (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                })
                .map(|n| format!("{}/{n}", signins::DIR))
                .collect();
            paths.sort();
            for path in paths {
                records.extend(read(&path)?.unwrap_or_default());
            }
            Ok((records, true))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok((records, true)),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            records.extend(read(&signins::of(util::own_user()))?.unwrap_or_default());
            Ok((records, false))
        }
        Err(err) => Err(util::failed(&signins::DIR, err)),
    }
}

/// Says on the standard error that `tool` shows the caller's own.
fn hint(tool: &str) {
    eprintln!(
        "Hint: you see only your own sign-ins. An administrator sees everyone's with: elevate {tool}"
    );
}

/// A duration, as util-linux's last shows it: ` (HH:MM)`, or `(D+HH:MM)`.
fn length(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3600 % 24, seconds / 60 % 60);
    if days > 0 {
        format!("({days}+{hours:02}:{minutes:02})")
    } else {
        format!(" ({hours:02}:{minutes:02})")
    }
}

/// The end of `line` and its length, as util-linux's last shows them,
/// times written with `when`.
fn end_text(line: &Line, times: Times, when: &dyn Fn(i64, &str) -> String) -> (String, String) {
    let (format, _) = times.format();
    let short = times == Times::Short;
    let still = |what: &str| {
        if short {
            ("  still".to_string(), what.to_string())
        } else {
            (format!("  still {what}"), String::new())
        }
    };
    match line.end {
        End::At(time) => {
            let at = if short { when(time, "%H:%M") } else { when(time, format) };
            (format!("- {at}"), length(time - line.start))
        }
        End::Down(time) => ("- down ".into(), length(time - line.start)),
        End::Crash(time) => ("- crash".into(), length(time - line.start)),
        End::Running if line.is_system() => still("running"),
        End::Running => still("logged in"),
        End::Gone if line.is_system() => (String::new(), String::new()),
        End::Gone => ("   gone".into(), "- no logout".into()),
    }
}

/// A line as last(1) shows it.
fn row(line: &Line, o: &Options, when: &dyn Fn(i64, &str) -> String) -> String {
    let name =
        if o.full_names { format!("{:<8}", line.name) } else { format!("{:<8.8}", line.name) };
    let mut row = format!("{name} {:<12.12} ", line.terminal);
    if o.hosts && !o.host_last {
        row += &format!("{:<16.16} ", line.from);
    }
    let (format, width) = o.times.format();
    let (end, length) = end_text(line, o.times, when);
    if o.times == Times::None {
        row += &length;
    } else {
        let end_width = if o.times == Times::Short { 7 } else { width + 2 };
        row += &format!("{:<width$} {end:<end_width$} {length}", when(line.start, format));
    }
    if o.hosts && o.host_last {
        row = format!("{} {}", row.trim_end(), line.from);
    }
    row.trim_end().to_string()
}

/// Whether `line` is one `o` asks for.
fn wanted(line: &Line, o: &Options) -> bool {
    if line.name == "shutdown" && line.session.is_none() && !o.system {
        return false;
    }
    if !o.who.is_empty() && !o.who.iter().any(|w| *w == line.name || *w == line.terminal) {
        return false;
    }
    let end = match line.end {
        End::At(t) | End::Down(t) | End::Crash(t) => Some(t),
        End::Running | End::Gone => None,
    };
    o.since.is_none_or(|since| line.start >= since)
        && o.until.is_none_or(|until| line.start <= until)
        && o.present.is_none_or(|at| line.start <= at && end.is_none_or(|end| end >= at))
}

/// Writes `lines`, those `o` asks for, then when the records begin.
fn show(
    lines: &[Line],
    o: &Options,
    begins: i64,
    when: &dyn Fn(i64, &str) -> String,
) -> Result<(), Fail> {
    let mut out = io::stdout().lock();
    let shown = lines.iter().filter(|l| wanted(l, o)).take(o.limit.unwrap_or(usize::MAX));
    for line in shown {
        writeln!(out, "{}", row(line, o, when))?;
    }
    writeln!(out, "\nsign-in records begin {}", when(begins, "%a %b %e %T %Y"))?;
    Ok(())
}

/// When the oldest of `records` is, or now if there is none.
fn begins(records: &[Record]) -> i64 {
    records.iter().map(|r| r.time).min().unwrap_or((util::now_ns() / 1_000_000_000) as i64)
}

/// last(1): the sign-ins, and the computer's starts, newest first.
pub fn last(args: &mut Args) -> Result<i32, Fail> {
    let o = options(args)?;
    let (records, everyone) = records()?;
    let alive = |session: u32| {
        proc::process(u64::from(session)).is_ok_and(|p| p.session == p.number && !p.zombie())
    };
    let lines = signins::history(&records, Some(sessions::boot_seconds()), alive);
    if !everyone {
        hint("last");
    }
    show(&lines, &o, begins(&records), &util::local_time)?;
    Ok(0)
}

/// lastb(1): the wrong passwords typed at a sign-in, newest first.
pub fn lastb(args: &mut Args) -> Result<i32, Fail> {
    let o = options(args)?;
    let (records, everyone) = records()?;
    let lines = signins::failures(&records);
    if !everyone {
        hint("lastb");
    }
    let begins = begins(&records);
    show(&lines, &o, begins, &util::local_time)?;
    Ok(0)
}

/// lastlog(8): each account's last sign-in.
pub fn lastlog(args: &mut Args) -> Result<i32, Fail> {
    let mut only: Option<String> = None;
    let mut newer: Option<i64> = None;
    let mut older: Option<i64> = None;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('u') => only = Some(args.text()?),
            Arg::Long(l) if l == "user" => only = Some(args.text()?),
            Arg::Short('t') => newer = Some(args.number()?),
            Arg::Long(l) if l == "time" => newer = Some(args.number()?),
            Arg::Short('b') => older = Some(args.number()?),
            Arg::Long(l) if l == "before" => older = Some(args.number()?),
            other => return Err(args.unknown(&other)),
        }
    }
    let passwd =
        fs::read_to_string(accounts::PASSWD).map_err(|e| util::failed(&accounts::PASSWD, e))?;
    let users = accounts::users(&passwd);
    let own = util::own_user();
    let everyone = own == 0;
    let mut accounts: Vec<&User> =
        users.entries().filter(|u| u.uid == 0 || u.is_person()).collect();
    if let Some(name) = &only {
        let user = users
            .entries()
            .find(|u| u.name == *name || u.uid.to_string() == *name)
            .ok_or_else(|| fail(format!("there is no account {name}")))?;
        if !everyone && user.uid != own {
            return Err(fail(format!(
                "the sign-ins of {} are for administrators: run elevate lastlog -u {}",
                user.name, user.name
            )));
        }
        accounts = vec![user];
    } else if !everyone {
        accounts.retain(|u| u.uid == own);
        hint("lastlog");
    }
    let now = (util::now_ns() / 1_000_000_000) as i64;
    let mut out = io::stdout().lock();
    writeln!(out, "{:<16} {:<8} {:<16} Latest", "Username", "Port", "From")?;
    for user in accounts {
        let records = read(&signins::of(user.uid))?.unwrap_or_default();
        let latest = signins::latest(&records).sign_in;
        let time = latest.as_ref().map_or(0, |s| s.time);
        if newer.is_some_and(|days| latest.is_none() || time < now - days * 86_400) {
            continue;
        }
        if older.is_some_and(|days| latest.is_some() && time >= now - days * 86_400) {
            continue;
        }
        let line = match &latest {
            Some(place) => format!(
                "{:<16} {:<8.8} {:<16.16} {}",
                user.name,
                place.terminal,
                place.from,
                util::local_time(place.time, "%a %b %e %H:%M:%S %z %Y")
            ),
            None => format!("{:<16} {:<8} {:<16} **Never signed in**", user.name, "", ""),
        };
        writeln!(out, "{line}")?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(name: &str, terminal: &str, start: i64, end: End) -> Line {
        let system = name == "reboot" || name == "shutdown";
        Line {
            name: name.into(),
            terminal: terminal.into(),
            from: if system { "0.13.0".into() } else { String::new() },
            start,
            end,
            session: (!system).then_some(61),
        }
    }

    fn defaults() -> Options {
        Options {
            limit: None,
            times: Times::Short,
            system: false,
            hosts: true,
            host_last: false,
            full_names: false,
            since: None,
            until: None,
            present: None,
            who: Vec::new(),
        }
    }

    /// Times in UTC, as `T<seconds>` for the short end.
    fn when(time: i64, format: &str) -> String {
        util::time_text(time, format, true)
    }

    #[test]
    fn rows_are_util_linux_last_s() {
        let o = defaults();
        // 2026-10-31 12:40 UTC is a Saturday.
        let start = 1_793_450_400;
        let rows = [
            (
                line("alice", "pts/0", start, End::Running),
                "alice    pts/0                         Sat Oct 31 12:40   still logged in",
            ),
            (
                line("bob", "pts/1", start, End::At(start + 300)),
                "bob      pts/1                         Sat Oct 31 12:40 - 12:45  (00:05)",
            ),
            (
                line("bob", "pts/1", start, End::At(start + 90_000)),
                "bob      pts/1                         Sat Oct 31 12:40 - 13:40 (1+01:00)",
            ),
            (
                line("carol", "tty3", start, End::Down(start + 3600)),
                "carol    tty3                          Sat Oct 31 12:40 - down   (01:00)",
            ),
            (
                line("carol", "tty3", start, End::Crash(start + 60)),
                "carol    tty3                          Sat Oct 31 12:40 - crash  (00:01)",
            ),
            (
                line("dave", "tty4", start, End::Gone),
                "dave     tty4                          Sat Oct 31 12:40    gone - no logout",
            ),
            (
                line("reboot", "system boot", start, End::Running),
                "reboot   system boot  0.13.0           Sat Oct 31 12:40   still running",
            ),
            (
                line("reboot", "system boot", start, End::At(start + 7200)),
                "reboot   system boot  0.13.0           Sat Oct 31 12:40 - 14:40  (02:00)",
            ),
        ];
        for (line, want) in rows {
            assert_eq!(row(&line, &o, &when), want);
        }
        let long = line("alexandria", "pts/0", start, End::Running);
        assert!(row(&long, &o, &when).starts_with("alexandr pts/0"));
        let o = Options { full_names: true, hosts: false, times: Times::Full, ..defaults() };
        assert_eq!(
            row(&long, &o, &when),
            "alexandria pts/0        Sat Oct 31 12:40:00 2026   still logged in"
        );
        let ended = line("bob", "pts/1", start, End::At(start + 300));
        assert_eq!(
            row(&ended, &o, &when),
            "bob      pts/1        Sat Oct 31 12:40:00 2026 - Sat Oct 31 12:45:00 2026  (00:05)"
        );
        let o = Options { times: Times::None, ..defaults() };
        assert_eq!(row(&ended, &o, &when), "bob      pts/1                          (00:05)");
        let o = Options { times: Times::Iso, ..defaults() };
        assert_eq!(
            row(&ended, &o, &when),
            "bob      pts/1                         2026-10-31T12:40:00+0000 - 2026-10-31T12:45:00+0000  (00:05)"
        );
        let from = Line { from: "10.0.0.2".into(), ..ended };
        let o = Options { host_last: true, ..defaults() };
        assert_eq!(
            row(&from, &o, &when),
            "bob      pts/1        Sat Oct 31 12:40 - 12:45  (00:05) 10.0.0.2"
        );
    }

    #[test]
    fn lines_are_chosen() {
        let o = defaults();
        let alice = line("alice", "pts/0", 100, End::At(200));
        let down = line("shutdown", "system down", 300, End::At(400));
        assert!(wanted(&alice, &o));
        // A stop shows with -x only.
        assert!(!wanted(&down, &o));
        assert!(wanted(&down, &Options { system: true, ..defaults() }));
        // Names and terminals.
        assert!(wanted(&alice, &Options { who: vec!["alice".into()], ..defaults() }));
        assert!(wanted(&alice, &Options { who: vec!["pts/0".into()], ..defaults() }));
        assert!(!wanted(&alice, &Options { who: vec!["bob".into()], ..defaults() }));
        // Times.
        assert!(wanted(&alice, &Options { since: Some(100), ..defaults() }));
        assert!(!wanted(&alice, &Options { since: Some(101), ..defaults() }));
        assert!(!wanted(&alice, &Options { until: Some(99), ..defaults() }));
        assert!(wanted(&alice, &Options { present: Some(150), ..defaults() }));
        assert!(!wanted(&alice, &Options { present: Some(250), ..defaults() }));
        let running = line("bob", "pts/1", 100, End::Running);
        assert!(wanted(&running, &Options { present: Some(10_000), ..defaults() }));
        assert_eq!(length(59), " (00:00)");
        assert_eq!(length(3 * 86_400 + 3600 + 120), "(3+01:02)");
    }

    #[test]
    fn options_read() {
        let mut args = Args::new(
            "last",
            ["-5", "-Fx", "--since", "2026-10-31", "bob"].map(Into::into).to_vec(),
        );
        let o = options(&mut args).map_err(|e| e.message).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(o.limit, Some(5));
        assert_eq!(o.times, Times::Full);
        assert!(o.system);
        assert!(o.since.is_some());
        assert_eq!(o.who, ["bob"]);
        let mut args =
            Args::new("last", ["-n", "12", "--time-format=iso", "-R"].map(Into::into).to_vec());
        let o = options(&mut args).map_err(|e| e.message).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((o.limit, o.times, o.hosts), (Some(12), Times::Iso, false));
        let mut args = Args::new("last", ["--time-format=long"].map(Into::into).to_vec());
        assert!(options(&mut args).is_err());
    }
}
