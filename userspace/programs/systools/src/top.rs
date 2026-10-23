// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! top(1): the processes using the CPU most, refreshed, with the system's
//! load, tasks, CPU time and memory above them, as procps-ng's top shows
//! them; in batch mode (`-b`) one frame after another for a log or a pipe.
//! Its CPU percentages are of one CPU, over the time since the frame
//! before.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::args::{Arg, Args};
use crate::proc::{self, Process, Stat, Thread};
use crate::util::{self, Fail, usage};

/// What the frames are sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortBy {
    Cpu,
    Memory,
    Time,
    Pid,
    Command,
}

impl SortBy {
    fn from_name(name: &str) -> Option<SortBy> {
        Some(match name.to_ascii_uppercase().as_str() {
            "%CPU" | "CPU" | "P" => SortBy::Cpu,
            "%MEM" | "MEM" | "RES" | "M" => SortBy::Memory,
            "TIME+" | "TIME" | "T" => SortBy::Time,
            "PID" | "N" => SortBy::Pid,
            "COMMAND" => SortBy::Command,
            _ => return None,
        })
    }
}

/// How top runs.
struct Settings {
    batch: bool,
    iterations: Option<u64>,
    delay: Duration,
    pids: Vec<u64>,
    threads: bool,
    sort: SortBy,
    reverse: bool,
    command_line: bool,
    per_cpu: bool,
    hide_idle: bool,
    width: Option<usize>,
    help: bool,
    /// `-u` named a user other than root, who has no processes.
    other_user: bool,
}

/// One row: a process or a thread of one.
struct Row {
    pid: u64,
    name: String,
    args: Vec<OsString>,
    state: char,
    policy: String,
    nice: i64,
    virtual_kib: u64,
    resident_kib: u64,
    cpu_ns: u64,
    percent: f64,
}

/// What top keeps from one frame for the next: each process's or thread's
/// CPU time, and when.
#[derive(Default)]
struct Before {
    cpu: HashMap<u64, u64>,
    /// `/proc/stat` at the frame before this one, and at this one.
    stat_before: Option<Stat>,
    stat: Option<Stat>,
    at_ns: u64,
}

/// Set by SIGTERM, SIGHUP or SIGINT: put the terminal back and stop.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Catches the signals that should end top cleanly, without restarting
/// the calls they interrupt, so that a wait for a key ends.
fn catch_signals() {
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGWINCH] {
        let mut action: libc::sigaction = util::unsafe_zeroed();
        action.sa_sigaction = if signal == libc::SIGWINCH {
            // Only to wake the wait: the size is read each frame.
            noop as *const () as usize
        } else {
            on_signal as *const () as usize
        };
        // SAFETY: the handlers only store to an atomic; the action is
        // valid for the call.
        unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
    }
}

extern "C" fn noop(_: libc::c_int) {}

/// The terminal in raw mode, on the alternate screen, until dropped.
struct Screen {
    saved: Option<libc::termios>,
}

