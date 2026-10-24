// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ps(1): a snapshot of the processes, from `/proc`, with the options of
//! procps-ng's ps, Linux's: the UNIX options (`-ef`, `-o`), the BSD ones
//! without a dash (`aux`) and the GNU long ones (`--sort`, `--forest`).

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{self, Write};

use crate::args::{Arg, Args};
use crate::proc::{self, Process, Thread};
use crate::util::{self, Fail, usage};

/// A column ps can show: its key, its header, and whether it is a
/// number, which lines up on the right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    name: &'static str,
    header: &'static str,
    right: bool,
}

/// The columns, under each name ps(1) knows them by.
const KEYS: &[(&[&str], &str, bool)] = &[
    (&["pid", "tgid"], "PID", true),
    (&["ppid"], "PPID", true),
    (&["pgid", "pgrp"], "PGID", true),
    (&["sid", "sess", "session"], "SID", true),
    (&["tty", "tt", "tname"], "TT", false),
    (&["stat"], "STAT", false),
    (&["s", "state"], "S", false),
    (&["time", "cputime"], "TIME", true),
    (&["bsdtime"], "TIME", true),
    (&["args", "cmd", "command"], "COMMAND", false),
    (&["comm", "ucmd", "ucomm"], "COMMAND", false),
    (&["uid", "euid", "ruid", "suid", "fsuid"], "UID", true),
    (&["user", "euser", "uname", "ruser", "suser", "fsuser"], "USER", false),
    (&["gid", "egid", "rgid"], "GID", true),
    (&["group", "egroup", "rgroup"], "GROUP", false),
    (&["c", "cp"], "C", true),
    (&["pcpu", "%cpu"], "%CPU", true),
    (&["pmem", "%mem"], "%MEM", true),
    (&["vsz", "vsize"], "VSZ", true),
    (&["rss", "rssize", "rsz"], "RSS", true),
    (&["sz"], "SZ", true),
    (&["stime", "start_time"], "STIME", false),
    (&["start", "bsdstart"], "START", false),
    (&["lstart"], "STARTED", false),
    (&["ni", "nice"], "NI", true),
    (&["pri"], "PRI", true),
    (&["priority"], "PRI", true),
    (&["rtprio"], "RTPRIO", true),
    (&["cls", "class", "policy", "sched"], "CLS", false),
    (&["psr", "cpu"], "PSR", true),
    (&["wchan", "nwchan"], "WCHAN", false),
    (&["lwp", "tid", "spid"], "LWP", true),
    (&["nlwp", "thcount"], "NLWP", true),
    (&["etime"], "ELAPSED", true),
    (&["etimes"], "ELAPSED", true),
    (&["majflt", "maj_flt"], "MAJFLT", true),
    (&["minflt", "min_flt"], "MINFLT", true),
    (&["f", "flag", "flags"], "F", true),
    (&["addr"], "ADDR", false),
    (&["tpgid"], "TPGID", true),
    (&["ioclass"], "IOCLASS", false),
    (&["domain"], "DOMAIN", false),
];

/// The column `name` names.
fn key(name: &str) -> Option<Key> {
    let lower = name.to_ascii_lowercase();
    KEYS.iter()
        .find(|(names, ..)| names.contains(&lower.as_str()))
        .map(|(names, header, right)| Key { name: names[0], header, right: *right })
}

/// A column to show, with its header.
#[derive(Clone, Debug)]
struct Column {
    key: Key,
    header: String,
}

/// Parses a format list (`-o pid,user=WHO,args`): keys separated by
/// commas or blanks, each with an optional `=header`, which runs to the
/// end of the list.
fn format(text: &str) -> Result<Vec<Column>, Fail> {
    let mut columns = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        let end = rest.find([',', ' ', '\t', '=']).unwrap_or(rest.len());
        let name = &rest[..end];
        let k = key(name)
            .ok_or_else(|| usage(format!("unknown user-defined format specifier \"{name}\"")))?;
        let mut header = k.header.to_string();
        rest = &rest[end..];
        if let Some(after) = rest.strip_prefix('=') {
            // A header takes the rest of the list.
            header = after.to_string();
            rest = "";
        } else {
            rest = rest.trim_start_matches([',', ' ', '\t']);
        }
        if k.name == "tty" && header == "TT" {
            header = "TTY".into();
        }
        columns.push(Column { key: k, header });
    }
    Ok(columns)
}

