// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! pidstat(1): what processes use, as sysstat's pidstat reports it: CPU
//! (`-u`), page faults and memory (`-r`), disk I/O (`-d`) and context
//! switches (`-w`), per process or per thread (`-t`). Without an interval
//! it reports each process's use since it started; with one, the use in
//! each interval, then the averages.

use std::collections::HashMap;
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use crate::args::{Arg, Args};
use crate::proc::{self, Process, Thread};
use crate::util::{self, Fail, Regex, usage};

/// What one process or thread had used when it was read.
#[derive(Clone, Default)]
struct Use {
    pid: u64,
    tid: Option<u64>,
    name: String,
    user_ns: u64,
    system_ns: u64,
    cpu: u32,
    minor: u64,
    major: u64,
    virtual_kib: u64,
    resident_kib: u64,
    read: u64,
    written: u64,
    voluntary: u64,
    involuntary: u64,
    started_ns: u64,
}

/// A report.
#[derive(Clone, Copy)]
enum Report {
    Cpu,
    Memory,
    Disk,
    Switches,
}

impl Report {
    /// Whether `d`, a use over some time, shows activity in this report,
    /// as sysstat's pidstat asks of the tasks it shows without `-p ALL`.
    fn active(self, d: &Use) -> bool {
        match self {
            Report::Cpu => d.user_ns + d.system_ns > 0,
            Report::Memory => d.minor + d.major > 0,
            Report::Disk => d.read + d.written > 0,
            Report::Switches => d.voluntary + d.involuntary > 0,
        }
    }
}

/// A report, its heading, and how it shows one use over some seconds.
type Section = (Report, String, Box<dyn Fn(&Use, f64) -> String>);

/// The reports asked for.
#[derive(Clone, Copy, Default)]
struct Reports {
    cpu: bool,
    memory: bool,
    disk: bool,
    switches: bool,
}

fn sample(pids: &Option<Vec<u64>>, command: &Option<Regex>, threads: bool) -> Vec<Use> {
    let processes = proc::processes().unwrap_or_default();
    let mut uses = Vec::new();
    for p in processes.iter().filter(|p| !p.zombie()) {
        if pids.as_ref().is_some_and(|l| !l.contains(&p.number)) {
            continue;
        }
        if command.as_ref().is_some_and(|c| !c.is_match(&p.name)) {
            continue;
        }
        uses.push(of_process(p));
        if threads {
            for t in proc::threads(p.number) {
                uses.push(of_thread(p, &t));
            }
        }
    }
    uses
}

fn of_process(p: &Process) -> Use {
    let first = proc::threads(p.number).into_iter().find(|t| t.number == p.number);
    Use {
        pid: p.number,
        tid: None,
        name: p.name.clone(),
        user_ns: p.user_ns,
        system_ns: p.system_ns,
        cpu: first.map_or(0, |t| t.cpu),
        minor: p.minor_faults,
        major: p.major_faults,
        virtual_kib: p.virtual_kib,
        resident_kib: p.resident_kib,
        read: p.read_bytes,
        written: p.written_bytes,
        voluntary: p.voluntary_switches,
        involuntary: p.involuntary_switches,
        started_ns: p.started_ns,
    }
}

fn of_thread(p: &Process, t: &Thread) -> Use {
    Use {
        pid: p.number,
        tid: Some(t.number),
        name: format!("|__{}", t.name),
        user_ns: t.user_ns,
        system_ns: t.system_ns,
        cpu: t.cpu,
        voluntary: t.voluntary_switches,
        involuntary: t.involuntary_switches,
        virtual_kib: p.virtual_kib,
        resident_kib: p.resident_kib,
        started_ns: p.started_ns,
        ..Use::default()
    }
}