impl Screen {
    fn new() -> Screen {
        let mut saved: libc::termios = util::unsafe_zeroed();
        // SAFETY: tcgetattr fills the termios it is given.
        let ok = unsafe { libc::tcgetattr(0, &mut saved) } == 0;
        if ok {
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            // SAFETY: `raw` is a valid termios.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) };
        }
        print!("\x1b[?1049h\x1b[?25l");
        let _ = io::stdout().flush();
        Screen { saved: ok.then_some(saved) }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        print!("\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
        if let Some(saved) = self.saved {
            // SAFETY: `saved` is the termios read at the start.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
        }
    }
}

/// Waits up to `delay` for a key; returns it if one came.
fn wait_key(delay: Duration) -> Option<u8> {
    let mut fds = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    let ms = delay.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one valid pollfd.
    let ready = unsafe { libc::poll(&mut fds, 1, ms) };
    if ready <= 0 {
        return None;
    }
    let mut key = 0u8;
    // SAFETY: reads one byte into `key`.
    let n = unsafe { libc::read(0, (&mut key as *mut u8).cast(), 1) };
    (n == 1).then_some(key)
}

/// top(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut s = Settings {
        batch: false,
        iterations: None,
        delay: Duration::from_secs(3),
        pids: Vec::new(),
        threads: false,
        sort: SortBy::Cpu,
        reverse: false,
        command_line: false,
        per_cpu: false,
        hide_idle: false,
        width: None,
        help: false,
        other_user: false,
    };
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('b') => s.batch = true,
            Arg::Short('n') => s.iterations = Some(args.number()?),
            Arg::Short('d') => {
                let seconds: f64 = args
                    .text()?
                    .replace(',', ".")
                    .parse()
                    .map_err(|_| usage("bad delay interval".into()))?;
                if !(seconds > 0.0 && seconds.is_finite()) {
                    return Err(usage("bad delay interval".into()));
                }
                s.delay = Duration::from_secs_f64(seconds);
            }
            Arg::Short('p') => {
                for pid in util::list(&args.text()?) {
                    s.pids.push(pid.parse().map_err(|_| usage(format!("bad pid '{pid}'")))?);
                }
            }
            Arg::Short('u' | 'U') => {
                let name = args.text()?;
                let uid =
                    util::user_id(&name).ok_or_else(|| usage(format!("invalid user '{name}'")))?;
                // Every process is root's.
                s.other_user = uid != 0;
            }
            Arg::Short('H') => s.threads = true,
            Arg::Short('o') => {
                let field = args.text()?;
                let (reverse, name) = match field.strip_prefix('+') {
                    Some(name) => (false, name.to_string()),
                    None => (field.starts_with('-'), field.trim_start_matches('-').to_string()),
                };
                s.sort = SortBy::from_name(&name)
                    .ok_or_else(|| usage(format!("unrecognized field name '{name}'")))?;
                s.reverse = reverse;
            }
            Arg::Short('c') => s.command_line = true,
            Arg::Short('1') => s.per_cpu = true,
            Arg::Short('i') => s.hide_idle = true,
            Arg::Short('w') => {
                s.width = Some(args.joined_value().map_or(Ok(512), |w| {
                    w.to_string_lossy().parse().map_err(|_| usage("bad width".into()))
                })?)
            }
            Arg::Short('s' | 'S' | 'E' | 'e') => {}
            Arg::Short('h') => {
                println!(
                    "usage: top [-bcHi1] [-d SECONDS] [-n NUMBER] [-o FIELD] [-p PID,...] [-u USER] [-w [COLUMNS]]"
                );
                return Ok(0);
            }
            Arg::Short('v') => {
                println!("top from Dukovo's systools {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
            other => return Err(args.unknown(&other)),
        }
    }
    if !s.batch && !util::is_terminal(0) {
        // No terminal to read keys from: run as in batch mode.
        s.batch = true;
    }
    catch_signals();
    let screen = if s.batch { None } else { Some(Screen::new()) };
    let mut before = Before::default();
    // A first look, so that the first frame has CPU use to show.
    sample(&s, &mut before)?;
    std::thread::sleep(Duration::from_millis(200));
    let mut frame = 0u64;
    let mut out = io::stdout().lock();
    loop {
        let rows = sample(&s, &mut before)?;
        let text = render(&s, &before, &rows);
        if s.batch {
            write!(out, "{text}")?;
            writeln!(out)?;
        } else {
            // Over the last frame, each line cleared to its end.
            let lines: Vec<String> = text.lines().map(|l| format!("{l}\x1b[K")).collect();
            write!(out, "\x1b[H{}\x1b[J", lines.join("\r\n"))?;
        }
        out.flush()?;
        frame += 1;
        if s.iterations.is_some_and(|n| frame >= n) || STOP.load(Ordering::Relaxed) {
            break;
        }
        if s.batch {
            std::thread::sleep(s.delay);
            if STOP.load(Ordering::Relaxed) {
                break;
            }
            continue;
        }
        if let Some(key) = wait_key(s.delay) {
            match key {
                b'q' | 3 | 4 => break,
                b'P' => s.sort = SortBy::Cpu,
                b'M' => s.sort = SortBy::Memory,
                b'T' => s.sort = SortBy::Time,
                b'N' => s.sort = SortBy::Pid,
                b'R' => s.reverse = !s.reverse,
                b'c' => s.command_line = !s.command_line,
                b'H' => s.threads = !s.threads,
                b'1' => s.per_cpu = !s.per_cpu,
                b'i' => s.hide_idle = !s.hide_idle,
                b'h' | b'?' => s.help = !s.help,
                _ => {}
            }
        }
        if STOP.load(Ordering::Relaxed) {
            break;
        }
    }
    drop(out);
    drop(screen);
    Ok(0)
}

