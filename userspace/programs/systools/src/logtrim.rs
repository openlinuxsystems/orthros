// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! logtrim(8): trims the system's log (ADR 0211).
//!
//! logd(8) keeps the log in a file a day, and the security events of the
//! audit log in a file a day of their own; it trims both as
//! `/system/settings/log` says (log(5)): the log's records after 90 days,
//! the audit log's after a year, and the oldest of the log's files, then
//! the audit log's, while the two take more than 4 GiB or a tenth of
//! their file system. It trims when it starts, when a day begins, after
//! each 64 MiB it writes, and when asked: logtrim asks it, now or with
//! limits of its own for the log (not the audit log), as journalctl's
//! `--vacuum-size` and `--vacuum-time` do. The file being written is
//! never trimmed, and each trim is a security event in the audit log.

use serde::{Deserialize, Serialize};

use crate::args::{Arg, Args};
use crate::logd::{self, Request};
use crate::util::{self, Fail, fail, usage};

/// The settings file.
pub const SETTINGS: &str = "/system/settings/log";

/// How much of the log logd keeps, as the settings file sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Days the log keeps records; 0 for no limit.
    pub keep_days: u64,
    /// The most bytes the log and the audit log take together; 0 for no
    /// limit.
    pub max_bytes: u64,
    /// The most they take, in percent of their file system; 0 for no
    /// limit.
    pub max_percent: u64,
    /// Days the audit log keeps security events; 0 for no limit.
    pub audit_keep_days: u64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { keep_days: 90, max_bytes: 4 << 30, max_percent: 10, audit_keep_days: 365 }
    }
}

impl Limits {
    /// The limits settings file text `text` sets, the defaults for the
    /// rest, and what is wrong in it, a line each.
    pub fn parse(text: &str) -> (Limits, Vec<String>) {
        let mut limits = Limits::default();
        let mut wrong = Vec::new();
        for (at, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                wrong.push(format!("{SETTINGS}:{}: not `setting = value`", at + 1));
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            let number = match key {
                "max-size" => util::parse_size(value),
                _ => value.parse().ok(),
            };
            let slot = match key {
                "keep-days" => &mut limits.keep_days,
                "max-size" => &mut limits.max_bytes,
                "max-percent" => &mut limits.max_percent,
                "audit-keep-days" => &mut limits.audit_keep_days,
                _ => {
                    wrong.push(format!("{SETTINGS}:{}: no setting is called {key}", at + 1));
                    continue;
                }
            };
            match number {
                Some(n) if key != "max-percent" || n <= 100 => *slot = n,
                _ => wrong.push(format!("{SETTINGS}:{}: '{value}' is not a {key}", at + 1)),
            }
        }
        (limits, wrong)
    }

    /// The limits the settings file sets, and what is wrong in it.
    pub fn read() -> (Limits, Vec<String>) {
        match std::fs::read_to_string(SETTINGS) {
            Ok(text) => Limits::parse(&text),
            Err(_) => (Limits::default(), Vec::new()),
        }
    }

    /// The most bytes the log and the audit log may take on a file system
    /// of `fs_size` bytes; 0 for no limit.
    pub fn cap(&self, fs_size: u64) -> u64 {
        let share = match (self.max_percent, fs_size) {
            (0, _) | (_, 0) => 0,
            (percent, size) => (u128::from(size) * u128::from(percent) / 100) as u64,
        };
        match (self.max_bytes, share) {
            (0, cap) | (cap, 0) => cap,
            (a, b) => a.min(b),
        }
    }
}

/// What one trim goes by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// Days the log keeps: of 0, today's alone; None for no limit.
    pub days: Option<u64>,
    /// Days the audit log keeps; None for no limit.
    pub audit_days: Option<u64>,
    /// The most bytes both take; 0 for no limit.
    pub cap: u64,
    /// The most bytes the log takes, without the audit log; 0 for no
    /// limit.
    pub log_cap: u64,
}

/// What a trim removed, or would: the day files, the audit log's
/// as `audit/YYYY-MM-DD.jsonl`, and their bytes; and the bytes the log
/// and the audit log take after it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trimmed {
    pub files: Vec<String>,
    pub bytes: u64,
    pub left: u64,
}