/// pidstat(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut reports = Reports::default();
    let mut pids: Option<Vec<u64>> = None;
    // Every task, or only those active in a report (`-p ALL`).
    let mut all = false;
    let mut command: Option<Regex> = None;
    let mut threads = false;
    let mut horizontal = false;
    let mut numbers: Vec<u64> = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('u') => reports.cpu = true,
            Arg::Short('r') => reports.memory = true,
            Arg::Short('d') => reports.disk = true,
            Arg::Short('w') => reports.switches = true,
            Arg::Short('t') => threads = true,
            Arg::Short('h') => horizontal = true,
            Arg::Short('p') => {
                let text = args.text()?;
                all = true;
                if text != "ALL" && text != "SELF" {
                    let mut list = Vec::new();
                    for pid in util::list(&text) {
                        list.push(pid.parse().map_err(|_| usage(format!("invalid PID '{pid}'")))?);
                    }
                    pids = Some(list);
                } else if text == "SELF" {
                    pids = Some(vec![proc::own_number()]);
                }
            }
            Arg::Short('C') => {
                let text = args.text()?;
                command =
                    Some(Regex::new(&text, false).map_err(|e| usage(format!("bad pattern: {e}")))?);
            }
            Arg::Short('l' | 'I' | 'H' | 'U' | 'G' | 'R' | 's' | 'v') => {}
            Arg::Operand(n) => {
                let n = n.to_string_lossy();
                numbers.push(
                    n.parse().map_err(|_| usage(format!("invalid interval or count '{n}'")))?,
                );
            }
            other => return Err(args.unknown(&other)),
        }
    }
    if !(reports.cpu || reports.memory || reports.disk || reports.switches) {
        reports.cpu = true;
    }
    let interval = numbers.first().copied();
    let count = numbers.get(1).copied();
    if interval == Some(0) {
        return Err(usage("the interval must be above 0".into()));
    }
    let mut out = io::stdout().lock();
    let now_s = (util::now_ns() / 1_000_000_000) as i64;
    let cpus = proc::stat().map(|s| s.cpus.len()).unwrap_or(1);
    // sysstat's first line: the system, the host, the date, the machine.
    let (system, release, machine) = util::uname();
    writeln!(
        out,
        "{system} {release} ({}) \t{} \t_{machine}_\t({cpus} CPU)\n",
        util::hostname(),
        util::local_time(now_s, "%m/%d/%Y")
    )?;
    let Some(interval) = interval else {
        // Since each one started.
        let uses = sample(&pids, &command, threads);
        let now = util::monotonic_ns();
        let since: HashMap<(u64, Option<u64>), (Use, u64)> = HashMap::new();
        let shown = Shown { reports, threads, horizontal, all };
        report(&mut out, &uses, &since, now, shown, &stamp())?;
        return Ok(0);
    };
    let mut before = sample(&pids, &command, threads);
    let mut at = util::monotonic_ns();
    let first: HashMap<(u64, Option<u64>), (Use, u64)> =
        before.iter().map(|u| ((u.pid, u.tid), (u.clone(), at))).collect();
    let mut round = 0u64;
    let shown = Shown { reports, threads, horizontal, all };
    loop {
        thread::sleep(Duration::from_secs(interval));
        let now = util::monotonic_ns();
        let uses = sample(&pids, &command, threads);
        let since: HashMap<(u64, Option<u64>), (Use, u64)> =
            before.iter().map(|u| ((u.pid, u.tid), (u.clone(), at))).collect();
        report(&mut out, &uses, &since, now, shown, &stamp())?;
        before = uses;
        at = now;
        round += 1;
        if count.is_some_and(|c| round >= c) {
            break;
        }
    }
    // The averages over every interval, from the first sample to the last.
    report(&mut out, &before, &first, at, shown, "Average:")?;
    Ok(0)
}

/// The time of day, as a report's first column.
fn stamp() -> String {
    util::local_time((util::now_ns() / 1_000_000_000) as i64, "%H:%M:%S")
}

/// How the reports are shown.
#[derive(Clone, Copy)]
struct Shown {
    reports: Reports,
    /// A line for each thread too (`-t`).
    threads: bool,
    /// The reports side by side (`-h`).
    horizontal: bool,
    /// Every task, not only those active in a report shown.
    all: bool,
}