/// Reads the processes, works out their CPU use since `before`, and
/// keeps what the next frame needs.
fn sample(s: &Settings, before: &mut Before) -> Result<Vec<Row>, Fail> {
    let now = util::monotonic_ns();
    let processes = proc::processes().map_err(|e| util::failed(&"/proc", e))?;
    let span = now.saturating_sub(before.at_ns).max(1) as f64;
    let mut rows = Vec::new();
    let mut cpu = HashMap::new();
    let wanted = |p: &&Process| !s.other_user && (s.pids.is_empty() || s.pids.contains(&p.number));
    for p in processes.iter().filter(wanted) {
        let args = if s.command_line { proc::cmdline(p.number) } else { Vec::new() };
        if s.threads && !p.zombie() {
            for t in proc::threads(p.number) {
                rows.push(thread_row(p, &t, &args, before, span));
                cpu.insert(t.number, t.user_ns + t.system_ns);
            }
        } else {
            rows.push(process_row(p, args, before, span));
            cpu.insert(p.number, p.cpu_ns());
        }
    }
    before.cpu = cpu;
    before.at_ns = now;
    before.stat_before = before.stat.take();
    before.stat = proc::stat().ok();
    Ok(rows)
}

fn percent(id: u64, cpu_ns: u64, before: &Before, span: f64) -> f64 {
    let was = before.cpu.get(&id).copied().unwrap_or(0);
    cpu_ns.saturating_sub(was) as f64 * 100.0 / span
}

fn process_row(p: &Process, args: Vec<OsString>, before: &Before, span: f64) -> Row {
    Row {
        pid: p.number,
        name: p.name.clone(),
        args,
        state: p.state_letter(),
        policy: p.policy.clone(),
        nice: p.nice,
        virtual_kib: p.virtual_kib,
        resident_kib: p.resident_kib,
        cpu_ns: p.cpu_ns(),
        percent: percent(p.number, p.cpu_ns(), before, span),
    }
}

fn thread_row(p: &Process, t: &Thread, args: &[OsString], before: &Before, span: f64) -> Row {
    let cpu_ns = t.user_ns + t.system_ns;
    Row {
        pid: t.number,
        name: t.name.clone(),
        args: args.to_vec(),
        state: if p.run == "stopped" { 'T' } else { proc::run_letter(&t.run) },
        policy: t.policy.clone(),
        nice: t.nice,
        virtual_kib: p.virtual_kib,
        resident_kib: p.resident_kib,
        cpu_ns,
        percent: percent(t.number, cpu_ns, before, span),
    }
}

