// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! pidstat(1): what processes use, as sysstat's pidstat reports it and
//! in its layout: CPU (`-u`), page faults and memory (`-r`), I/O (`-d`),
//! context switches (`-w`), threads and descriptors (`-v`) and scheduling
//! (`-R`), per process or per thread too (`-t`). Without an interval it
//! reports each process's use since it started; with one, the use in
//! each interval, then the averages, also when interrupted.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::args::{Arg, Args};
use crate::proc::{self, Process, Thread};
use crate::util::{self, Fail, Regex, usage};

/// Set by SIGINT: the averages come next, as with sysstat's pidstat.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// What one process or thread had used when it was read, and how it is
/// scheduled.
#[derive(Clone, Default)]
struct Use {
    pid: u64,
    tid: Option<u64>,
    /// A thread shown without its process: its own name matched `-C`.
    alone: bool,
    /// The Command column.
    name: String,
    uid: u32,
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
    threads: u64,
    fds: u64,
    priority: i64,
    policy: &'static str,
}

/// A task's key: its process and, for a thread's line, the thread.
type Key = (u64, Option<u64>);

/// The reports, in the order sysstat's pidstat prints them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Report {
    Cpu,
    Memory,
    Disk,
    Switches,
    Tasks,
    Scheduling,
}

impl Report {
    /// Its columns' heading, each column after a space.
    fn heading(self) -> &'static str {
        match self {
            Report::Cpu => "    %usr %system  %guest   %wait    %CPU   CPU",
            Report::Memory => "  minflt/s  majflt/s     VSZ     RSS   %MEM",
            Report::Disk => "   kB_rd/s   kB_wr/s kB_ccwr/s iodelay",
            Report::Switches => "   cswch/s nvcswch/s",
            Report::Tasks => " threads   fd-nr",
            Report::Scheduling => " prio policy",
        }
    }

    /// Whether a task did something in this report, as sysstat's pidstat
    /// asks of the tasks it shows without `-p`: `d`, its use over the
    /// time, is not all zeros; and for the levels of `-v` and `-R`, they
    /// changed since `before` (or are not those of a task just made).
    fn active(self, d: &Use, before: Option<&Use>) -> bool {
        let new = Use { policy: "NORMAL", ..Use::default() };
        let before = before.unwrap_or(&new);
        match self {
            Report::Cpu => d.user_ns + d.system_ns > 0,
            Report::Memory => d.minor + d.major > 0,
            Report::Disk => d.read + d.written > 0,
            Report::Switches => d.voluntary + d.involuntary > 0,
            Report::Tasks => (d.threads, d.fds) != (before.threads, before.fds),
            Report::Scheduling => (d.priority, d.policy) != (before.priority, before.policy),
        }
    }

    /// Its columns for `d`, a use over `seconds`, each after a space;
    /// `average` for the averages, which name no CPU.
    fn cells(self, d: &Use, seconds: f64, how: &Shown, average: bool) -> String {
        let rate = |n: u64| n as f64 / seconds;
        match self {
            Report::Cpu => {
                let usr = d.user_ns as f64 / 1e7 / seconds;
                let sys = d.system_ns as f64 / 1e7 / seconds;
                // -I: of all the CPUs, not of one.
                let total = (usr + sys) / if how.of_all { how.cpus as f64 } else { 1.0 };
                let cpu = if average { "-".to_string() } else { d.cpu.to_string() };
                format!(" {usr:7.2} {sys:7.2} {:7.2} {:7.2} {total:7.2} {cpu:>5}", 0.0, 0.0)
            }
            Report::Memory => format!(
                " {:9.2} {:9.2} {:7} {:7} {:6.2}",
                rate(d.minor),
                rate(d.major),
                d.virtual_kib,
                d.resident_kib,
                d.resident_kib as f64 * 100.0 / how.memory_kib as f64
            ),
            Report::Disk => format!(
                " {:9.2} {:9.2} {:9.2} {:7}",
                d.read as f64 / 1024.0 / seconds,
                d.written as f64 / 1024.0 / seconds,
                0.0,
                0
            ),
            Report::Switches => format!(" {:9.2} {:9.2}", rate(d.voluntary), rate(d.involuntary)),
            Report::Tasks => format!(" {:7} {:7}", d.threads, d.fds),
            Report::Scheduling => format!(" {:4} {:>6}", d.priority, d.policy),
        }
    }
}

