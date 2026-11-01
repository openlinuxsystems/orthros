// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The sign-in records (ADR 0254): when the computer started and
//! stopped, who signed in where and when, and signed out, and the wrong
//! passwords typed at a sign-in, as Linux's wtmp, btmp and lastlog keep
//! them, for last(1), lastb(1), lastlog(8) and the lines a person reads
//! as they sign in.
//!
//! authd(8) alone writes them, a line each, in [`DIR`], which is root's
//! and mode 0711, so no one but root lists it:
//!
//! - [`SYSTEM`]: the computer's starts and stops, which everyone reads,
//!   as uptime(1) tells everyone;
//! - [`of`] each account's number: its sign-ins and sign-outs and the
//!   wrong passwords typed for it, root's and the account's own group's,
//!   mode 0640, so the person and root read it and no one else does;
//! - [`UNKNOWN`]: the wrong passwords typed for names no account has,
//!   root's alone.
//!
//! A file is named by the account's number, which is never given again
//! (ADR 0220), so a later account of the same name reads nothing of the
//! one removed; a line keeps the name the account had then. A line is the
//! time, in seconds since the epoch, what happened, and its fields, none
//! with a space in it, `-` for none:
//!
//! ```text
//! 1793440000 boot 0.13.0
//! 1793440100 in alice pts/0 - 61
//! 1793440200 failed bob pts/1 -
//! 1793440300 out alice pts/0 - 61
//! 1793440400 down
//! ```
//!
//! `in`, `out` and `failed` name the terminal and where from: `-` at the
//! computer itself, a host's address once SSH comes (13c). `in` and `out`
//! end with the number of the session's first process, which pairs them.

use std::fmt::Write as _;

/// Where the records are.
pub const DIR: &str = "/system/state/sign-ins";
/// The computer's starts and stops.
pub const SYSTEM: &str = "/system/state/sign-ins/system";
/// The wrong passwords typed for names no account has.
pub const UNKNOWN: &str = "/system/state/sign-ins/unknown";

/// The file of the account numbered `number`.
pub fn of(number: u32) -> String {
    format!("{DIR}/{number}")
}

/// A file this long is trimmed when a record is added to it...
pub const FILE_MAX: u64 = 512 * 1024;
/// ...to its newest records, this many ([`trimmed`]).
pub const KEEP: usize = 4096;

/// The longest field a line keeps, in bytes.
const FIELD_MAX: usize = 64;

/// What happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The computer started, with this kernel release.
    Boot {
        /// `uname -r`.
        release: String,
    },
    /// The computer is stopping: authd was asked to end.
    Down,
    /// Someone signed in.
    In(Seat),
    /// They signed out: the session's first process ended.
    Out(Seat),
    /// A wrong password was typed at a sign-in.
    Failed {
        /// The name typed.
        name: String,
        /// The terminal.
        terminal: String,
        /// Where from, empty at the computer itself.
        from: String,
    },
}

/// Who signed in or out, where, and their session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seat {
    /// The account's name then.
    pub name: String,
    /// The terminal, as `pts/0`.
    pub terminal: String,
    /// Where from, empty at the computer itself.
    pub from: String,
    /// The number of the session's first process.
    pub session: u32,
}

/// A record: when, and what happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Seconds since the epoch.
    pub time: i64,
    /// What happened.
    pub event: Event,
}

/// `text` as a field of a line: no space or control character, which
/// show as `?`, at most [`FIELD_MAX`] bytes, and `-` if empty.
pub fn field(text: &str) -> String {
    let mut kept = String::new();
    for c in text.chars().map(|c| if c.is_whitespace() || c.is_control() { '?' } else { c }) {
        if kept.len() + c.len_utf8() > FIELD_MAX {
            break;
        }
        kept.push(c);
    }
    if kept.is_empty() { "-".into() } else { kept }
}

/// A field read back: `-` is none.
fn unfield(text: &str) -> String {
    if text == "-" { String::new() } else { text.into() }
}

