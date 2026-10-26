// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! kill(1), and pgrep(1) and pkill(1), which find processes by name and
//! other attributes, as procps-ng's do.

use std::io::{self, Write};

use crate::args::{Arg, Args};
use crate::proc::{self, Process};
use crate::util::{self, Fail, Regex, fail, usage};

/// Sends `signal` to process `pid` (a group if negative).
fn send(pid: i64, signal: i32) -> io::Result<()> {
    // SAFETY: kill reads nothing of the program's.
    if unsafe { libc::kill(pid as libc::pid_t, signal) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Lists the signals: their names in a row, or with `table` a table of
/// numbers and names.
fn list_signals(table: bool) -> io::Result<()> {
    let mut out = io::stdout().lock();
    if table {
        for (i, (number, name)) in util::SIGNALS.iter().enumerate() {
            write!(out, "{number:2} {name:<8}")?;
            if i % 7 == 6 {
                writeln!(out)?;
            }
        }
        writeln!(out)
    } else {
        let names: Vec<&str> = util::SIGNALS.iter().map(|(_, name)| *name).collect();
        writeln!(out, "{}", names.join(" "))
    }
}

/// kill(1): `kill [-s SIGNAL | -SIGNAL] PID...`, `kill -l [STATUS]`.
pub fn kill(args: &mut Args) -> Result<i32, Fail> {
    let mut signal = libc::SIGTERM;
    let mut pids: Vec<String> = Vec::new();
    let mut listing = None;
    let mut list_arg = None;
    // `-9` and `-KILL` are options here, and `-123` after `--` a group.
    let items: Vec<String> =
        args.rest().into_iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let mut items = items.into_iter().peekable();
    let mut options = true;
    let mut signal_given = false;
    while let Some(item) = items.next() {
        // After the signal, `-123` is process group 123.
        let group = signal_given
            && item
                .strip_prefix('-')
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if !options || !item.starts_with('-') || item == "-" || group {
            pids.push(item);
            options = false;
            continue;
        }
        signal_given = true;
        match item.as_str() {
            "--" => options = false,
            "-l" | "--list" => listing = Some(false),
            "-L" | "--table" => listing = Some(true),
            "-s" | "--signal" | "-n" => {
                let name = items
                    .next()
                    .ok_or_else(|| usage("option requires an argument -- 's'".into()))?;
                signal = util::signal_number(&name)
                    .ok_or_else(|| usage(format!("{name}: invalid signal specification")))?;
            }
            "-a" | "--all" | "-p" | "--pid" => {}
            "-h" | "--help" => {
                println!("usage: kill [-s SIGNAL | -SIGNAL] PID...\n       kill -l [STATUS]");
                return Ok(0);
            }
            _ => {
                if let Some(l) = item.strip_prefix("-l").filter(|r| !r.is_empty()) {
                    listing = Some(false);
                    list_arg = Some(l.to_string());
                    continue;
                }
                let name = &item[1..];
                signal = util::signal_number(name)
                    .ok_or_else(|| usage(format!("{name}: invalid signal specification")))?;
            }
        }
    }
    if let Some(table) = listing {
        let arg = list_arg.or_else(|| pids.first().cloned());
        if let Some(arg) = arg {
            // A number (or an exit status above 128) names a signal; a
            // name gives its number.
            if let Ok(number) = arg.parse::<i32>() {
                let number = if number > 128 { number - 128 } else { number };
                let name = util::signal_name(number)
                    .ok_or_else(|| fail(format!("{arg}: invalid signal specification")))?;
                println!("{name}");
            } else {
                let number = util::signal_number(&arg)
                    .ok_or_else(|| fail(format!("{arg}: invalid signal specification")))?;
                println!("{number}");
            }
            return Ok(0);
        }
        list_signals(table)?;
        return Ok(0);
    }
    if pids.is_empty() {
        return Err(usage("usage: kill [-s SIGNAL | -SIGNAL] PID...".into()));
    }
    let mut code = 0;
    for pid in &pids {
        let number: i64 = match pid.parse() {
            Ok(n) => n,
            Err(_) => {
                if pid.starts_with('%') {
                    eprintln!("kill: {pid}: job specifications need the shell's kill");
                } else {
                    eprintln!("kill: {pid}: arguments must be process or job IDs");
                }
                code = 1;
                continue;
            }
        };
        if let Err(e) = send(number, signal) {
            eprintln!("kill: ({number}) - {}", util::strip_os_error(&e));
            code = 1;
        }
    }
    Ok(code)
}

/// What pgrep and pkill match processes by: all of what is given.
#[derive(Default)]
struct Match {
    pattern: Option<Regex>,
    full: bool,
    exact: bool,
    newest: bool,
    oldest: bool,
    parents: Vec<u64>,
    groups: Vec<u64>,
    sessions: Vec<u64>,
    terminals: Vec<String>,
    users: Vec<u32>,
    states: Option<String>,
    older: Option<u64>,
    inverse: bool,
}

impl Match {
    fn matches(&self, p: &Process, own: u64) -> bool {
        // Never pgrep itself.
        if p.number == own {
            return false;
        }
        let mut ok = true;
        if let Some(pattern) = &self.pattern {
            let text = if self.full { command_line(p) } else { p.name.clone() };
            ok &= pattern.is_match(&text);
        }
        ok &= self.parents.is_empty() || self.parents.contains(&p.parent);
        ok &= self.groups.is_empty() || self.groups.contains(&p.group);
        ok &= self.sessions.is_empty() || self.sessions.contains(&p.session);
        ok &= self.users.is_empty() || self.users.contains(&0);
        ok &= self.terminals.is_empty()
            || p.terminal.as_ref().is_some_and(|t| {
                self.terminals.iter().any(|want| want == t || format!("pts/{want}") == *t)
            });
        if let Some(states) = &self.states {
            ok &= states.contains(p.state_letter());
        }
        if let Some(seconds) = self.older {
            let age = util::monotonic_ns().saturating_sub(p.started_ns) / 1_000_000_000;
            ok &= age >= seconds;
        }
        ok != self.inverse
    }
}

/// Numbers from a list, with 0 meaning pgrep's own (as procps-ng has it
/// for groups and sessions).
fn numbers(text: &str, own: u64) -> Result<Vec<u64>, Fail> {
    util::list(text)
        .map(|n| match n.parse::<u64>() {
            Ok(0) => Ok(own),
            Ok(n) => Ok(n),
            Err(_) => Err(usage(format!("invalid argument: '{n}'"))),
        })
        .collect()
}

/// pgrep(1) and pkill(1).
fn grep(args: &mut Args, kill: bool) -> Result<i32, Fail> {
    let mut m = Match::default();
    let mut signal = libc::SIGTERM;
    let mut delimiter = "\n".to_string();
    let mut list_name = false;
    let mut list_full = false;
    let mut count = false;
    let mut echo = false;
    let mut ignore_case = false;
    let mut pattern: Option<String> = None;
    let me = proc::process(proc::own_number()).unwrap_or_default();
    // pkill takes `-SIGNAL` first, as kill does.
    if kill {
        let rest = args.rest();
        let mut rest = rest.into_iter().peekable();
        let mut kept = Vec::new();
        if let Some(first) = rest.peek().map(|a| a.to_string_lossy().into_owned())
            && let Some(name) = first.strip_prefix('-')
            && let Some(number) = util::signal_number(name)
        {
            signal = number;
            rest.next();
        }
        kept.extend(rest);
        *args = Args::new(args.tool, kept);
    }
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('f') => m.full = true,
            Arg::Long(l) if l == "full" => m.full = true,
            Arg::Short('x') => m.exact = true,
            Arg::Long(l) if l == "exact" => m.exact = true,
            Arg::Short('i') => ignore_case = true,
            Arg::Long(l) if l == "ignore-case" => ignore_case = true,
            Arg::Short('n') => m.newest = true,
            Arg::Long(l) if l == "newest" => m.newest = true,
            Arg::Short('o') => m.oldest = true,
            Arg::Long(l) if l == "oldest" => m.oldest = true,
            Arg::Short('v') => m.inverse = true,
            Arg::Long(l) if l == "inverse" => m.inverse = true,
            Arg::Short('P') => m.parents.extend(numbers(&args.text()?, me.number)?),
            Arg::Long(l) if l == "parent" => m.parents.extend(numbers(&args.text()?, me.number)?),
            Arg::Short('g') => m.groups.extend(numbers(&args.text()?, me.group)?),
            Arg::Long(l) if l == "pgroup" => m.groups.extend(numbers(&args.text()?, me.group)?),
            Arg::Short('s') => m.sessions.extend(numbers(&args.text()?, me.session)?),
            Arg::Long(l) if l == "session" => {
                m.sessions.extend(numbers(&args.text()?, me.session)?);
            }
            Arg::Short('t') => m.terminals.extend(util::list(&args.text()?).map(String::from)),
            Arg::Long(l) if l == "terminal" => {
                m.terminals.extend(util::list(&args.text()?).map(String::from));
            }
            Arg::Short('u' | 'U') => {
                for name in util::list(&args.text()?) {
                    m.users.push(
                        util::user_id(name)
                            .ok_or_else(|| usage(format!("invalid user name: {name}")))?,
                    );
                }
            }
            Arg::Short('r') => m.states = Some(args.text()?),
            Arg::Long(l) if l == "runstates" => m.states = Some(args.text()?),
            Arg::Short('O') => m.older = Some(args.number()?),
            Arg::Long(l) if l == "older" => m.older = Some(args.number()?),
            Arg::Short('d') if !kill => delimiter = args.text()?,
            Arg::Long(l) if l == "delimiter" && !kill => delimiter = args.text()?,
            Arg::Short('l') if !kill => list_name = true,
            Arg::Long(l) if l == "list-name" && !kill => list_name = true,
            Arg::Short('a') if !kill => list_full = true,
            Arg::Long(l) if l == "list-full" && !kill => list_full = true,
            Arg::Short('c') => count = true,
            Arg::Long(l) if l == "count" => count = true,
            Arg::Short('e') if kill => echo = true,
            Arg::Long(l) if l == "echo" && kill => echo = true,
            Arg::Long(l) if l == "signal" => {
                let name = args.text()?;
                signal = util::signal_number(&name)
                    .ok_or_else(|| usage(format!("Unknown signal \"{name}\".")))?;
            }
            Arg::Operand(p) if pattern.is_none() => {
                pattern = Some(p.to_string_lossy().into_owned());
            }
            Arg::Operand(_) => return Err(usage("only one pattern can be provided".into())),
            other => return Err(args.unknown(&other)),
        }
    }
    let selected = pattern.is_some()
        || !m.parents.is_empty()
        || !m.groups.is_empty()
        || !m.sessions.is_empty()
        || !m.terminals.is_empty()
        || !m.users.is_empty()
        || m.states.is_some()
        || m.older.is_some();
    if !selected {
        return Err(usage("no matching criteria specified".into()));
    }
    if let Some(pattern) = pattern {
        let pattern = if m.exact { format!("^({pattern})$") } else { pattern };
        m.pattern = Some(
            Regex::new(&pattern, ignore_case).map_err(|e| usage(format!("bad pattern: {e}")))?,
        );
    }
    let own = proc::own_number();
    let mut found: Vec<Process> = proc::processes()
        .map_err(|e| util::fail_with(3, format!("{}: {e}", proc::PROC)))?
        .into_iter()
        .filter(|p| m.matches(p, own))
        .collect();
    if m.newest {
        found.sort_by_key(|p| p.started_ns);
        found = found.pop().into_iter().collect();
    } else if m.oldest {
        found.sort_by_key(|p| p.started_ns);
        found.truncate(1);
    }
    let mut out = io::stdout().lock();
    if kill {
        let mut sent = 0;
        for p in &found {
            match send(p.number as i64, signal) {
                Ok(()) => {
                    sent += 1;
                    if echo {
                        writeln!(out, "{} killed (pid {})", p.name, p.number)?;
                    }
                }
                Err(e) => eprintln!(
                    "pkill: killing pid {} failed: {}",
                    p.number,
                    util::strip_os_error(&e)
                ),
            }
        }
        if count {
            writeln!(out, "{sent}")?;
        }
        return Ok(i32::from(sent <= 0));
    }
    if count {
        writeln!(out, "{}", found.len())?;
    } else {
        let items: Vec<String> = found
            .iter()
            .map(|p| {
                if list_full {
                    format!("{} {}", p.number, command_line(p))
                } else if list_name {
                    format!("{} {}", p.number, p.name)
                } else {
                    p.number.to_string()
                }
            })
            .collect();
        if !items.is_empty() {
            writeln!(out, "{}", items.join(&delimiter))?;
        }
    }
    Ok(i32::from(found.is_empty()))
}

/// The command line -f matches and -a prints, as procps has it: the
/// arguments, or as ps shows a process without any, `[name]`, and
/// `[name] <defunct>` for a zombie, which pgrep finds too.
fn command_line(p: &Process) -> String {
    if p.zombie() {
        return format!("[{}] <defunct>", p.name);
    }
    let args = proc::cmdline(p.number);
    if args.is_empty() {
        format!("[{}]", p.name)
    } else {
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
    }
}

pub fn pgrep(args: &mut Args) -> Result<i32, Fail> {
    grep(args, false)
}

pub fn pkill(args: &mut Args) -> Result<i32, Fail> {
    grep(args, true)
}