/// A frame: the summary and the rows that fit.
fn render(s: &Settings, before: &Before, rows: &[Row]) -> String {
    let (width, height) = if s.batch {
        (s.width.unwrap_or(usize::MAX), usize::MAX)
    } else {
        util::terminal_size(1).unwrap_or((80, 24))
    };
    let mut lines: Vec<String> = Vec::new();
    let processes = proc::processes().unwrap_or_default();
    let users = processes.iter().filter(|p| p.number == p.session && p.terminal.is_some()).count();
    let load = proc::loadavg();
    let now_s = (util::now_ns() / 1_000_000_000) as i64;
    lines.push(format!(
        "top - {} up {},  {users} user{},  load average: {:.2}, {:.2}, {:.2}",
        util::local_time(now_s, "%H:%M:%S"),
        util::up_for(proc::uptime_ns() / 1_000_000_000),
        if users > 1 { "s" } else { "" },
        load[0],
        load[1],
        load[2]
    ));
    let count = |state: char| rows.iter().filter(|r| r.state == state).count();
    let running = count('R');
    let stopped = count('T');
    let zombie = count('Z');
    lines.push(format!(
        "{}: {:3} total, {:3} running, {:3} sleeping, {:3} stopped, {:3} zombie",
        if s.threads { "Threads" } else { "Tasks" },
        rows.len(),
        running,
        rows.len() - running - stopped - zombie,
        stopped,
        zombie
    ));
    // CPU time since the frame before, by CPU and in all.
    if let (Some(stat), Some(now)) = (&before.stat_before, &before.stat) {
        let cpu_line = |label: &str, user: u64, system: u64, idle: u64| {
            let all = (user + system + idle).max(1) as f64;
            format!(
                "{label}:{:5.1} us,{:5.1} sy,  0.0 ni,{:5.1} id,  0.0 wa,  0.0 hi,  0.0 si,  0.0 st",
                user as f64 * 100.0 / all,
                system as f64 * 100.0 / all,
                idle as f64 * 100.0 / all
            )
        };
        if s.per_cpu {
            for (cpu, times) in &now.cpus {
                let was =
                    stat.cpus.iter().find(|(c, _)| c == cpu).map(|(_, t)| *t).unwrap_or_default();
                lines.push(cpu_line(
                    &format!("%Cpu{cpu:<3}"),
                    times.user_ns.saturating_sub(was.user_ns),
                    times.system_ns.saturating_sub(was.system_ns),
                    times.idle_ns.saturating_sub(was.idle_ns),
                ));
            }
        } else {
            lines.push(cpu_line(
                "%Cpu(s)",
                now.all.user_ns.saturating_sub(stat.all.user_ns),
                now.all.system_ns.saturating_sub(stat.all.system_ns),
                now.all.idle_ns.saturating_sub(stat.all.idle_ns),
            ));
        }
    }
    let m = proc::memory().unwrap_or_default();
    let mib = |kib: u64| kib as f64 / 1024.0;
    lines.push(format!(
        "MiB Mem : {:8.1} total, {:8.1} free, {:8.1} used, {:8.1} buff/cache",
        mib(m.total),
        mib(m.free),
        mib(m.used()),
        mib(m.cache)
    ));
    lines.push(format!(
        "MiB Swap: {:8.1} total, {:8.1} free, {:8.1} used. {:8.1} avail Mem",
        0.0,
        0.0,
        0.0,
        mib(m.available())
    ));
    if s.help {
        lines.push(String::new());
        lines.push(
            "Keys: q quit   P sort by %CPU   M by %MEM   T by TIME+   N by PID   R reverse".into(),
        );
        lines.push(
            "      c command lines   H threads   1 each CPU   i hide idle   h this help".into(),
        );
    }
    lines.push(String::new());
    lines.push(format!(
        "{:>7} {:<8} {:>3} {:>3} {:>7} {:>6} {:>6} S {:>5} {:>5} {:>9} COMMAND",
        "PID", "USER", "PR", "NI", "VIRT", "RES", "SHR", "%CPU", "%MEM", "TIME+"
    ));
    let mut shown: Vec<&Row> =
        rows.iter().filter(|r| !s.hide_idle || r.percent > 0.0 || r.state == 'R').collect();
    shown.sort_by(|a, b| {
        let ord = match s.sort {
            SortBy::Cpu => b.percent.total_cmp(&a.percent).then(b.cpu_ns.cmp(&a.cpu_ns)),
            SortBy::Memory => b.resident_kib.cmp(&a.resident_kib),
            SortBy::Time => b.cpu_ns.cmp(&a.cpu_ns),
            SortBy::Pid => a.pid.cmp(&b.pid),
            SortBy::Command => a.name.cmp(&b.name),
        };
        if s.reverse { ord.reverse() } else { ord }
    });
    let user = util::user_name(0);
    let room = height.saturating_sub(lines.len());
    for r in shown.into_iter().take(room) {
        let centis = r.cpu_ns / 10_000_000;
        let time = format!("{}:{:02}.{:02}", centis / 6000, centis / 100 % 60, centis % 100);
        let pr = match r.policy.as_str() {
            "fifo" | "round_robin" | "rr" => "rt".to_string(),
            _ => (20 + r.nice).to_string(),
        };
        let command = if s.command_line && !r.args.is_empty() {
            r.args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
        } else {
            r.name.clone()
        };
        let pmem = if m.total == 0 { 0.0 } else { r.resident_kib as f64 * 100.0 / m.total as f64 };
        lines.push(format!(
            "{:>7} {:<8} {:>3} {:>3} {:>7} {:>6} {:>6} {} {:>5.1} {:>5.1} {:>9} {}",
            r.pid,
            user.chars().take(8).collect::<String>(),
            pr,
            r.nice,
            r.virtual_kib,
            r.resident_kib,
            0,
            r.state,
            r.percent,
            pmem,
            time,
            command
        ));
    }
    let mut text = String::new();
    for line in lines {
        let line: String =
            if width == usize::MAX { line } else { line.chars().take(width).collect() };
        text.push_str(&line);
        text.push('\n');
    }
    text
}
