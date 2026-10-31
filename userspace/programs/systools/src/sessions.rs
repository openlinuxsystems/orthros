// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! who(1), w(1), users(1) and uptime(1). Radix keeps no utmp: a session
//! is a session leader with a controlling terminal, as `/processes` shows it,
//! logged in when its leader started, by the user it acts for.

use std::ffi::CStr;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;

use crate::args::{Arg, Args};
use crate::proc::{self, Process};
use crate::util::{self, Fail};

/// A login session.
struct Session {
    leader: Process,
    terminal: String,
    /// The name of the user its leader acts for.
    user: String,
}

/// The sessions with a terminal, by terminal.
fn sessions(processes: &[Process]) -> Vec<Session> {
    let mut sessions: Vec<Session> = processes
        .iter()
        .filter(|p| p.number == p.session && !p.zombie())
        .filter_map(|p| {
            let terminal = p.terminal.clone()?;
            Some(Session { leader: p.clone(), terminal, user: util::user_name(p.user) })
        })
        .collect();
    sessions.sort_by(|a, b| a.terminal.cmp(&b.terminal));
    sessions
}

/// When the system started, in seconds since the epoch.
pub fn boot_seconds() -> i64 {
    let boot = proc::stat().map_or(0, |s| s.boot_time_ns);
    let boot = if boot == 0 { util::now_ns().saturating_sub(proc::uptime_ns()) } else { boot };
    (boot / 1_000_000_000) as i64
}

/// When a session began, in seconds since the epoch.
fn login_seconds(session: &Session, boot: i64) -> i64 {
    boot + (session.leader.started_ns / 1_000_000_000) as i64
}

/// How long since the terminal was last used, in seconds, if known.
fn idle_seconds(terminal: &str) -> Option<u64> {
    let meta = std::fs::metadata(format!("{}/{terminal}", util::DEVICES)).ok()?;
    let last = meta.atime().max(meta.mtime());
    if last <= 0 {
        return None;
    }
    let now = (util::now_ns() / 1_000_000_000) as i64;
    Some(now.saturating_sub(last).max(0) as u64)
}

/// The terminal standard input is, as `pts/N`.
fn own_terminal() -> Option<String> {
    // SAFETY: ttyname returns a C string the C library keeps, or null.
    let name = unsafe { libc::ttyname(0) };
    if name.is_null() {
        return None;
    }
    // SAFETY: a non-null result is a C string.
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned();
    Some(util::terminal_name(&name).to_string())
}

/// uptime(1)'s line: the time, how long the system has been up, the
/// users and the load averages.
fn uptime_line(users: usize) -> String {
    let now = (util::now_ns() / 1_000_000_000) as i64;
    let up = proc::uptime_ns() / 1_000_000_000;
    let load = proc::loadavg();
    format!(
        " {} up {},  {users} user{},  load average: {:.2}, {:.2}, {:.2}",
        util::local_time(now, "%H:%M:%S"),
        util::up_for(up),
        if users > 1 { "s" } else { "" },
        load[0],
        load[1],
        load[2]
    )
}

/// uptime(1).
pub fn uptime(args: &mut Args) -> Result<i32, Fail> {
    let mut pretty = false;
    let mut since = false;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('p') => pretty = true,
            Arg::Long(l) if l == "pretty" => pretty = true,
            Arg::Short('s') => since = true,
            Arg::Long(l) if l == "since" => since = true,
            Arg::Long(l) if l == "help" => {
                println!("usage: uptime [-p|-s]");
                return Ok(0);
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let up = proc::uptime_ns() / 1_000_000_000;
    if since {
        println!("{}", util::local_time(boot_seconds(), "%Y-%m-%d %H:%M:%S"));
    } else if pretty {
        let parts = [
            (up / 604800, "week"),
            (up % 604800 / 86400, "day"),
            (up % 86400 / 3600, "hour"),
            (up % 3600 / 60, "minute"),
        ];
        let words: Vec<String> = parts
            .iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, unit)| format!("{n} {unit}{}", if *n == 1 { "" } else { "s" }))
            .collect();
        let words = if words.is_empty() { vec!["0 minutes".to_string()] } else { words };
        println!("up {}", words.join(", "));
    } else {
        let processes = proc::processes().unwrap_or_default();
        println!("{}", uptime_line(sessions(&processes).len()));
    }
    Ok(0)
}

/// users(1).
pub fn users(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    let processes = proc::processes().map_err(|e| util::failed(&proc::PROC, e))?;
    // Sorted, as users(1) on Linux lists them.
    let mut names: Vec<String> = sessions(&processes).into_iter().map(|s| s.user).collect();
    names.sort();
    println!("{}", names.join(" "));
    Ok(0)
}