/// What selects the processes shown: any of them does.
#[derive(Default)]
struct Select {
    all: bool,
    /// BSD `a` or `-a`: every process with a terminal.
    with_terminal: bool,
    /// `-a`, `-d`: not session leaders.
    not_leaders: bool,
    /// BSD `x`: also those without one.
    without_terminal: bool,
    pids: Vec<u64>,
    parents: Vec<u64>,
    sessions: Vec<u64>,
    groups: Vec<u64>,
    terminals: Vec<String>,
    commands: Vec<String>,
    users: Vec<u32>,
    negate: bool,
    /// BSD `r`: of those, only the running ones.
    running: bool,
}

impl Select {
    /// Whether anything but the default selects.
    fn any(&self) -> bool {
        self.all
            || self.with_terminal
            || self.not_leaders
            || !self.pids.is_empty()
            || !self.parents.is_empty()
            || !self.sessions.is_empty()
            || !self.groups.is_empty()
            || !self.terminals.is_empty()
            || !self.commands.is_empty()
            || !self.users.is_empty()
    }

    /// Whether `p` is selected; `own` is the terminal ps runs on.
    fn selects(&self, p: &Process, own: Option<&str>) -> bool {
        let chosen = if !self.any() {
            // Those on ps's own terminal, or, with BSD `x`, every one of
            // the user's (every one, as every one is root's).
            self.without_terminal || p.terminal.as_deref() == own
        } else {
            let leader = p.number == p.session;
            self.all
                || (self.with_terminal && p.terminal.is_some() && !(self.not_leaders && leader))
                || (self.not_leaders && !self.with_terminal && !leader)
                || (self.without_terminal && p.terminal.is_none())
                || self.pids.contains(&p.number)
                || self.parents.contains(&p.parent)
                || self.sessions.contains(&p.session)
                || self.groups.contains(&p.group)
                || p.terminal.as_ref().is_some_and(|t| {
                    self.terminals.iter().any(|want| want == t || format!("pts/{want}") == *t)
                })
                || (p.terminal.is_none() && self.terminals.iter().any(|t| t == "?" || t == "-"))
                || self.commands.contains(&p.name)
                || (!self.users.is_empty() && self.users.contains(&0))
        };
        chosen != self.negate && (!self.running || proc::run_letter(&p.run) == 'R')
    }
}

/// The options read from the command line.
#[derive(Default)]
struct Options {
    select: Select,
    columns: Vec<Column>,
    /// The style the default columns are in.
    style: Style,
    /// Columns `-O` adds after the PID.
    after_pid: Vec<Column>,
    threads: Threads,
    forest: bool,
    sort: Vec<(Key, bool)>,
    no_headers: bool,
    wide: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Style {
    #[default]
    Plain,
    Full,
    ExtraFull,
    Long,
    Jobs,
    BsdPlain,
    BsdUser,
    BsdJobs,
    BsdLong,
    BsdSignals,
    BsdMemory,
}

/// How threads are shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Threads {
    #[default]
    No,
    /// `-L`: LWP and NLWP columns.
    Lwp,
    /// `-T`: an SPID column.
    Spid,
    /// BSD `H`, `-m`: threads as if they were processes.
    AsProcesses,
}

/// The default columns of a style.
fn style_columns(style: Style, threads: Threads) -> &'static str {
    match (style, threads) {
        (Style::Plain, Threads::Lwp) => "pid,lwp,tty,time,comm",
        (Style::Plain, Threads::Spid) => "pid,spid,tty,time,comm",
        (Style::Plain, _) => "pid,tty,time,comm",
        (Style::Full, Threads::Lwp) => "user=UID,pid,ppid,lwp,c,nlwp,stime,tty,time,args=CMD",
        (Style::Full, Threads::Spid) => "user=UID,pid,spid,ppid,c,stime,tty,time,args=CMD",
        (Style::Full, _) => "user=UID,pid,ppid,c,stime,tty,time,args=CMD",
        (Style::ExtraFull, _) => "user=UID,pid,ppid,c,sz,rss,psr,stime,tty,time,args=CMD",
        (Style::Long, _) => "f,s,uid,pid,ppid,c,pri,ni,addr,sz,wchan,tty,time,comm=CMD",
        (Style::Jobs, _) => "pid,pgid,sid,tty,time,comm=CMD",
        (Style::BsdPlain, _) => "pid,tty,stat,bsdtime,args",
        (Style::BsdUser, _) => "user,pid,%cpu,%mem,vsz,rss,tty,stat,start,bsdtime,args",
        (Style::BsdJobs, _) => "ppid,pid,pgid,sid,tty,tpgid,stat,uid,bsdtime,args",
        (Style::BsdLong, _) => "f,uid,pid,ppid,pri,ni,vsz,rss,wchan,stat,tty,bsdtime,args",
        (Style::BsdSignals, _) => "uid,pid,stat,tty,bsdtime,args",
        (Style::BsdMemory, _) => "pid,tty,stat,bsdtime,majflt,vsz,rss,%mem,args",
    }
}

