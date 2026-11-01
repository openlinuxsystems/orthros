// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The command record (ADR 0262): every command typed at an interactive
//! shell, with when it ran, how it ended, how long it took and in which
//! folder, appended at once to `~/.local/state/nukesh/commands`, which
//! every shell of the person's shares. Ctrl+R's picker and the hints
//! from the history read it, so a command typed in one shell is found in
//! the others at their next prompt. The history file (`HISTFILE`), its
//! numbers and `fc` are bash's, and unchanged.
//!
//! Each line is `time TAB status TAB milliseconds TAB folder TAB command`,
//! the command's newlines and backslashes escaped.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// The file, under the home folder.
pub const FILE: &str = ".local/state/nukesh/commands";

/// The most records kept: older ones are dropped when the file grows to
/// half again as many.
const KEEP: usize = 20_000;

/// One command as it was run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Seconds since 1970.
    pub time: i64,
    pub status: i32,
    pub millis: u64,
    pub folder: String,
    pub command: String,
}

impl Record {
    /// The record as a line of the file, with its newline.
    pub fn line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\n",
            self.time,
            self.status,
            self.millis,
            escape(&self.folder),
            escape(&self.command)
        )
    }

    /// A line of the file, or none for one that is not a record.
    pub fn parse(line: &str) -> Option<Record> {
        let mut fields = line.splitn(5, '\t');
        let time = fields.next()?.parse().ok()?;
        let status = fields.next()?.parse().ok()?;
        let millis = fields.next()?.parse().ok()?;
        let folder = unescape(fields.next()?);
        let command = unescape(fields.next()?);
        (!command.is_empty()).then_some(Record { time, status, millis, folder, command })
    }
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', "\\t")
}

fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// The records of every shell, read as they grow.
#[derive(Debug, Default)]
pub struct Records {
    /// The file, or none without a home.
    path: Option<String>,
    /// Every record read, oldest first.
    pub all: Vec<Record>,
    /// How far into the file has been read.
    offset: u64,
}

impl Records {
    pub fn new(home: Option<&str>) -> Records {
        let path = home
            .filter(|h| h.starts_with('/'))
            .map(|h| format!("{}/{FILE}", h.trim_end_matches('/')));
        let mut records = Records { path, ..Records::default() };
        records.refresh();
        records
    }

    /// Reads what other shells have added since the last look: one
    /// stat when nothing was.
    pub fn refresh(&mut self) {
        let Some(path) = &self.path else { return };
        let Ok(length) = std::fs::metadata(path).map(|m| m.len()) else { return };
        if length == self.offset {
            return;
        }
        let Ok(mut file) = std::fs::File::open(path) else { return };
        if length < self.offset {
            // Trimmed by another shell: read it all again.
            self.offset = 0;
            self.all.clear();
        }
        if length == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut data = Vec::new();
        if file.read_to_end(&mut data).is_err() {
            return;
        }
        // Only whole lines; a line being written is read next time.
        let Some(end) = data.iter().rposition(|&b| b == b'\n') else { return };
        let text = String::from_utf8_lossy(&data[..=end]);
        self.all.extend(text.lines().filter_map(Record::parse));
        self.offset += end as u64 + 1;
    }