/// The day of day file `name` (`YYYY-MM-DD.jsonl`), in days since the
/// epoch.
fn day_of(name: &str) -> Option<i64> {
    let year = name.get(..4)?.parse().ok()?;
    let month = name.get(5..7)?.parse().ok()?;
    let day = name.get(8..10)?.parse().ok()?;
    Some(logd::days(year, month, day))
}

/// The day files to remove of `log` and `audit`, each (name, bytes),
/// oldest first, to keep to `policy` on day `today` (days since the
/// epoch): first the ones past their days, then the oldest of the log,
/// then of the audit log, while they take more than the caps. Never the
/// one of `today`, nor the ones `writing`.
pub fn plan(
    log: &[(String, u64)],
    audit: &[(String, u64)],
    today: i64,
    policy: &Policy,
    writing: &[&str],
) -> Vec<(String, u64)> {
    let today_name = logd::day_file(today as u64 * 86_400 * 1_000_000_000);
    let kept = |name: &str| name == today_name || writing.contains(&name);
    let old = |name: &str, days: Option<u64>| {
        days.is_some_and(|days| {
            !kept(name) && day_of(name).is_some_and(|day| day < today.saturating_sub_unsigned(days))
        })
    };
    let mut doomed: Vec<(String, u64)> = Vec::new();
    let mut log_left: Vec<&(String, u64)> = Vec::new();
    let mut audit_left: Vec<&(String, u64)> = Vec::new();
    for file in log {
        if old(&file.0, policy.days) { doomed.push(file.clone()) } else { log_left.push(file) }
    }
    for file in audit {
        if old(&file.0, policy.audit_days) {
            doomed.push((format!("{}/{}", logd::AUDIT, file.0), file.1));
        } else {
            audit_left.push(file);
        }
    }
    let mut log_bytes: u64 = log_left.iter().map(|f| f.1).sum();
    let mut total = log_bytes + audit_left.iter().map(|f| f.1).sum::<u64>();
    let over = |total: u64, log_bytes: u64| {
        policy.cap > 0 && total > policy.cap || policy.log_cap > 0 && log_bytes > policy.log_cap
    };
    for file in log_left {
        if !over(total, log_bytes) {
            break;
        }
        if !kept(&file.0) {
            doomed.push(file.clone());
            total -= file.1;
            log_bytes -= file.1;
        }
    }
    for file in audit_left {
        if policy.cap == 0 || total <= policy.cap {
            break;
        }
        if !kept(&file.0) {
            doomed.push((format!("{}/{}", logd::AUDIT, file.0), file.1));
            total -= file.1;
        }
    }
    doomed
}

/// logtrim(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut request = Request { ask: "trim".into(), ..Request::default() };
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('n') => request.dry_run = true,
            Arg::Long(l) if l == "dry-run" => request.dry_run = true,
            Arg::Short('s') => request.keep_bytes = Some(size(&args.text()?)?),
            Arg::Long(l) if l == "size" => request.keep_bytes = Some(size(&args.text()?)?),
            Arg::Short('d') => request.keep_days = Some(days(&args.text()?)?),
            Arg::Long(l) if l == "days" => request.keep_days = Some(days(&args.text()?)?),
            other => return Err(args.unknown(&other)),
        }
    }
    let trimmed = ask(&request)?;
    report(&trimmed, request.dry_run, false);
    Ok(0)
}

/// A size of `--size` or journalctl's `--vacuum-size`.
pub fn size(text: &str) -> Result<u64, Fail> {
    util::parse_size(text).ok_or_else(|| usage(format!("'{text}' is not a size")))
}

/// A number of days of `--days`.
fn days(text: &str) -> Result<u64, Fail> {
    text.parse().map_err(|_| usage(format!("'{text}' is not a number of days")))
}

/// Sends trim request `request` to logd and returns its answer.
pub fn ask(request: &Request) -> Result<Trimmed, Fail> {
    let mut trimmed = None;
    logd::ask(request, |line| {
        trimmed = serde_json::from_str::<Trimmed>(line).ok();
        Ok(false)
    })?;
    trimmed.ok_or_else(|| fail("logd did not say what it trimmed".into()))
}