/// Numbers from a list, or a mistake.
fn numbers(text: &str) -> Result<Vec<u64>, Fail> {
    util::list(text)
        .map(|n| n.parse().map_err(|_| usage(format!("process ID list syntax error: '{n}'"))))
        .collect()
}

/// Reads BSD options: a word of letters without a dash.
fn bsd(options: &mut Options, word: &str) -> Result<(), Fail> {
    for c in word.chars() {
        match c {
            'a' => options.select.with_terminal = true,
            'x' => options.select.without_terminal = true,
            'A' | 'e' | 'g' => options.select.all = true,
            'u' => options.style = Style::BsdUser,
            'j' => options.style = Style::BsdJobs,
            'l' => options.style = Style::BsdLong,
            's' => options.style = Style::BsdSignals,
            'v' => options.style = Style::BsdMemory,
            'f' => options.forest = true,
            'h' => options.no_headers = true,
            'H' | 'm' => options.threads = Threads::AsProcesses,
            'w' => options.wide = true,
            'r' => options.select.running = true,
            'c' | 'S' | 'T' | 'X' | 'n' | 'Z' => {}
            _ => return Err(usage(format!("unsupported option (BSD syntax) '{c}'"))),
        }
    }
    if options.style == Style::Plain {
        options.style = Style::BsdPlain;
    }
    Ok(())
}

/// Reads the command line.
fn options(args: &mut Args) -> Result<Options, Fail> {
    let mut o = Options::default();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('e' | 'A') => o.select.all = true,
            Arg::Short('a') => {
                o.select.with_terminal = true;
                o.select.not_leaders = true;
            }
            Arg::Short('d') => o.select.not_leaders = true,
            Arg::Short('N') => o.select.negate = true,
            Arg::Long(l) if l == "deselect" => o.select.negate = true,
            Arg::Short('x') => o.select.without_terminal = true,
            Arg::Short('p' | 'q') => o.select.pids.extend(numbers(&args.text()?)?),
            Arg::Long(l) if l == "pid" || l == "quick-pid" => {
                o.select.pids.extend(numbers(&args.text()?)?)
            }
            Arg::Long(l) if l == "ppid" => o.select.parents.extend(numbers(&args.text()?)?),
            Arg::Short('s') => o.select.sessions.extend(numbers(&args.text()?)?),
            Arg::Long(l) if l == "sid" => o.select.sessions.extend(numbers(&args.text()?)?),
            Arg::Short('g') => o.select.groups.extend(numbers(&args.text()?)?),
            Arg::Short('t') => {
                o.select.terminals.extend(util::list(&args.text()?).map(String::from))
            }
            Arg::Long(l) if l == "tty" => {
                o.select.terminals.extend(util::list(&args.text()?).map(String::from))
            }
            Arg::Short('C') => {
                o.select.commands.extend(util::list(&args.text()?).map(String::from))
            }
            Arg::Short('u' | 'U') => users(&mut o.select, &args.text()?)?,
            Arg::Long(l) if l == "user" || l == "User" => users(&mut o.select, &args.text()?)?,
            Arg::Short('f') => o.style = Style::Full,
            Arg::Short('F') => o.style = Style::ExtraFull,
            Arg::Short('l') => o.style = Style::Long,
            Arg::Short('j') => o.style = Style::Jobs,
            Arg::Short('o') => o.columns.extend(format(&args.text()?)?),
            Arg::Long(l) if l == "format" => o.columns.extend(format(&args.text()?)?),
            Arg::Short('O') => o.after_pid.extend(format(&args.text()?)?),
            Arg::Short('L') => o.threads = Threads::Lwp,
            Arg::Short('T') => o.threads = Threads::Spid,
            Arg::Short('m') => o.threads = Threads::AsProcesses,
            Arg::Short('H') => o.forest = true,
            Arg::Long(l) if l == "forest" => o.forest = true,
            Arg::Long(l) if l == "sort" => sort(&mut o, &args.text()?)?,
            Arg::Long(l) if l == "no-headers" || l == "no-heading" => o.no_headers = true,
            Arg::Long(l) if l == "headers" => o.no_headers = false,
            Arg::Short('w') => o.wide = true,
            Arg::Long(l) if l == "width" || l == "cols" || l == "columns" => {
                let _: usize = args.number()?;
                o.wide = true;
            }
            Arg::Short('y' | 'c' | 'M' | 'Z') => {}
            Arg::Long(l) if l == "help" => {
                println!("usage: ps [options]   (see ps(1))");
                return Err(util::fail_with(0, String::new()));
            }
            Arg::Short('V') => version(),
            Arg::Long(l) if l == "version" => version(),
            Arg::Operand(word) => {
                let word = word.to_string_lossy().into_owned();
                if word.chars().all(|c| c.is_ascii_digit() || c == ',') {
                    o.select.pids.extend(numbers(&word)?);
                } else {
                    bsd(&mut o, &word)?;
                }
            }
            other => return Err(args.unknown(&other)),
        }
    }
    Ok(o)
}