impl Record {
    /// Its line, with its newline.
    pub fn line(&self) -> String {
        let time = self.time;
        match &self.event {
            Event::Boot { release } => format!("{time} boot {}\n", field(release)),
            Event::Down => format!("{time} down\n"),
            Event::In(seat) | Event::Out(seat) => {
                let kind = if matches!(self.event, Event::In(_)) { "in" } else { "out" };
                format!(
                    "{time} {kind} {} {} {} {}\n",
                    field(&seat.name),
                    field(&seat.terminal),
                    field(&seat.from),
                    seat.session
                )
            }
            Event::Failed { name, terminal, from } => {
                format!("{time} failed {} {} {}\n", field(name), field(terminal), field(from))
            }
        }
    }

    /// The record in `line`, if it is one.
    pub fn parse(line: &str) -> Option<Record> {
        let mut fields = line.split_ascii_whitespace();
        let time = fields.next()?.parse().ok()?;
        let kind = fields.next()?;
        let rest: Vec<&str> = fields.collect();
        let event = match (kind, rest.as_slice()) {
            ("boot", [release]) => Event::Boot { release: unfield(release) },
            ("down", []) => Event::Down,
            ("in" | "out", [name, terminal, from, session]) => {
                let seat = Seat {
                    name: unfield(name),
                    terminal: unfield(terminal),
                    from: unfield(from),
                    session: session.parse().ok()?,
                };
                if kind == "in" { Event::In(seat) } else { Event::Out(seat) }
            }
            ("failed", [name, terminal, from]) => Event::Failed {
                name: unfield(name),
                terminal: unfield(terminal),
                from: unfield(from),
            },
            _ => return None,
        };
        Some(Record { time, event })
    }
}

/// The records in `text`, in its order; a line that is none is passed
/// over.
pub fn parse(text: &str) -> Vec<Record> {
    text.lines().filter_map(Record::parse).collect()
}

/// What to keep of a file's `text` that grew past [`FILE_MAX`]: its
/// newest [`KEEP`] lines, and, if they hold none, the newest sign-in
/// before them with its sign-out, and the newest start, so that
/// lastlog(8) and the next sign-in still know them however many wrong
/// passwords came after.
pub fn trimmed(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= KEEP {
        return text.to_string();
    }
    let (old, new) = lines.split_at(lines.len() - KEEP);
    let records = |lines: &[&str]| -> Vec<Option<Record>> {
        lines.iter().map(|line| Record::parse(line)).collect()
    };
    let (old_records, new_records) = (records(old), records(new));
    let has = |records: &[Option<Record>], is: fn(&Event) -> bool| {
        records.iter().flatten().any(|r| is(&r.event))
    };
    let is_in = |e: &Event| matches!(e, Event::In(_));
    let is_boot = |e: &Event| matches!(e, Event::Boot { .. });
    let mut kept: Vec<usize> = Vec::new();
    if !has(&new_records, is_in)
        && let Some(i) =
            old_records.iter().rposition(|r| r.as_ref().is_some_and(|r| is_in(&r.event)))
    {
        kept.push(i);
        if let Some(Some(Record { event: Event::In(seat), .. })) = old_records.get(i) {
            let out = old_records[i + 1..].iter().position(|r| {
                matches!(r, Some(Record { event: Event::Out(s), .. }) if s.session == seat.session)
            });
            kept.extend(out.map(|j| i + 1 + j));
        }
    }
    if !has(&new_records, is_boot)
        && let Some(i) =
            old_records.iter().rposition(|r| r.as_ref().is_some_and(|r| is_boot(&r.event)))
    {
        kept.push(i);
    }
    kept.sort_unstable();
    kept.dedup();
    let mut text = String::new();
    for line in kept.iter().map(|&i| old[i]).chain(new.iter().copied()) {
        let _ = writeln!(text, "{line}");
    }
    text
}