/// Says what was trimmed, as logtrim does or, for `journalctl`, as
/// journalctl's `--vacuum-*` do.
pub fn report(trimmed: &Trimmed, dry_run: bool, journalctl: bool) {
    let verb = if dry_run { "Would remove" } else { "Removed" };
    for file in &trimmed.files {
        println!("{verb} {}/{file}", logd::DIRECTORY);
    }
    let freed = util::human(trimmed.bytes, false);
    let left = util::human(trimmed.left, false);
    if journalctl {
        println!("Vacuuming done, freed {freed} of archived journals from {}.", logd::DIRECTORY);
    } else if dry_run {
        println!("Would free {freed}; the log would take up {left}.");
    } else {
        println!("Freed {freed}; the log takes up {left}.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[(&str, u64)]) -> Vec<(String, u64)> {
        names.iter().map(|(n, b)| (n.to_string(), *b)).collect()
    }

    #[test]
    fn settings() {
        let (limits, wrong) = Limits::parse(
            "# comment\nkeep-days = 30\nmax-size = 512M\nmax-percent=5\naudit-keep-days = 0\n",
        );
        assert!(wrong.is_empty(), "{wrong:?}");
        assert_eq!(
            limits,
            Limits { keep_days: 30, max_bytes: 512 << 20, max_percent: 5, audit_keep_days: 0 }
        );
        let (limits, wrong) = Limits::parse("keep-days = soon\nmax-percent = 200\nloud = 1\nx\n");
        assert_eq!(limits, Limits::default());
        assert_eq!(wrong.len(), 4);
        assert!(wrong[2].contains("no setting is called loud"));
        assert_eq!(Limits::default().cap(100 << 30), 4 << 30);
        assert_eq!(Limits::default().cap(10 << 30), 1 << 30);
        assert_eq!(Limits::default().cap(0), 4 << 30);
        assert_eq!(Limits { max_bytes: 0, max_percent: 0, ..Limits::default() }.cap(1 << 30), 0);
    }

    #[test]
    fn what_goes() {
        let today = logd::days(2026, 10, 30);
        let log = files(&[
            ("2026-07-01.jsonl", 10),
            ("2026-08-01.jsonl", 20),
            ("2026-10-29.jsonl", 30),
            ("2026-10-30.jsonl", 40),
        ]);
        let audit = files(&[("2025-10-01.jsonl", 1), ("2026-10-30.jsonl", 2)]);
        let names = |doomed: Vec<(String, u64)>| -> Vec<String> {
            doomed.into_iter().map(|f| f.0).collect()
        };
        // By days: the log's of July, and the audit log's of a year ago.
        let policy = Policy { days: Some(90), audit_days: Some(365), ..Policy::default() };
        assert_eq!(
            names(plan(&log, &audit, today, &policy, &[])),
            ["2026-07-01.jsonl", "audit/2025-10-01.jsonl"]
        );
        // By space: the oldest of the log first, then of the audit log;
        // never today's, nor the one being written.
        let policy = Policy { cap: 75, ..Policy::default() };
        assert_eq!(
            names(plan(&log, &audit, today, &policy, &[])),
            ["2026-07-01.jsonl", "2026-08-01.jsonl"]
        );
        let policy = Policy { cap: 1, ..Policy::default() };
        let writing = ["2026-10-29.jsonl"];
        assert_eq!(
            names(plan(&log, &audit, today, &policy, &writing)),
            ["2026-07-01.jsonl", "2026-08-01.jsonl", "audit/2025-10-01.jsonl"]
        );
        // journalctl's --vacuum-size: the log alone.
        let policy = Policy { log_cap: 70, ..Policy::default() };
        assert_eq!(
            names(plan(&log, &audit, today, &policy, &[])),
            ["2026-07-01.jsonl", "2026-08-01.jsonl"]
        );
        assert!(plan(&log, &audit, today, &Policy::default(), &[]).is_empty());
        // Days of 0: every day file of the log but today's.
        let policy = Policy { days: Some(0), ..Policy::default() };
        assert_eq!(
            names(plan(&log, &audit, today, &policy, &[])),
            ["2026-07-01.jsonl", "2026-08-01.jsonl", "2026-10-29.jsonl"]
        );
    }
}