fn version() {
    println!("ps from Dukovo's systools {}", env!("CARGO_PKG_VERSION"));
    std::process::exit(0);
}

/// Adds users to select, by name or number.
fn users(select: &mut Select, text: &str) -> Result<(), Fail> {
    for name in util::list(text) {
        let uid = util::user_id(name)
            .ok_or_else(|| usage(format!("user name does not exist: {name}")))?;
        select.users.push(uid);
    }
    Ok(())
}

/// Reads sort keys: `[+|-]key,...`.
fn sort(o: &mut Options, text: &str) -> Result<(), Fail> {
    for item in util::list(text) {
        let (descending, name) = match item.strip_prefix('-') {
            Some(name) => (true, name),
            None => (false, item.strip_prefix('+').unwrap_or(item)),
        };
        let k = key(name).ok_or_else(|| usage(format!("unknown sort specifier: {name}")))?;
        o.sort.push((k, descending));
    }
    Ok(())
}

/// What a row needs besides its process: the time and memory to measure
/// percentages by.
struct Context {
    now_mono: u64,
    boot_ns: u64,
    memory_kib: u64,
    page_kib: u64,
    now_s: i64,
}

/// A row: a process, or one of its threads.
struct Row<'a> {
    p: &'a Process,
    thread: Option<&'a Thread>,
    threads: &'a [Thread],
    args: &'a [OsString],
    depth: usize,
}