/// Prints each report asked for: the use between `since` (or each one's
/// start) and `now`.
fn report(
    out: &mut impl Write,
    uses: &[Use],
    since: &HashMap<(u64, Option<u64>), (Use, u64)>,
    now: u64,
    shown: Shown,
    time: &str,
) -> io::Result<()> {
    let Shown { reports, threads, horizontal, all } = shown;
    let total_kib = proc::memory().map(|m| m.total).unwrap_or(0).max(1);
    let id_head = if threads { "      TGID       TID" } else { "       PID" };
    let ids = |u: &Use| {
        if threads {
            match u.tid {
                Some(tid) => format!("{:>10} {:>9}", "-", tid),
                None => format!("{:>10} {:>9}", u.pid, "-"),
            }
        } else {
            format!("{:>10}", u.pid)
        }
    };
    // The use since the sample before, and over how many seconds.
    let delta = |u: &Use| -> (Use, f64) {
        match since.get(&(u.pid, u.tid)) {
            Some((b, at)) => {
                let d = |a: u64, b: u64| a.saturating_sub(b);
                (
                    Use {
                        user_ns: d(u.user_ns, b.user_ns),
                        system_ns: d(u.system_ns, b.system_ns),
                        minor: d(u.minor, b.minor),
                        major: d(u.major, b.major),
                        read: d(u.read, b.read),
                        written: d(u.written, b.written),
                        voluntary: d(u.voluntary, b.voluntary),
                        involuntary: d(u.involuntary, b.involuntary),
                        ..u.clone()
                    },
                    (now.saturating_sub(*at) as f64 / 1e9).max(0.001),
                )
            }
            None => (u.clone(), (now.saturating_sub(u.started_ns) as f64 / 1e9).max(0.001)),
        }
    };
    let mut sections: Vec<Section> = Vec::new();
    if reports.cpu {
        sections.push((
            Report::Cpu,
            "    %usr %system  %guest   %wait    %CPU   CPU".into(),
            Box::new(|d: &Use, s: f64| {
                let usr = d.user_ns as f64 / 1e7 / s;
                let sys = d.system_ns as f64 / 1e7 / s;
                format!("{usr:8.2} {sys:7.2} {:7.2} {:7.2} {:7.2} {:5}", 0.0, 0.0, usr + sys, d.cpu)
            }),
        ));
    }
    if reports.memory {
        sections.push((
            Report::Memory,
            "  minflt/s  majflt/s     VSZ     RSS   %MEM".into(),
            Box::new(move |d: &Use, s: f64| {
                format!(
                    "{:10.2} {:9.2} {:7} {:7} {:6.2}",
                    d.minor as f64 / s,
                    d.major as f64 / s,
                    d.virtual_kib,
                    d.resident_kib,
                    d.resident_kib as f64 * 100.0 / total_kib as f64
                )
            }),
        ));
    }
    if reports.disk {
        sections.push((
            Report::Disk,
            "   kB_rd/s   kB_wr/s kB_ccwr/s iodelay".into(),
            Box::new(|d: &Use, s: f64| {
                format!(
                    "{:10.2} {:9.2} {:9.2} {:7}",
                    d.read as f64 / 1024.0 / s,
                    d.written as f64 / 1024.0 / s,
                    0.0,
                    0
                )
            }),
        ));
    }
    if reports.switches {
        sections.push((
            Report::Switches,
            "   cswch/s nvcswch/s".into(),
            Box::new(|d: &Use, s: f64| {
                format!("{:10.2} {:9.2}", d.voluntary as f64 / s, d.involuntary as f64 / s)
            }),
        ));
    }
    let blocks: Vec<Vec<Section>> =
        if horizontal { vec![sections] } else { sections.into_iter().map(|s| vec![s]).collect() };
    for block in blocks {
        let heads: Vec<&str> = block.iter().map(|(_, h, _)| h.as_str()).collect();
        writeln!(out, "{time} {:>5} {id_head} {}  Command", "UID", heads.join(" "))?;
        for u in uses {
            let (d, seconds) = delta(u);
            if !all && !block.iter().any(|(r, _, _)| r.active(&d)) {
                continue;
            }
            let cells: Vec<String> = block.iter().map(|(_, _, f)| f(&d, seconds)).collect();
            writeln!(out, "{time} {:>5} {} {}  {}", 0, ids(u), cells.join(" "), u.name)?;
        }
        writeln!(out)?;
    }
    Ok(())
}