/// sysstat's name for a scheduling policy that /processes names.
fn policy_name(policy: &str) -> &'static str {
    match policy {
        "fifo" => "FIFO",
        "round_robin" | "rr" => "RR",
        "batch" => "BATCH",
        "idle" => "IDLE",
        _ => "NORMAL",
    }
}

/// Which tasks to read.
#[derive(Default)]
struct Select {
    /// `-p`: these processes only.
    pids: Option<Vec<u64>>,
    /// `-G`: the processes whose names match, with all their threads.
    process: Option<Regex>,
    /// `-C`: the tasks, processes and threads alike, whose own names
    /// match.
    command: Option<Regex>,
    /// `-U` with a user: that user's processes only; `Some(None)` for a
    /// user who does not exist, who has none.
    user: Option<Option<u32>>,
    threads: bool,
    /// `-l`: the whole command line as a process's name.
    command_line: bool,
    /// Whether `-v` asks for the threads and descriptors.
    tasks: bool,
}

fn sample(select: &Select) -> Vec<Use> {
    let processes = proc::processes().unwrap_or_default();
    let mut uses = Vec::new();
    for p in processes.iter().filter(|p| !p.zombie()) {
        if select.pids.as_ref().is_some_and(|l| !l.contains(&p.number)) {
            continue;
        }
        let uid = p.user;
        if select.user.is_some_and(|user| user != Some(uid)) {
            continue;
        }
        if select.process.as_ref().is_some_and(|r| !r.is_match(&p.name)) {
            continue;
        }
        let matches = |name: &str| select.command.as_ref().is_none_or(|r| r.is_match(name));
        let shown = matches(&p.name);
        let threads = proc::threads(p.number);
        let process = of_process(p, &threads, select);
        if shown {
            uses.push(process.clone());
        }
        if select.threads {
            for t in &threads {
                if matches(&t.name) {
                    uses.push(of_thread(&process, p, t, !shown));
                }
            }
        }
    }
    uses
}

fn of_process(p: &Process, threads: &[Thread], select: &Select) -> Use {
    let first = threads.iter().find(|t| t.number == p.number);
    let name = match select.command_line.then(|| proc::cmdline(p.number)) {
        Some(args) if !args.is_empty() => {
            args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
        }
        _ => p.name.clone(),
    };
    let fds = if select.tasks {
        proc::files(p.number).map_or(0, |f| f.iter().filter(|h| h.fd.is_some()).count() as u64)
    } else {
        0
    };
    Use {
        pid: p.number,
        uid: p.user,
        name,
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
        threads: p.threads,
        fds,
        priority: p.priority,
        policy: policy_name(&p.policy),
        ..Use::default()
    }
}

/// A thread's line: under its process's (`|__name`), or `alone` with its
/// process's name in brackets, as sysstat's pidstat shows a thread that
/// `-C` found by its own name.
fn of_thread(process: &Use, p: &Process, t: &Thread, alone: bool) -> Use {
    Use {
        tid: Some(t.number),
        alone,
        name: if alone { format!("({})__{}", p.name, t.name) } else { format!("|__{}", t.name) },
        user_ns: t.user_ns,
        system_ns: t.system_ns,
        cpu: t.cpu,
        minor: 0,
        major: 0,
        read: 0,
        written: 0,
        voluntary: t.voluntary_switches,
        involuntary: t.involuntary_switches,
        priority: t.priority,
        policy: policy_name(&t.policy),
        ..process.clone()
    }
}