impl Row<'_> {
    fn cpu_ns(&self) -> u64 {
        self.thread.map_or(self.p.cpu_ns(), |t| t.user_ns + t.system_ns)
    }

    fn first(&self) -> Option<&Thread> {
        self.thread.or_else(|| {
            self.threads.iter().find(|t| t.number == self.p.number).or(self.threads.first())
        })
    }

    /// Its CPU use since it started, in percent.
    fn pcpu(&self, ctx: &Context) -> f64 {
        let lived = ctx.now_mono.saturating_sub(self.p.started_ns);
        if lived == 0 { 0.0 } else { self.cpu_ns() as f64 * 100.0 / lived as f64 }
    }

    fn policy(&self) -> &str {
        self.thread.map_or(self.p.policy.as_str(), |t| t.policy.as_str())
    }

    fn nice(&self) -> i64 {
        self.thread.map_or(self.p.nice, |t| t.nice)
    }

    fn realtime(&self) -> bool {
        matches!(self.policy(), "fifo" | "round_robin" | "rr")
    }

    fn stat(&self) -> String {
        let p = self.p;
        let mut s = String::new();
        let run = match self.thread {
            Some(t) if !p.zombie() && p.run != "stopped" => &t.run,
            _ => &p.run,
        };
        s.push(proc::run_letter(run));
        if self.realtime() || self.nice() < 0 {
            s.push('<');
        } else if self.nice() > 0 {
            s.push('N');
        }
        if p.number == p.session {
            s.push('s');
        }
        if p.threads > 1 {
            s.push('l');
        }
        if self.terminal_group() == p.group as i128 {
            s.push('+');
        }
        s
    }

    /// TPGID: its terminal's foreground process group, -1 without a
    /// terminal.
    fn terminal_group(&self) -> i128 {
        match (&self.p.terminal, self.p.terminal_group) {
            (Some(_), Some(group)) => group as i128,
            _ => -1,
        }
    }

    fn command(&self, wide_args: bool) -> String {
        let p = self.p;
        let mut text = String::new();
        if self.depth > 0 {
            text.push(' ');
            for _ in 1..self.depth {
                text.push_str("    ");
            }
            text.push_str("\\_ ");
        }
        if p.zombie() {
            text.push_str(&format!("[{}] <defunct>", p.name));
        } else if let Some(t) = self.thread.filter(|_| !wide_args) {
            text.push_str(&t.name);
        } else if wide_args && !self.args.is_empty() {
            let args: Vec<String> =
                self.args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
            text.push_str(&args.join(" "));
        } else if wide_args {
            text.push_str(&format!("[{}]", p.name));
        } else {
            text.push_str(&p.name);
        }
        text
    }

    /// The value of column `key`.
    fn value(&self, key: &Key, ctx: &Context) -> String {
        let p = self.p;
        let started_s = ((ctx.boot_ns + p.started_ns) / 1_000_000_000) as i64;
        match key.name {
            "pid" => p.number.to_string(),
            "ppid" => p.parent.to_string(),
            "pgid" => p.group.to_string(),
            "sid" => p.session.to_string(),
            "tty" => p.terminal.clone().unwrap_or_else(|| "?".into()),
            "stat" => self.stat(),
            "s" => self.stat()[..1].to_string(),
            "time" => util::clock_time(self.cpu_ns() / 1_000_000_000),
            "bsdtime" => {
                let s = self.cpu_ns() / 1_000_000_000;
                format!("{}:{:02}", s / 60, s % 60)
            }
            "args" => self.command(true),
            "comm" => self.command(false),
            "uid" | "gid" => "0".into(),
            "user" => util::user_name(0),
            "group" => "root".into(),
            "c" => format!("{}", self.pcpu(ctx) as u64),
            "pcpu" => format!("{:.1}", self.pcpu(ctx)),
            "pmem" => {
                let share = if ctx.memory_kib == 0 {
                    0.0
                } else {
                    p.resident_kib as f64 * 100.0 / ctx.memory_kib as f64
                };
                format!("{share:.1}")
            }
            "vsz" => p.virtual_kib.to_string(),
            "rss" => p.resident_kib.to_string(),
            "sz" => (p.virtual_kib / ctx.page_kib.max(1)).to_string(),
            "stime" => {
                let age = ctx.now_s - started_s;
                if age < 86400 {
                    util::local_time(started_s, "%H:%M")
                } else if age < 365 * 86400 {
                    util::local_time(started_s, "%b%d")
                } else {
                    util::local_time(started_s, "%Y")
                }
            }
            "start" => {
                if ctx.now_s - started_s < 86400 {
                    util::local_time(started_s, "%H:%M")
                } else {
                    util::local_time(started_s, "%b %d")
                }
            }
            "lstart" => util::local_time(started_s, "%a %b %e %H:%M:%S %Y"),
            "ni" => {
                if self.realtime() {
                    "-".into()
                } else {
                    self.nice().to_string()
                }
            }
            "pri" => {
                // As procps-ng shows it: 80 + nice for the fair class,
                // below 60 for the realtime ones.
                let priority = self.thread.map_or(p.priority, |t| t.priority);
                if self.realtime() {
                    (59 - priority).to_string()
                } else {
                    (80 + self.nice()).to_string()
                }
            }
            "priority" => {
                let priority = self.thread.map_or(p.priority, |t| t.priority);
                if self.realtime() {
                    (-1 - priority).to_string()
                } else {
                    (20 + self.nice()).to_string()
                }
            }
            "rtprio" => {
                if self.realtime() {
                    self.thread.map_or(p.priority, |t| t.priority).to_string()
                } else {
                    "-".into()
                }
            }
            "cls" => match self.policy() {
                "other" | "normal" => "TS",
                "batch" => "B",
                "idle" => "IDL",
                "fifo" => "FF",
                "round_robin" | "rr" => "RR",
                "" => "-",
                _ => "?",
            }
            .into(),
            "psr" => self.first().map_or("-".into(), |t| t.cpu.to_string()),
            "wchan" => self.first().and_then(|t| t.waits.clone()).unwrap_or_else(|| "-".into()),
            "lwp" => self.thread.map_or(p.number, |t| t.number).to_string(),
            "nlwp" => p.threads.to_string(),
            "etime" => util::elapsed(ctx.now_mono.saturating_sub(p.started_ns) / 1_000_000_000),
            "etimes" => (ctx.now_mono.saturating_sub(p.started_ns) / 1_000_000_000).to_string(),
            "majflt" => p.major_faults.to_string(),
            "minflt" => p.minor_faults.to_string(),
            "f" => "0".into(),
            "addr" => "-".into(),
            "tpgid" => self.terminal_group().to_string(),
            "ioclass" => format!("{}/{}", p.io_class, p.io_level),
            "domain" => p.domain.clone(),
            _ => "?".into(),
        }
    }

    /// The value to sort by: a number where the column is one.
    fn sort_value(&self, key: &Key, ctx: &Context) -> (i128, String) {
        let p = self.p;
        let n: i128 = match key.name {
            "pid" => p.number as i128,
            "ppid" => p.parent as i128,
            "pgid" => p.group as i128,
            "sid" => p.session as i128,
            "time" | "bsdtime" => self.cpu_ns() as i128,
            "c" | "pcpu" => (self.pcpu(ctx) * 1000.0) as i128,
            "pmem" | "rss" => p.resident_kib as i128,
            "vsz" | "sz" => p.virtual_kib as i128,
            // The oldest first by when they started, the newest by how
            // long they have run, as procps sorts them.
            "stime" | "start" | "lstart" => p.started_ns as i128,
            "etime" | "etimes" => -(p.started_ns as i128),
            "tpgid" => self.terminal_group(),
            "ni" => self.nice() as i128,
            "pri" | "priority" | "rtprio" => self.thread.map_or(p.priority, |t| t.priority) as i128,
            "psr" => self.first().map_or(0, |t| t.cpu as i128),
            "lwp" => self.thread.map_or(p.number, |t| t.number) as i128,
            "nlwp" => p.threads as i128,
            "majflt" => p.major_faults as i128,
            "minflt" => p.minor_faults as i128,
            _ => return (0, self.value(key, ctx)),
        };
        (n, String::new())
    }
}