/// who(1).
pub fn who(args: &mut Args) -> Result<i32, Fail> {
    let mut heading = false;
    let mut boot = false;
    let mut count = false;
    let mut idle = false;
    let mut only_mine = false;
    let mut users_too = true;
    let mut operands = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('H') => heading = true,
            Arg::Long(l) if l == "heading" => heading = true,
            Arg::Short('b') => {
                boot = true;
                users_too = false;
            }
            Arg::Long(l) if l == "boot" => {
                boot = true;
                users_too = false;
            }
            Arg::Short('q') => count = true,
            Arg::Long(l) if l == "count" => count = true,
            Arg::Short('u') => idle = true,
            Arg::Long(l) if l == "users" => idle = true,
            Arg::Short('a') => {
                boot = true;
                idle = true;
            }
            Arg::Long(l) if l == "all" => {
                boot = true;
                idle = true;
            }
            Arg::Short('m') => only_mine = true,
            Arg::Short('s' | 'T' | 'w' | 'l' | 'p' | 'r' | 'd' | 't') => {}
            Arg::Operand(word) => operands.push(word.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    // `who am i`, `who mom likes`: only the caller's own line.
    if operands.len() == 2 {
        only_mine = true;
    }
    let processes = proc::processes().map_err(|e| util::failed(&proc::PROC, e))?;
    let mut list = sessions(&processes);
    if only_mine {
        let mine = own_terminal();
        list.retain(|s| Some(&s.terminal) == mine.as_ref());
    }
    let mut out = io::stdout().lock();
    if count {
        let names: Vec<&str> = list.iter().map(|s| s.user.as_str()).collect();
        writeln!(out, "{}", names.join(" "))?;
        writeln!(out, "# users={}", list.len())?;
        return Ok(0);
    }
    let boot_s = boot_seconds();
    if heading {
        if idle {
            writeln!(
                out,
                "{:<8} {:<12} {:<16} {:<5} {:>10}",
                "NAME", "LINE", "TIME", "IDLE", "PID"
            )?;
        } else {
            writeln!(out, "{:<8} {:<12} TIME", "NAME", "LINE")?;
        }
    }
    if boot {
        writeln!(
            out,
            "{:<8} {:<12} {}",
            "",
            "system boot",
            util::local_time(boot_s, "%Y-%m-%d %H:%M")
        )?;
    }
    if users_too {
        for s in &list {
            let when = util::local_time(login_seconds(s, boot_s), "%Y-%m-%d %H:%M");
            if idle {
                let idle = match idle_seconds(&s.terminal) {
                    Some(seconds) if seconds < 60 => ".".to_string(),
                    Some(seconds) if seconds < 86400 => {
                        format!("{:02}:{:02}", seconds / 3600, seconds % 3600 / 60)
                    }
                    Some(_) => "old".to_string(),
                    None => "?".to_string(),
                };
                writeln!(
                    out,
                    "{:<8} {:<12} {when} {idle:<5} {:>10}",
                    s.user, s.terminal, s.leader.number
                )?;
            } else {
                writeln!(out, "{:<8} {:<12} {when}", s.user, s.terminal)?;
            }
        }
    }
    Ok(0)
}

/// An idle time as w(1) shows it.
fn w_idle(seconds: Option<u64>) -> String {
    match seconds {
        None => "?".into(),
        Some(s) if s < 60 => format!("{s}.00s"),
        Some(s) if s < 3600 => format!("{}:{:02}", s / 60, s % 60),
        Some(s) if s < 86400 * 2 => format!("{}:{:02}m", s / 3600, s % 3600 / 60),
        Some(s) => format!("{}days", s / 86400),
    }
}

/// A CPU time as w(1) shows it.
fn w_cpu(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    if s < 60 {
        format!("{}.{:02}s", s, ns % 1_000_000_000 / 10_000_000)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// w(1).
pub fn w(args: &mut Args) -> Result<i32, Fail> {
    let mut header = true;
    let mut short = false;
    let mut from = true;
    let mut user_filter = None;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('h') => header = false,
            Arg::Long(l) if l == "no-header" => header = false,
            Arg::Short('s') => short = true,
            Arg::Long(l) if l == "short" => short = true,
            Arg::Short('f') => from = !from,
            Arg::Long(l) if l == "from" => from = !from,
            Arg::Short('u' | 'i' | 'o' | 'p') => {}
            Arg::Operand(user) => user_filter = Some(user.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    let processes = proc::processes().map_err(|e| util::failed(&proc::PROC, e))?;
    let mut list = sessions(&processes);
    if let Some(wanted) = &user_filter {
        list.retain(|s| s.user == *wanted || s.leader.user.to_string() == *wanted);
    }
    let mut out = io::stdout().lock();
    if header {
        writeln!(out, "{}", uptime_line(list.len()))?;
        // procps-ng's columns: the names 9 wide, the host 16, the times 7.
        let mut line = format!("{:<9}{:<9}", "USER", "TTY");
        if from {
            line += &format!("{:<16}", "FROM");
        }
        if short {
            line += &format!("{:>7} {}", "IDLE", "WHAT");
        } else {
            line += &format!(" {:<6}{:>7}{:>7}{:>7}  {}", "LOGIN@", "IDLE", "JCPU", "PCPU", "WHAT");
        }
        writeln!(out, "{line}")?;
    }
    let boot_s = boot_seconds();
    let now = (util::now_ns() / 1_000_000_000) as i64;
    for s in &list {
        // Its processes; the one it runs now is the newest of them.
        let on: Vec<&Process> = processes
            .iter()
            .filter(|p| p.terminal.as_ref() == Some(&s.terminal) && !p.zombie())
            .collect();
        let jcpu: u64 = on.iter().map(|p| p.cpu_ns()).sum();
        let current = on.iter().max_by_key(|p| p.started_ns).copied().unwrap_or(&s.leader);
        let what = {
            let args = proc::cmdline(current.number);
            if args.is_empty() {
                current.name.clone()
            } else {
                args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
            }
        };
        let login = login_seconds(s, boot_s);
        let login = if now - login < 86400 {
            util::local_time(login, "%H:%M")
        } else {
            util::local_time(login, "%d%b%y")
        };
        let mut line = format!("{:<8} {:<8} ", s.user, s.terminal);
        if from {
            line += &format!("{:<16}", "-");
        }
        let idle = w_idle(idle_seconds(&s.terminal));
        if short {
            line += &format!("{idle:>7} {what}");
        } else {
            line += &format!(
                " {login:<6}{idle:>7}{:>7}{:>7}  {what}",
                w_cpu(jcpu),
                w_cpu(current.cpu_ns())
            );
        }
        writeln!(out, "{line}")?;
    }
    Ok(0)
}