/// How the reports are shown.
struct Shown {
    reports: Vec<Report>,
    /// A line for each thread too (`-t`): TGID and TID columns.
    threads: bool,
    /// The reports side by side (`-h`), with no averages.
    horizontal: bool,
    /// Every task chosen, not only those active in a report shown.
    all: bool,
    /// User names, not numbers (`-U`).
    names: bool,
    /// %CPU of all the CPUs (`-I`).
    of_all: bool,
    /// Times as seconds since the epoch (`-H`).
    epoch: bool,
    /// One process in one report: one heading, and no blank lines.
    once: bool,
    cpus: usize,
    memory_kib: u64,
}

impl Shown {
    /// The first column of a report made now.
    fn stamp(&self) -> String {
        let seconds = (util::now_ns() / 1_000_000_000) as i64;
        if self.epoch { seconds.to_string() } else { util::local_time(seconds, "%H:%M:%S") }
    }

    /// The columns before the reports': the user and the task's numbers.
    fn who(&self, user: &str, pid: &str, tid: &str) -> String {
        let user = if self.names { format!(" {user:>8}") } else { format!(" {user:>5}") };
        if self.threads { format!("{user} {pid:>9} {tid:>9}") } else { format!("{user} {pid:>9}") }
    }

    /// The heading line of `block`, after `time`.
    fn heading(&self, time: &str, block: &[Report]) -> String {
        let user = if self.names { "USER" } else { "UID" };
        let ids =
            if self.threads { self.who(user, "TGID", "TID") } else { self.who(user, "PID", "") };
        let heads: String = block.iter().map(|r| r.heading()).collect();
        format!("{time:<11}{ids}{heads}  Command")
    }
}