/// Orders processes as a tree: each after its parent, children by
/// number, with their depth.
fn forest(processes: &[Process]) -> Vec<(usize, usize)> {
    let numbers: HashSet<u64> = processes.iter().map(|p| p.number).collect();
    let mut children: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (i, p) in processes.iter().enumerate() {
        if p.parent != p.number && numbers.contains(&p.parent) {
            children.entry(p.parent).or_default().push(i);
        } else {
            roots.push(i);
        }
    }
    let mut order = Vec::new();
    let mut stack: Vec<(usize, usize)> = roots.into_iter().rev().map(|i| (i, 0)).collect();
    let mut seen = HashSet::new();
    while let Some((i, depth)) = stack.pop() {
        if !seen.insert(i) {
            continue;
        }
        order.push((i, depth));
        if let Some(kids) = children.get(&processes[i].number) {
            for &kid in kids.iter().rev() {
                stack.push((kid, depth + 1));
            }
        }
    }
    order
}

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut o = options(args)?;
    if o.columns.is_empty() {
        // Each of a style's columns is read alone, since a header in a
        // format list takes the rest of it.
        let mut columns = Vec::new();
        for item in style_columns(o.style, o.threads).split(',') {
            columns.extend(format(item)?);
        }
        if !o.after_pid.is_empty() {
            let at = columns.iter().position(|c| c.key.name == "pid").map_or(0, |i| i + 1);
            columns.splice(at..at, o.after_pid.drain(..));
        }
        o.columns = columns;
    }
    let own = own_terminal();
    let processes: Vec<Process> = proc::processes()
        .map_err(|e| util::failed(&"/proc", e))?
        .into_iter()
        .filter(|p| o.select.selects(p, own.as_deref()))
        .collect();
    let memory = proc::memory().unwrap_or_default();
    let stat = proc::stat().unwrap_or_default();
    let ctx = Context {
        now_mono: util::monotonic_ns(),
        boot_ns: stat.boot_time_ns,
        memory_kib: memory.total,
        page_kib: 4,
        now_s: (util::now_ns() / 1_000_000_000) as i64,
    };
    let need_threads =
        o.threads != Threads::No || o.columns.iter().any(|c| matches!(c.key.name, "psr" | "wchan"));
    let threads: Vec<Vec<Thread>> = processes
        .iter()
        .map(|p| if need_threads { proc::threads(p.number) } else { Vec::new() })
        .collect();
    let wants_args = o.columns.iter().any(|c| c.key.name == "args");
    let cmdlines: Vec<Vec<OsString>> = processes
        .iter()
        .map(|p| if wants_args { proc::cmdline(p.number) } else { Vec::new() })
        .collect();
    // Order: as a tree, by the sort keys, or by number.
    let mut order: Vec<(usize, usize)> =
        if o.forest { forest(&processes) } else { (0..processes.len()).map(|i| (i, 0)).collect() };
    if !o.sort.is_empty() && !o.forest {
        let rows: Vec<Row> = order
            .iter()
            .map(|&(i, depth)| Row {
                p: &processes[i],
                thread: None,
                threads: &threads[i],
                args: &cmdlines[i],
                depth,
            })
            .collect();
        // Each row's sort values, and where it came from.
        type Keyed = (Vec<(i128, String)>, (usize, usize));
        let mut keyed: Vec<Keyed> = rows
            .iter()
            .zip(order.iter())
            .map(|(row, &at)| (o.sort.iter().map(|(k, _)| row.sort_value(k, &ctx)).collect(), at))
            .collect();
        keyed.sort_by(|a, b| {
            for (i, (_, descending)) in o.sort.iter().enumerate() {
                let ord = a.0[i].cmp(&b.0[i]);
                let ord = if *descending { ord.reverse() } else { ord };
                if ord.is_ne() {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
        order = keyed.into_iter().map(|(_, at)| at).collect();
    }
    let mut table: Vec<Vec<String>> = Vec::new();
    // With every header empty (`-o pid=`) there is no header line.
    if !o.no_headers && o.columns.iter().any(|c| !c.header.is_empty()) {
        table.push(o.columns.iter().map(|c| c.header.clone()).collect());
    }
    for &(i, depth) in &order {
        let p = &processes[i];
        let mut rows = Vec::new();
        if o.threads != Threads::No && !p.zombie() && !threads[i].is_empty() {
            for t in &threads[i] {
                rows.push(Row {
                    p,
                    thread: Some(t),
                    threads: &threads[i],
                    args: &cmdlines[i],
                    depth,
                });
            }
        } else {
            rows.push(Row { p, thread: None, threads: &threads[i], args: &cmdlines[i], depth });
        }
        for row in rows {
            table.push(o.columns.iter().map(|c| row.value(&c.key, &ctx)).collect());
        }
    }
    let width = if o.wide || !util::is_terminal(1) {
        None
    } else {
        util::terminal_size(1).map(|(columns, _)| columns)
    };
    let rights: Vec<bool> = o.columns.iter().map(|c| c.key.right).collect();
    print_table(&table, &rights, width)?;
    // Like procps-ng, 1 when nothing was selected.
    Ok(if order.is_empty() { 1 } else { 0 })
}

/// The terminal ps runs on, as `pts/N`.
fn own_terminal() -> Option<String> {
    for fd in [0, 1, 2] {
        // SAFETY: ttyname returns a C string the C library keeps, or null.
        let name = unsafe { libc::ttyname(fd) };
        if !name.is_null() {
            // SAFETY: a non-null result is a C string.
            let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned();
            return Some(name.strip_prefix("/dev/").unwrap_or(&name).to_string());
        }
    }
    None
}

/// Prints rows lined up in columns, the last unpadded; numbers to the
/// right. Lines are cut to `width` if given.
pub fn print_table(table: &[Vec<String>], right: &[bool], width: Option<usize>) -> io::Result<()> {
    let columns = right.len();
    let mut widths = vec![0; columns];
    for row in table {
        for (i, cell) in row.iter().enumerate().take(columns.saturating_sub(1)) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = io::BufWriter::new(io::stdout().lock());
    for row in table {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push(' ');
            }
            let last = i + 1 == columns;
            let pad = widths[i].saturating_sub(cell.chars().count());
            if right[i] && !last {
                line.extend(std::iter::repeat_n(' ', pad));
                line.push_str(cell);
            } else if last {
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.extend(std::iter::repeat_n(' ', pad));
            }
        }
        let line = line.trim_end();
        match width {
            Some(width) if line.chars().count() > width => {
                writeln!(out, "{}", line.chars().take(width).collect::<String>())?
            }
            _ => writeln!(out, "{line}")?,
        }
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_take_keys_and_headers() {
        let columns = format("pid,user=WHO IS IT").unwrap();
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].header, "PID");
        assert_eq!(columns[1].key.name, "user");
        assert_eq!(columns[1].header, "WHO IS IT");
        let columns = format("pid tty %cpu").unwrap();
        assert_eq!(columns[1].header, "TTY");
        assert_eq!(columns[2].key.name, "pcpu");
        assert!(format("bogus").is_err());
    }

    #[test]
    fn styles_name_their_columns() {
        let headers = |style| -> Vec<String> {
            style_columns(style, Threads::No)
                .split(',')
                .flat_map(|item| format(item).unwrap())
                .map(|c| c.header)
                .collect()
        };
        assert_eq!(
            headers(Style::Full),
            ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"]
        );
        assert_eq!(headers(Style::Jobs), ["PID", "PGID", "SID", "TTY", "TIME", "CMD"]);
        assert_eq!(headers(Style::Long).len(), 14);
    }

    #[test]
    fn trees_put_children_under_parents() {
        let p = |number, parent| Process { number, parent, ..Process::default() };
        let processes = vec![p(1, 0), p(2, 1), p(3, 2), p(4, 1), p(9, 7)];
        let order = forest(&processes);
        let numbers: Vec<(u64, usize)> =
            order.iter().map(|&(i, d)| (processes[i].number, d)).collect();
        assert_eq!(numbers, vec![(1, 0), (2, 1), (3, 2), (4, 1), (9, 0)]);
    }

    #[test]
    fn foreground_groups_and_start_order() {
        let p = Process {
            number: 7,
            group: 7,
            session: 5,
            run: "running".into(),
            terminal: Some("pts/1".into()),
            terminal_group: Some(7),
            started_ns: 10,
            ..Process::default()
        };
        let row = |p| Row { p, thread: None, threads: &[], args: &[], depth: 0 };
        assert_eq!(row(&p).stat(), "R+");
        assert_eq!(row(&p).terminal_group(), 7);
        let background = Process { group: 6, ..p.clone() };
        assert_eq!(row(&background).stat(), "R");
        let detached = Process { terminal: None, terminal_group: None, ..p.clone() };
        assert_eq!(row(&detached).terminal_group(), -1);
        // procps sorts start times oldest first, elapsed times newest
        // first.
        let ctx = Context { now_mono: 100, boot_ns: 0, memory_kib: 1, page_kib: 4, now_s: 0 };
        let later = Process { started_ns: 20, ..p.clone() };
        let key = |name: &str| format(name).unwrap().remove(0).key;
        let value = |p, name: &str| row(p).sort_value(&key(name), &ctx).0;
        assert!(value(&p, "start_time") < value(&later, "start_time"));
        assert!(value(&p, "lstart") < value(&later, "lstart"));
        assert!(value(&p, "etime") > value(&later, "etime"));
        // BSD `r`: only the running ones.
        let select = Select { all: true, running: true, ..Select::default() };
        assert!(select.selects(&p, None));
        assert!(!select.selects(&Process { run: "sleeping".into(), ..p.clone() }, None));
    }

    #[test]
    fn default_selection_is_the_terminal() {
        let select = Select::default();
        let mut p =
            Process { number: 5, session: 5, terminal: Some("pts/1".into()), ..Process::default() };
        assert!(select.selects(&p, Some("pts/1")));
        assert!(!select.selects(&p, Some("pts/2")));
        p.terminal = None;
        let select = Select { without_terminal: true, ..Select::default() };
        assert!(select.selects(&p, Some("pts/2")));
    }
}