/// How a line of last(1) ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// At this time: a sign-out, the stop of a start, the start after a
    /// stop, or a wrong password's own time.
    At(i64),
    /// The computer stopped at this time while the session ran.
    Down(i64),
    /// The computer started again at this time with no stop before.
    Crash(i64),
    /// It runs still.
    Running,
    /// Its first process is gone, and no sign-out was recorded, as when
    /// authd was not there to see it.
    Gone,
}

/// A line of last(1) or lastb(1): a session, a start of the computer, a
/// stop (`shutdown`, shown with `-x`), or a wrong password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The account's name, `reboot` or `shutdown`.
    pub name: String,
    /// The terminal, `system boot` or `system down`.
    pub terminal: String,
    /// Where from, or the kernel's release for a start or a stop.
    pub from: String,
    /// When it began.
    pub start: i64,
    /// How it ended.
    pub end: End,
    /// The number of a session's first process.
    pub session: Option<u32>,
}

impl Line {
    /// Whether it is a start or a stop of the computer.
    pub fn is_system(&self) -> bool {
        self.session.is_none() && (self.name == "reboot" || self.name == "shutdown")
    }
}

/// The lines of last(1) for `records`, every file's merged, newest
/// first. `boot` is when the computer last started, if known: the
/// records' newest start, if it is that one (a second either way), is
/// the start that runs, and a session of it with no sign-out runs if
/// `alive` says its first process does. Without `boot`, the newest start
/// runs.
pub fn history(records: &[Record], boot: Option<i64>, alive: impl Fn(u32) -> bool) -> Vec<Line> {
    let mut records: Vec<&Record> = records.iter().collect();
    // Each file is in time order; merged, a start comes before what
    // happened in the same second, and a stop after, and the sort keeps
    // each file's own order: a sign-in before its sign-out.
    let rank = |e: &Event| match e {
        Event::Boot { .. } => 0,
        Event::Down => 2,
        _ => 1,
    };
    records.sort_by_key(|r| (r.time, rank(&r.event)));
    let mut lines: Vec<Line> = Vec::new();
    // The sessions and the start that run, and the stop the next start ends.
    let mut open: Vec<usize> = Vec::new();
    let mut started: Option<usize> = None;
    let mut stopped: Option<usize> = None;
    let mut release = String::new();
    for record in records {
        let time = record.time;
        match &record.event {
            Event::Boot { release: now } => {
                for i in open.drain(..) {
                    lines[i].end = End::Crash(time);
                }
                if let Some(i) = started.take() {
                    lines[i].end = End::Crash(time);
                }
                if let Some(i) = stopped.take() {
                    lines[i].end = End::At(time);
                }
                release.clone_from(now);
                started = Some(lines.len());
                lines.push(Line {
                    name: "reboot".into(),
                    terminal: "system boot".into(),
                    from: now.clone(),
                    start: time,
                    end: End::Running,
                    session: None,
                });
            }
            Event::Down => {
                for i in open.drain(..) {
                    lines[i].end = End::Down(time);
                }
                if let Some(i) = started.take() {
                    lines[i].end = End::At(time);
                }
                stopped = Some(lines.len());
                lines.push(Line {
                    name: "shutdown".into(),
                    terminal: "system down".into(),
                    from: release.clone(),
                    start: time,
                    end: End::Running,
                    session: None,
                });
            }
            Event::In(seat) => {
                open.push(lines.len());
                lines.push(Line {
                    name: seat.name.clone(),
                    terminal: seat.terminal.clone(),
                    from: seat.from.clone(),
                    start: time,
                    end: End::Running,
                    session: Some(seat.session),
                });
            }
            Event::Out(seat) => {
                let found = open.iter().position(|&i| {
                    lines[i].session == Some(seat.session) && lines[i].terminal == seat.terminal
                });
                if let Some(at) = found {
                    let i = open.remove(at);
                    lines[i].end = End::At(time);
                }
            }
            Event::Failed { .. } => {}
        }
    }
    // What is still open ran until now, if its start is the one that
    // runs; otherwise no one saw it end.
    let current = started.is_none_or(|i| boot.is_none_or(|b| (lines[i].start - b).abs() <= 1));
    for i in open {
        let running = current && lines[i].session.is_some_and(&alive);
        lines[i].end = if running { End::Running } else { End::Gone };
    }
    if let Some(i) = started
        && !current
    {
        lines[i].end = End::Gone;
    }
    if let Some(i) = stopped {
        // A stop with no start after it: the computer is still stopping,
        // or never started again.
        lines[i].end = End::Gone;
    }
    lines.reverse();
    lines
}