/// pidstat(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let (mut cpu, mut memory, mut disk, mut switches, mut tasks, mut scheduling) =
        (false, false, false, false, false, false);
    let mut select = Select::default();
    let mut all = false;
    let mut horizontal = false;
    let mut names = false;
    let mut of_all = false;
    let mut epoch = false;
    let mut numbers: Vec<u64> = Vec::new();
    let pattern =
        |text: &str| Regex::new(text, false).map_err(|e| usage(format!("bad pattern: {e}")));
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('u') => cpu = true,
            Arg::Short('r') => memory = true,
            Arg::Short('d') => disk = true,
            Arg::Short('w') => switches = true,
            Arg::Short('v') => tasks = true,
            Arg::Short('R') => scheduling = true,
            // Radix does not tell the stacks' sizes yet.
            Arg::Short('s') => {}
            Arg::Short('t') => select.threads = true,
            Arg::Short('h') => horizontal = true,
            Arg::Short('l') => select.command_line = true,
            Arg::Short('I') => of_all = true,
            Arg::Short('H') => epoch = true,
            Arg::Short('U') => {
                names = true;
                // A user name may follow, but not a number: that is the
                // interval, as sysstat's pidstat reads it.
                let name = args
                    .optional_operand(|a| a.to_str().is_some_and(|t| t.parse::<u64>().is_err()));
                if let Some(name) = name {
                    select.user = Some(util::user_id(&name.to_string_lossy()));
                }
            }
            Arg::Short('p') => {
                let text = args.text()?;
                all = true;
                match text.as_str() {
                    "ALL" => {}
                    "SELF" => select.pids = Some(vec![proc::own_number()]),
                    _ => {
                        let mut list = Vec::new();
                        for pid in util::list(&text) {
                            list.push(
                                pid.parse().map_err(|_| usage(format!("invalid PID '{pid}'")))?,
                            );
                        }
                        select.pids = Some(list);
                    }
                }
            }
            Arg::Short('G') => select.process = Some(pattern(&args.text()?)?),
            Arg::Short('C') => select.command = Some(pattern(&args.text()?)?),
            Arg::Operand(n) if numbers.len() < 2 => {
                let n = n.to_string_lossy();
                numbers.push(
                    n.parse().map_err(|_| usage(format!("invalid interval or count '{n}'")))?,
                );
            }
            other => return Err(args.unknown(&other)),
        }
    }
    select.tasks = tasks;
    let mut reports = Vec::new();
    for (asked, report) in [
        (cpu, Report::Cpu),
        (memory, Report::Memory),
        (disk, Report::Disk),
        (switches, Report::Switches),
        (tasks, Report::Tasks),
        (scheduling, Report::Scheduling),
    ] {
        if asked {
            reports.push(report);
        }
    }
    if reports.is_empty() {
        reports.push(Report::Cpu);
    }
    // An interval of 0 is none: the use since each one started.
    let interval = numbers.first().copied().filter(|&n| n > 0);
    let count = numbers.get(1).copied();
    if count == Some(0) {
        return Err(usage("the count must be above 0".into()));
    }
    let cpus = proc::stat().map_or(1, |s| s.cpus.len()).max(1);
    let once = select.pids.as_ref().is_some_and(|l| l.len() == 1) && reports.len() == 1;
    let shown = Shown {
        reports,
        threads: select.threads,
        horizontal,
        all,
        names,
        of_all,
        epoch,
        once,
        cpus,
        memory_kib: proc::memory().map_or(0, |m| m.total).max(1),
    };
    let mut out = io::stdout().lock();
    let now_s = (util::now_ns() / 1_000_000_000) as i64;
    // sysstat's first line: the system, the host, the date, the machine.
    let (system, release, machine) = util::uname();
    writeln!(
        out,
        "{system} {release} ({}) \t{} \t_{machine}_\t({cpus} CPU)",
        util::hostname(),
        util::local_time(now_s, "%m/%d/%Y")
    )?;
    let Some(interval) = interval else {
        let uses = sample(&select);
        let time = shown.stamp();
        let report = Round { uses: &uses, since: &HashMap::new(), now: util::monotonic_ns() };
        print(&mut out, &shown, &report, &time, &time, true, false)?;
        return Ok(0);
    };
    // SAFETY: all zeros is a valid sigaction: numbers, a signal set and a function address.
    let mut action: libc::sigaction = unsafe { util::zeroed() };
    action.sa_sigaction = on_interrupt as *const () as usize;
    // SAFETY: the handler only stores to an atomic.
    unsafe { libc::sigaction(libc::SIGINT, &raw const action, std::ptr::null_mut()) };
    let mut before = sample(&select);
    let mut at = util::monotonic_ns();
    let first: HashMap<Key, (Use, u64)> =
        before.iter().map(|u| ((u.pid, u.tid), (u.clone(), at))).collect();
    // What the levels of -r and -v added up to in each report, and in
    // how many, for their averages.
    let mut levels: HashMap<Key, [u64; 5]> = HashMap::new();
    let mut time_before = shown.stamp();
    let started = Instant::now();
    let mut round = 0u64;
    loop {
        // Each interval from the first sample, so that they do not drift.
        let deadline = started + Duration::from_secs(interval.saturating_mul(round + 1));
        while Instant::now() < deadline && !INTERRUPTED.load(Ordering::Relaxed) {
            std::thread::sleep((deadline - Instant::now()).min(Duration::from_millis(100)));
        }
        if INTERRUPTED.load(Ordering::Relaxed) {
            break;
        }
        let uses = sample(&select);
        let now = util::monotonic_ns();
        let time = shown.stamp();
        let since: HashMap<Key, (Use, u64)> =
            before.iter().map(|u| ((u.pid, u.tid), (u.clone(), at))).collect();
        let report = Round { uses: &uses, since: &since, now };
        print(&mut out, &shown, &report, &time_before, &time, round == 0, false)?;
        out.flush()?;
        for u in &uses {
            let sum = levels.entry((u.pid, u.tid)).or_default();
            for (s, v) in sum.iter_mut().zip([u.virtual_kib, u.resident_kib, u.threads, u.fds, 1]) {
                *s += v;
            }
        }
        before = uses;
        at = now;
        time_before = time;
        round += 1;
        if count.is_some_and(|c| round >= c) || INTERRUPTED.load(Ordering::Relaxed) {
            break;
        }
    }
    // Interrupted before the first report: nothing to average.
    if round == 0 {
        return Ok(0);
    }
    if INTERRUPTED.load(Ordering::Relaxed) {
        // Past the ^C the terminal echoed.
        writeln!(out)?;
    }
    if !shown.horizontal {
        // From the first sample to the last; the levels, their mean.
        let averaged: Vec<Use> = before
            .iter()
            .map(|u| match levels.get(&(u.pid, u.tid)) {
                Some(&[vsz, rss, threads, fds, n]) if n > 0 => {
                    let mean = |sum: u64| (sum + n / 2) / n;
                    Use {
                        virtual_kib: mean(vsz),
                        resident_kib: mean(rss),
                        threads: mean(threads),
                        fds: mean(fds),
                        ..u.clone()
                    }
                }
                _ => u.clone(),
            })
            .collect();
        let report = Round { uses: &averaged, since: &first, now: at };
        print(&mut out, &shown, &report, "Average:", "Average:", true, true)?;
    }
    Ok(0)
}