    /// Adds a record, to the file at once; it is read back, with those
    /// of other shells, at the next [`Records::refresh`].
    pub fn add(&mut self, record: &Record) {
        let Some(path) = &self.path else { return };
        let open = || std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path);
        let opened = open().or_else(|err| {
            // The first: its folder, which only its owner may read.
            let dir = std::path::Path::new(path).parent().ok_or(err)?;
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            open()
        });
        if let Ok(mut file) = opened {
            // One write, so that shells writing at once do not interleave.
            let _ = file.write_all(record.line().as_bytes());
        }
        if self.all.len() > KEEP + KEEP / 2 {
            self.refresh();
            self.trim();
        }
    }

    /// Keeps the newest `KEEP` records.
    fn trim(&mut self) {
        let Some(path) = &self.path else { return };
        let start = self.all.len().saturating_sub(KEEP);
        let kept: String = self.all[start..].iter().map(Record::line).collect();
        let temporary = format!("{path}.new");
        if std::fs::write(&temporary, kept.as_bytes()).is_ok()
            && std::fs::rename(&temporary, path).is_ok()
        {
            self.all.drain(..start);
            self.offset = kept.len() as u64;
        }
    }

    /// The newest command that starts with `prefix`: in `folder` first,
    /// then anywhere.
    pub fn hint(&self, prefix: &str, folder: &str) -> Option<&str> {
        if prefix.is_empty() {
            return None;
        }
        let matching = |r: &&Record| r.command.starts_with(prefix) && r.command != prefix;
        self.all
            .iter()
            .rev()
            .filter(matching)
            .find(|r| r.folder == folder)
            .or_else(|| self.all.iter().rev().find(matching))
            .map(|r| r.command.as_str())
    }

    /// The command most often run next after `last` (ADR 0280): of the
    /// records right after one of `last`, within 15 minutes, each counts
    /// once, twice if in `folder`; the most counted wins if it came next
    /// at least twice, the newest of equals first. Nothing if it is `last`
    /// itself or it failed the last time.
    pub fn next_after(&self, last: &str, folder: &str) -> Option<&str> {
        if last.is_empty() {
            return None;
        }
        // Command, weight, where last seen, whether it worked, times seen.
        let mut counts: Vec<(&str, u32, usize, bool, u32)> = Vec::new();
        for (at, pair) in self.all.windows(2).enumerate() {
            let (before, after) = (&pair[0], &pair[1]);
            if before.command != last || after.command == last || after.time - before.time > 900 {
                continue;
            }
            let weight = if after.folder == folder { 2 } else { 1 };
            match counts.iter_mut().find(|c| c.0 == after.command) {
                Some(c) => {
                    c.1 += weight;
                    c.2 = at;
                    c.3 = after.status == 0;
                    c.4 += 1;
                }
                None => counts.push((after.command.as_str(), weight, at, after.status == 0, 1)),
            }
        }
        counts
            .into_iter()
            .filter(|c| c.4 >= 2 && c.3)
            .max_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)))
            .map(|c| c.0)
    }

    /// The commands that match `typed` loosely (its letters in order), the
    /// best first: closer matches, then those run in `folder`, then the
    /// newest. Each command once, with its newest record.
    pub fn search(&self, typed: &str, folder: &str, now: i64, most: usize) -> Vec<&Record> {
        let mut seen = std::collections::HashSet::new();
        let mut found: Vec<(i64, usize, &Record)> = Vec::new();
        // Commands run anywhere in this folder's git repository come next
        // after those run in the folder itself (ADR 0288).
        let repo = crate::git::find(std::path::Path::new(folder)).map(|r| r.work);
        for (age, record) in self.all.iter().rev().enumerate() {
            if !seen.insert(record.command.as_str()) {
                continue;
            }
            let Some(score) = fuzzy(typed, &record.command) else { continue };
            let here = if record.folder == folder {
                40
            } else if repo
                .as_ref()
                .is_some_and(|r| std::path::Path::new(&record.folder).starts_with(r))
            {
                25
            } else {
                0
            };
            let hours = (now - record.time).max(0) / 3600;
            let fresh = 30 - hours.min(24 * 30) / 24;
            found.push((score + here + fresh, age, record));
        }
        found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        found.into_iter().take(most).map(|(_, _, r)| r).collect()
    }
}

/// How well `typed` matches `text`: none unless its letters (ignoring
/// case) come in `text` in order. Letters side by side, at a word's
/// start, and a match at the start score more; an empty `typed` matches
/// everything alike.
pub fn fuzzy(typed: &str, text: &str) -> Option<i64> {
    let typed: Vec<char> =
        typed.chars().flat_map(char::to_lowercase).filter(|c| !c.is_whitespace()).collect();
    if typed.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.chars().collect();
    let mut score = 0i64;
    let mut at = 0;
    let mut last: Option<usize> = None;
    for wanted in typed {
        let found =
            (at..text.len()).find(|&i| text[i].to_lowercase().eq(std::iter::once(wanted)))?;
        score += 1;
        if last.is_some_and(|l| l + 1 == found) {
            score += 5;
        }
        if found == 0 || !text[found - 1].is_alphanumeric() {
            score += 3;
        }
        last = Some(found);
        at = found + 1;
    }
    // Shorter commands for the same letters.
    Some(score * 4 - (text.len() as i64 / 8))
}

/// How a record ended, for the picker: `✗ 2` or nothing, and its time if
/// a second or more.
pub fn describe(record: &Record, now: i64) -> String {
    let mut parts = Vec::new();
    if record.status != 0 {
        parts.push(format!("✗ {}", record.status));
    }
    if record.millis >= 1000 {
        parts.push(duration(record.millis));
    }
    parts.push(ago(now - record.time));
    if !record.folder.is_empty() {
        parts.push(record.folder.clone());
    }
    parts.join(" · ")
}