/// The lines of lastb(1) for `records`: each wrong password, newest
/// first, ending when it began.
pub fn failures(records: &[Record]) -> Vec<Line> {
    let mut lines: Vec<Line> = records
        .iter()
        .filter_map(|r| match &r.event {
            Event::Failed { name, terminal, from } => Some(Line {
                name: name.clone(),
                terminal: terminal.clone(),
                from: from.clone(),
                start: r.time,
                end: End::At(r.time),
                session: None,
            }),
            _ => None,
        })
        .collect();
    lines.sort_by_key(|l| std::cmp::Reverse(l.start));
    lines
}

/// When, where and where from: a sign-in or a wrong password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// Seconds since the epoch.
    pub time: i64,
    /// The terminal.
    pub terminal: String,
    /// Where from, empty at the computer itself.
    pub from: String,
}

/// An account's newest sign-in, and the wrong passwords typed for it
/// since: what lastlog(8) shows, and a person reads as they sign in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Latest {
    /// The newest sign-in.
    pub sign_in: Option<Place>,
    /// How many wrong passwords were typed since, or ever if none.
    pub failed: usize,
    /// The newest of them.
    pub last_failed: Option<Place>,
}

/// The newest sign-in in `records`, one account's, and the wrong
/// passwords after it.
pub fn latest(records: &[Record]) -> Latest {
    let mut latest = Latest::default();
    for record in records {
        match &record.event {
            Event::In(seat) if latest.sign_in.as_ref().is_none_or(|s| s.time <= record.time) => {
                latest.sign_in = Some(Place {
                    time: record.time,
                    terminal: seat.terminal.clone(),
                    from: seat.from.clone(),
                });
                latest.failed = 0;
                latest.last_failed = None;
            }
            Event::Failed { terminal, from, .. }
                if latest.sign_in.as_ref().is_none_or(|s| s.time <= record.time) =>
            {
                latest.failed += 1;
                latest.last_failed = Some(Place {
                    time: record.time,
                    terminal: terminal.clone(),
                    from: from.clone(),
                });
            }
            _ => {}
        }
    }
    latest
}