/// The tasks of a report, and what each one had used at the sample
/// before (when) or at its start.
struct Round<'a> {
    uses: &'a [Use],
    since: &'a HashMap<Key, (Use, u64)>,
    now: u64,
}

impl Round<'_> {
    /// The use of `u` since the sample before, or since it started, and
    /// over how many seconds; and what it was at that sample.
    fn delta<'a>(&'a self, u: &'a Use) -> (Use, f64, Option<&'a Use>) {
        match self.since.get(&(u.pid, u.tid)) {
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
                    (self.now.saturating_sub(*at) as f64 / 1e9).max(0.001),
                    Some(b),
                )
            }
            None => {
                (u.clone(), (self.now.saturating_sub(u.started_ns) as f64 / 1e9).max(0.001), None)
            }
        }
    }
}

/// Prints a report of `round`: each report asked for, or all side by
/// side, its heading at `time_before` (sysstat's pidstat heads a report
/// with the time of the sample before) and its lines at `time`. `first`
/// for the first report, which alone has a heading when `once`; `average`
/// for the averages.
fn print(
    out: &mut impl Write,
    shown: &Shown,
    round: &Round,
    time_before: &str,
    time: &str,
    first: bool,
    average: bool,
) -> io::Result<()> {
    let blocks: Vec<&[Report]> = if shown.horizontal {
        vec![&shown.reports[..]]
    } else {
        shown.reports.iter().map(std::slice::from_ref).collect()
    };
    let mut users: HashMap<u32, String> = HashMap::new();
    for block in blocks {
        if first || !shown.once {
            let head = if shown.horizontal { "# Time" } else { time_before };
            if !(shown.once && average) {
                writeln!(out)?;
                writeln!(out, "{}", shown.heading(head, block))?;
            }
        }
        for u in round.uses {
            let (d, seconds, before) = round.delta(u);
            if !shown.all && !block.iter().any(|r| r.active(&d, before)) {
                continue;
            }
            let user = if shown.names {
                users.entry(u.uid).or_insert_with(|| util::user_name(u.uid)).clone()
            } else {
                u.uid.to_string()
            };
            let (pid, tid) = match u.tid {
                Some(tid) if u.alone => (u.pid.to_string(), tid.to_string()),
                Some(tid) => ("-".to_string(), tid.to_string()),
                None => (u.pid.to_string(), "-".to_string()),
            };
            let cells: String =
                block.iter().map(|r| r.cells(&d, seconds, shown, average)).collect();
            writeln!(out, "{time:<11}{}{cells}  {}", shown.who(&user, &pid, &tid), u.name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(reports: Vec<Report>) -> Shown {
        Shown {
            reports,
            threads: false,
            horizontal: false,
            all: true,
            names: false,
            of_all: false,
            epoch: false,
            once: false,
            cpus: 4,
            memory_kib: 16_000_000,
        }
    }

    /// Task 21, `sh`, as the sandbox showed it: nobody's (65534), with a
    /// thread, its address space and three descriptors.
    fn sh(user_ns: u64, voluntary: u64) -> Use {
        Use {
            pid: 21,
            name: "sh".into(),
            uid: 65534,
            user_ns,
            cpu: 2,
            virtual_kib: 2824,
            resident_kib: 1808,
            voluntary,
            started_ns: 1,
            threads: 1,
            fds: 3,
            policy: "NORMAL",
            ..Use::default()
        }
    }

    /// What `print` prints of `now` against `before`, one second on.
    fn printed(shown: &Shown, now: &[Use], before: &[Use], first: bool, average: bool) -> String {
        let since: HashMap<Key, (Use, u64)> =
            before.iter().map(|u| ((u.pid, u.tid), (u.clone(), 1_000_000_000))).collect();
        let round = Round { uses: now, since: &since, now: 2_000_000_000 };
        let (before, time) =
            if average { ("Average:", "Average:") } else { ("18:35:13", "18:35:14") };
        let mut out = Vec::new();
        print(&mut out, shown, &round, before, time, first, average).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// The lines sysstat 12.6.1's pidstat printed for these, in a sandbox
    /// on Linux 6.18 (with %wait 0, which Radix does not count).
    #[test]
    fn reports_line_up_as_sysstats() {
        let before = [sh(0, 0)];
        let now = [sh(980_200_000, 1)];
        assert_eq!(
            printed(&shown(vec![Report::Cpu]), &now, &before, true, false),
            "\n18:35:13      UID       PID    %usr %system  %guest   %wait    %CPU   CPU  Command\n\
             18:35:14    65534        21   98.02    0.00    0.00    0.00   98.02     2  sh\n"
        );
        assert_eq!(
            printed(&shown(vec![Report::Cpu]), &now, &before, true, true),
            "\nAverage:      UID       PID    %usr %system  %guest   %wait    %CPU   CPU  Command\n\
             Average:    65534        21   98.02    0.00    0.00    0.00   98.02     -  sh\n"
        );
        let all =
            vec![Report::Memory, Report::Disk, Report::Switches, Report::Tasks, Report::Scheduling];
        assert_eq!(
            printed(&shown(all.clone()), &now, &before, true, false),
            "\n18:35:13      UID       PID  minflt/s  majflt/s     VSZ     RSS   %MEM  Command\n\
             18:35:14    65534        21      0.00      0.00    2824    1808   0.01  sh\n\
             \n18:35:13      UID       PID   kB_rd/s   kB_wr/s kB_ccwr/s iodelay  Command\n\
             18:35:14    65534        21      0.00      0.00      0.00       0  sh\n\
             \n18:35:13      UID       PID   cswch/s nvcswch/s  Command\n\
             18:35:14    65534        21      1.00      0.00  sh\n\
             \n18:35:13      UID       PID threads   fd-nr  Command\n\
             18:35:14    65534        21       1       3  sh\n\
             \n18:35:13      UID       PID prio policy  Command\n\
             18:35:14    65534        21    0 NORMAL  sh\n"
        );
        // Side by side, under one heading with no time.
        let mut side = shown(vec![Report::Cpu, Report::Memory]);
        side.horizontal = true;
        assert_eq!(
            printed(&side, &now, &before, true, false),
            "\n# Time        UID       PID    %usr %system  %guest   %wait    %CPU   CPU  \
             minflt/s  majflt/s     VSZ     RSS   %MEM  Command\n\
             18:35:14    65534        21   98.02    0.00    0.00    0.00   98.02     2      \
             0.00      0.00    2824    1808   0.01  sh\n"
        );
        // -I: %CPU of the 4 CPUs.
        let mut of_all = shown(vec![Report::Cpu]);
        of_all.of_all = true;
        assert_eq!(
            printed(&of_all, &now, &before, true, false).lines().nth(2),
            Some("18:35:14    65534        21   98.02    0.00    0.00    0.00   24.50     2  sh")
        );
    }

    #[test]
    fn threads_users_and_one_process() {
        let mut process = sh(0, 0);
        let mut thread = Use { tid: Some(21), name: "|__sh".into(), ..process.clone() };
        let mut threads = shown(vec![Report::Cpu]);
        threads.threads = true;
        let line = printed(&threads, &[process.clone(), thread.clone()], &[], true, false);
        assert_eq!(
            line.lines().collect::<Vec<_>>()[1..],
            [
                "18:35:13      UID      TGID       TID    %usr %system  %guest   %wait    %CPU   CPU  Command",
                "18:35:14    65534        21         -    0.00    0.00    0.00    0.00    0.00     2  sh",
                "18:35:14    65534         -        21    0.00    0.00    0.00    0.00    0.00     2  |__sh",
            ]
        );
        // A thread -C found by its own name: both numbers, its process's
        // name in brackets.
        thread.alone = true;
        thread.tid = Some(24);
        thread.name = "(python3)__worker".into();
        assert!(printed(&threads, &[thread], &[], true, false).contains(
            "18:35:14    65534        21        24    0.00    0.00    0.00    0.00    0.00     2  (python3)__worker\n"
        ));
        // -U: user names, wider.
        let mut names = shown(vec![Report::Cpu]);
        names.names = true;
        assert_eq!(
            names.heading("18:36:32", &[Report::Cpu]),
            "18:36:32        USER       PID    %usr %system  %guest   %wait    %CPU   CPU  Command"
        );
        assert_eq!(names.who("nobody", "21", "-"), "   nobody        21");
        // One process in one report: one heading, then lines only, the
        // averages too.
        let mut once = shown(vec![Report::Cpu]);
        once.once = true;
        process.user_ns = 990_000_000;
        assert_eq!(printed(&once, &[process.clone()], &[sh(0, 0)], true, false).lines().count(), 3);
        assert_eq!(
            printed(&once, &[process.clone()], &[sh(0, 0)], false, false),
            "18:35:14    65534        21   99.00    0.00    0.00    0.00   99.00     2  sh\n"
        );
        assert_eq!(
            printed(&once, &[process], &[sh(0, 0)], true, true),
            "Average:    65534        21   99.00    0.00    0.00    0.00   99.00     -  sh\n"
        );
    }

    /// Without -p only the tasks that did something show: for -v and -R,
    /// those whose levels changed, or are not a new task's.
    #[test]
    fn active_tasks() {
        let idle = sh(0, 0);
        assert!(!Report::Cpu.active(&idle, Some(&idle)));
        assert!(Report::Cpu.active(&sh(1, 0), Some(&idle)));
        assert!(Report::Switches.active(&sh(0, 1), Some(&idle)));
        assert!(!Report::Tasks.active(&idle, Some(&idle)));
        assert!(Report::Tasks.active(&idle, None));
        assert!(Report::Tasks.active(&Use { fds: 4, ..idle.clone() }, Some(&idle)));
        assert!(!Report::Scheduling.active(&idle, None));
        let fifo = Use { policy: "FIFO", priority: 10, ..idle.clone() };
        assert!(Report::Scheduling.active(&fifo, None));
        assert!(!Report::Scheduling.active(&fifo, Some(&fifo)));
        let mut cpu = shown(vec![Report::Cpu]);
        cpu.all = false;
        assert_eq!(
            printed(&cpu, std::slice::from_ref(&idle), std::slice::from_ref(&idle), true, false)
                .lines()
                .count(),
            2
        );
        assert_eq!(policy_name("round_robin"), "RR");
        assert_eq!(policy_name("other"), "NORMAL");
    }
}