/// `850ms`, `4.2s`, `3m 05s`, `1h 02m`.
pub fn duration(millis: u64) -> String {
    let seconds = millis / 1000;
    match seconds {
        0 => format!("{millis}ms"),
        1..=59 => format!("{}.{}s", seconds, (millis % 1000) / 100),
        60..=3599 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// `now`, `5m ago`, `3h ago`, `2d ago`.
fn ago(seconds: i64) -> String {
    match seconds {
        ..=59 => "now".into(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(time: i64, folder: &str, command: &str) -> Record {
        Record { time, status: 0, millis: 5, folder: folder.into(), command: command.into() }
    }

    #[test]
    fn records_read_back_as_written() {
        let r = Record {
            time: 1_700_000_000,
            status: 127,
            millis: 4200,
            folder: "/users/a/my\tdir".into(),
            command: "for x in 1 2\ndo echo \\$x; done".into(),
        };
        assert_eq!(Record::parse(r.line().trim_end_matches('\n')), Some(r));
        assert_eq!(Record::parse("junk"), None);
    }

    #[test]
    fn shells_share_the_file() {
        let dir = std::env::temp_dir().join(format!("nukesh-records-{}", std::process::id()));
        let home = dir.to_str().unwrap();
        let mut one = Records::new(Some(home));
        let mut two = Records::new(Some(home));
        one.add(&record(1, "/a", "make test"));
        one.refresh();
        two.refresh();
        assert_eq!(two.all.len(), 1);
        two.add(&record(2, "/b", "make clean"));
        one.refresh();
        assert_eq!(
            one.all.iter().map(|r| r.command.as_str()).collect::<Vec<_>>(),
            ["make test", "make clean"]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hints_prefer_the_folder() {
        let records = Records {
            all: vec![record(1, "/a", "make test"), record(2, "/b", "make clean")],
            ..Records::default()
        };
        assert_eq!(records.hint("make", "/a"), Some("make test"));
        assert_eq!(records.hint("make", "/c"), Some("make clean"));
        assert_eq!(records.hint("make clean", "/c"), None);
        assert_eq!(records.hint("", "/a"), None);
    }

    #[test]
    fn search_is_loose_and_ranked() {
        let records = Records {
            all: vec![
                record(100, "/a", "git status"),
                record(200, "/b", "grep -rn listen site/"),
                record(300, "/b", "git status"),
                record(400, "/b", "systemctl"),
            ],
            ..Records::default()
        };
        let found: Vec<&str> =
            records.search("gst", "/b", 500, 10).iter().map(|r| r.command.as_str()).collect();
        // grep -rn listen site/ has g, s and t in order too, further apart.
        assert_eq!(found, ["git status", "grep -rn listen site/"]);
        let found: Vec<&str> =
            records.search("", "/b", 500, 10).iter().map(|r| r.command.as_str()).collect();
        assert_eq!(found.len(), 3);
        assert!(fuzzy("lstn", "grep -rn listen").is_some());
        assert!(fuzzy("xyz", "grep").is_none());
        assert!(fuzzy("gs", "git status").unwrap() > fuzzy("gs", "grep -rn something").unwrap());
    }

    #[test]
    fn the_next_command_is_what_usually_comes_next() {
        let mut all = Vec::new();
        for (i, folder) in ["/p", "/p", "/q"].iter().enumerate() {
            let t = i as i64 * 10_000;
            all.push(record(t, folder, "git add -A"));
            all.push(record(t + 5, folder, "git commit"));
        }
        all.push(record(40_000, "/p", "git add -A"));
        all.push(record(40_001, "/p", "ls"));
        let records = Records { all, ..Records::default() };
        assert_eq!(records.next_after("git add -A", "/p"), Some("git commit"));
        // Once is not a habit.
        assert_eq!(records.next_after("git commit", "/p"), None);
        assert_eq!(records.next_after("", "/p"), None);
        // Too long after is not next.
        let records = Records {
            all: vec![
                record(0, "/p", "a"),
                record(5000, "/p", "b"),
                record(6000, "/p", "a"),
                record(9000, "/p", "b"),
            ],
            ..Records::default()
        };
        assert_eq!(records.next_after("a", "/p"), None);
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(duration(850), "850ms");
        assert_eq!(duration(4200), "4.2s");
        assert_eq!(duration(185_000), "3m 05s");
        assert_eq!(duration(3_720_000), "1h 02m");
    }
}