/// The newest start in `records`, if any: whether the computer's start
/// that runs was recorded already, so that authd, started again, does not
/// record it twice.
pub fn last_boot(records: &[Record]) -> Option<i64> {
    records.iter().rev().find(|r| matches!(r.event, Event::Boot { .. })).map(|r| r.time)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(name: &str, terminal: &str, session: u32) -> Seat {
        Seat { name: name.into(), terminal: terminal.into(), from: String::new(), session }
    }

    fn records(text: &str) -> Vec<Record> {
        let records = parse(text);
        assert_eq!(records.len(), text.lines().count(), "{text}");
        records
    }

    #[test]
    fn lines_read_back() {
        let all = [
            Record { time: 100, event: Event::Boot { release: "0.13.0".into() } },
            Record { time: 101, event: Event::In(seat("alice", "pts/0", 61)) },
            Record {
                time: 102,
                event: Event::Failed {
                    name: "bob".into(),
                    terminal: "pts/1".into(),
                    from: "10.0.0.2".into(),
                },
            },
            Record { time: 103, event: Event::Out(seat("alice", "pts/0", 61)) },
            Record { time: 104, event: Event::Down },
        ];
        let text: String = all.iter().map(Record::line).collect();
        assert_eq!(
            text,
            "100 boot 0.13.0\n101 in alice pts/0 - 61\n102 failed bob pts/1 10.0.0.2\n\
             103 out alice pts/0 - 61\n104 down\n"
        );
        assert_eq!(parse(&text), all);
        // A name typed may hold anything: it stays one field of one line.
        let typed = Record {
            time: 5,
            event: Event::Failed {
                name: "a b\nc\td".into(),
                terminal: "tty1".into(),
                from: String::new(),
            },
        };
        assert_eq!(typed.line(), "5 failed a?b?c?d tty1 -\n");
        let long = "x".repeat(100);
        assert_eq!(field(&long).len(), FIELD_MAX);
        assert_eq!(field(""), "-");
        // What it does not read is passed over.
        for bad in ["", "x in a b c 1", "5 in a b c", "5 in a b c x", "5 later", "5 down now"] {
            assert_eq!(Record::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn sessions_pair_with_their_ends() {
        let text = "100 boot 0.12.0\n\
                    110 in alice tty1 - 61\n\
                    120 in bob pts/1 10.0.0.2 70\n\
                    130 out alice tty1 - 61\n\
                    200 down\n\
                    300 boot 0.13.0\n\
                    310 in alice tty1 - 64\n\
                    320 in bob tty2 - 66\n\
                    330 in carol tty3 - 68\n\
                    400 boot 0.13.0\n\
                    410 in alice tty1 - 61\n\
                    420 in bob tty2 - 62\n";
        let lines = history(&records(text), Some(400), |session| session == 61);
        let shown: Vec<(&str, &str, i64, End)> =
            lines.iter().map(|l| (l.name.as_str(), l.terminal.as_str(), l.start, l.end)).collect();
        assert_eq!(
            shown,
            [
                // The start that runs, a session of it that runs, and one
                // whose first process is gone with no sign-out seen.
                ("bob", "tty2", 420, End::Gone),
                ("alice", "tty1", 410, End::Running),
                ("reboot", "system boot", 400, End::Running),
                // The start before it ended with no stop: a crash.
                ("carol", "tty3", 330, End::Crash(400)),
                ("bob", "tty2", 320, End::Crash(400)),
                ("alice", "tty1", 310, End::Crash(400)),
                ("reboot", "system boot", 300, End::Crash(400)),
                // A stop ends what ran, and the next start ends it.
                ("shutdown", "system down", 200, End::At(300)),
                ("bob", "pts/1", 120, End::Down(200)),
                ("alice", "tty1", 110, End::At(130)),
                ("reboot", "system boot", 100, End::At(200)),
            ]
        );
        assert_eq!(lines[0].session, Some(62));
        assert_eq!(lines[2].from, "0.13.0");
        assert_eq!(lines[7].from, "0.12.0");
        assert_eq!(lines[8].from, "10.0.0.2");
        assert!(lines[2].is_system() && lines[7].is_system() && !lines[1].is_system());
        // If the newest start is not the one that runs, as when authd has
        // not started since, nothing runs.
        let lines = history(&records(text), Some(500), |_| true);
        assert_eq!(lines[0].end, End::Gone);
        assert_eq!(lines[2].end, End::Gone);
        // Without the start's time, the newest start is taken to run.
        let lines = history(&records(text), None, |_| true);
        assert_eq!(lines[0].end, End::Running);
    }

    #[test]
    fn files_merge_in_time_order() {
        // The system's file and two accounts', as last(1) reads them: a
        // start comes before what happened in the same second, and a stop
        // after it, and a file keeps its own order.
        let mut all = records("100 boot 0.13.0\n200 down\n230 boot 0.13.0\n");
        all.extend(records("100 in alice tty1 - 61\n200 out alice tty1 - 61\n"));
        all.extend(records("150 in bob tty2 - 62\n230 in bob tty2 - 63\n230 out bob tty2 - 63\n"));
        let lines = history(&all, Some(230), |_| true);
        let shown: Vec<(&str, End)> = lines.iter().map(|l| (l.name.as_str(), l.end)).collect();
        assert_eq!(
            shown,
            [
                ("bob", End::At(230)),
                ("reboot", End::Running),
                ("shutdown", End::At(230)),
                ("bob", End::Down(200)),
                ("alice", End::At(200)),
                ("reboot", End::At(200)),
            ]
        );
        // A sign-out pairs with its own session only.
        let lines = history(
            &records("10 in alice tty1 - 5\n11 in alice tty1 - 6\n12 out alice tty1 - 5\n"),
            None,
            |_| true,
        );
        assert_eq!((lines[0].session, lines[0].end), (Some(6), End::Running));
        assert_eq!((lines[1].session, lines[1].end), (Some(5), End::At(12)));
        // A sign-out whose sign-in was trimmed away is passed over.
        assert!(history(&records("12 out alice tty1 - 5\n"), None, |_| true).is_empty());
    }

    #[test]
    fn wrong_passwords_are_counted_since_the_last_sign_in() {
        let text = "10 failed alice tty1 -\n\
                    20 in alice tty1 - 61\n\
                    30 out alice tty1 - 61\n\
                    40 failed alice tty2 -\n\
                    50 failed alice pts/3 10.0.0.2\n";
        let latest = latest(&records(text));
        let place = |time, terminal: &str, from: &str| Place {
            time,
            terminal: terminal.into(),
            from: from.into(),
        };
        assert_eq!(
            latest,
            Latest {
                sign_in: Some(place(20, "tty1", "")),
                failed: 2,
                last_failed: Some(place(50, "pts/3", "10.0.0.2")),
            }
        );
        // Before any sign-in, every wrong one counts.
        let first = super::latest(&records("10 failed alice tty1 -\n"));
        assert_eq!((first.sign_in, first.failed), (None, 1));
        assert_eq!(super::latest(&[]), Latest::default());
        let failures = failures(&records(text));
        let starts: Vec<(i64, End)> = failures.iter().map(|l| (l.start, l.end)).collect();
        assert_eq!(starts, [(50, End::At(50)), (40, End::At(40)), (10, End::At(10))]);
        assert_eq!(failures[0].from, "10.0.0.2");
        assert_eq!(last_boot(&records("1 boot a\n2 down\n3 boot b\n4 down\n")), Some(3));
        assert_eq!(last_boot(&[]), None);
    }

    #[test]
    fn trimming_keeps_the_newest_and_the_last_sign_in() {
        // A sign-in, its sign-out, then more wrong passwords than are kept.
        let mut text = String::from("1 boot 0.13.0\n2 in alice tty1 - 7\n3 in alice tty2 - 8\n");
        text += "4 out alice tty2 - 8\n5 out alice tty1 - 7\n";
        for i in 0..KEEP + 10 {
            text += &format!("{} failed alice tty1 -\n", 10 + i);
        }
        let kept = trimmed(&text);
        let records = parse(&kept);
        assert_eq!(records.len(), KEEP + 3);
        // The newest sign-in and its own sign-out, and the start.
        let first: Vec<&str> = kept.lines().take(4).collect();
        assert_eq!(
            first,
            [
                "1 boot 0.13.0",
                "3 in alice tty2 - 8",
                "4 out alice tty2 - 8",
                "20 failed alice tty1 -"
            ]
        );
        assert_eq!(records.last().map(|r| r.time), Some(10 + KEEP as i64 + 9));
        let latest = latest(&records);
        assert_eq!(latest.sign_in.map(|s| s.time), Some(3));
        assert_eq!(latest.failed, KEEP);
        // A file of fewer lines stays as it is; one whose newest lines
        // hold a sign-in keeps nothing older.
        assert_eq!(trimmed("1 down\n"), "1 down\n");
        let mut text = String::from("1 in alice tty1 - 7\n");
        for i in 0..KEEP {
            text += &format!("{} in alice tty1 - {}\n", 10 + i, 10 + i);
        }
        assert_eq!(parse(&trimmed(&text)).len(), KEEP);
    }
}
