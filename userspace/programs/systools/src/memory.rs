// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! free(1) and vmstat(8), from `/proc/meminfo` and `/proc/stat`. Radix has
//! no swap until Phase 10 and counts no interrupts per CPU, so those
//! columns are 0; the page cache is free(1)'s buff/cache, which the kernel
//! can drop for programs, so it counts as available.

use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use crate::args::{Arg, Args};
use crate::proc::{self, Memory, Stat};
use crate::util::{self, Fail, usage};

/// How free(1) shows amounts.
#[derive(Clone, Copy)]
enum Unit {
    /// A power of 1024 (or 1000) of bytes.
    Scaled(u64),
    Human {
        si: bool,
    },
}

fn show(kib: u64, unit: Unit) -> String {
    match unit {
        Unit::Scaled(bytes) => (kib * 1024 / bytes).to_string(),
        Unit::Human { si } => {
            let text = util::human(kib * 1024, si);
            // free(1) says B, Ki, Mi...
            match text.chars().last() {
                Some(c) if c.is_ascii_alphabetic() && !si => format!("{text}i"),
                Some(c) if c.is_ascii_digit() => format!("{text}B"),
                _ => text,
            }
        }
    }
}

/// free(1).
pub fn free(args: &mut Args) -> Result<i32, Fail> {
    let mut unit = Unit::Scaled(1024);
    let mut si = false;
    let mut total = false;
    let mut wide = false;
    let mut every: Option<f64> = None;
    let mut count: Option<u64> = None;
    let mut lohi = false;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('b') => unit = Unit::Scaled(1),
            Arg::Long(l) if l == "bytes" => unit = Unit::Scaled(1),
            Arg::Short('k') => unit = Unit::Scaled(1024),
            Arg::Long(l) if l == "kibi" => unit = Unit::Scaled(1024),
            Arg::Short('m') => unit = Unit::Scaled(1 << 20),
            Arg::Long(l) if l == "mebi" => unit = Unit::Scaled(1 << 20),
            Arg::Short('g') => unit = Unit::Scaled(1 << 30),
            Arg::Long(l) if l == "gibi" => unit = Unit::Scaled(1 << 30),
            Arg::Long(l) if l == "tebi" => unit = Unit::Scaled(1 << 40),
            Arg::Long(l) if l == "pebi" => unit = Unit::Scaled(1 << 50),
            Arg::Long(l) if l == "kilo" => unit = Unit::Scaled(1000),
            Arg::Long(l) if l == "mega" => unit = Unit::Scaled(1_000_000),
            Arg::Long(l) if l == "giga" => unit = Unit::Scaled(1_000_000_000),
            Arg::Long(l) if l == "tera" => unit = Unit::Scaled(1_000_000_000_000),
            Arg::Short('h') => unit = Unit::Human { si: false },
            Arg::Long(l) if l == "human" => unit = Unit::Human { si: false },
            Arg::Long(l) if l == "si" => si = true,
            Arg::Short('t') => total = true,
            Arg::Long(l) if l == "total" => total = true,
            Arg::Short('w') => wide = true,
            Arg::Long(l) if l == "wide" => wide = true,
            Arg::Short('l') => lohi = true,
            Arg::Long(l) if l == "lohi" => lohi = true,
            Arg::Short('s') => every = Some(args.number()?),
            Arg::Long(l) if l == "seconds" => every = Some(args.number()?),
            Arg::Short('c') => count = Some(args.number()?),
            Arg::Long(l) if l == "count" => count = Some(args.number()?),
            Arg::Short('v' | 'L') => {}
            other => return Err(args.unknown(&other)),
        }
    }
    if let Unit::Human { .. } = unit {
        unit = Unit::Human { si };
    } else if si && let Unit::Scaled(bytes) = unit {
        // --si takes powers of 1000 for the unit asked.
        let power = (bytes.max(1) as f64).log(1024.0).round() as u32;
        unit = Unit::Scaled(1000u64.pow(power));
    }
    if every.is_some_and(|s| s <= 0.0) {
        return Err(usage("seconds argument must be positive".into()));
    }
    let rounds = count.unwrap_or(if every.is_some() { u64::MAX } else { 1 });
    let mut out = io::stdout().lock();
    for round in 0..rounds {
        if round > 0 {
            writeln!(out)?;
            out.flush()?;
            thread::sleep(Duration::from_secs_f64(every.unwrap_or(1.0)));
        }
        let m = proc::memory().map_err(|e| util::failed(&"/proc/meminfo", e))?;
        let w = 11;
        if wide {
            writeln!(
                out,
                "{:7}{:>w$} {:>w$} {:>w$} {:>w$} {:>w$} {:>w$} {:>w$}",
                "", "total", "used", "free", "shared", "buffers", "cache", "available"
            )?;
            writeln!(
                out,
                "{:7}{:>w$} {:>w$} {:>w$} {:>w$} {:>w$} {:>w$} {:>w$}",
                "Mem:",
                show(m.total, unit),
                show(m.used(), unit),
                show(m.free, unit),
                show(0, unit),
                show(0, unit),
                show(m.cache, unit),
                show(m.available(), unit)
            )?;
        } else {
            writeln!(
                out,
                "{:7}{:>w$} {:>w$} {:>w$} {:>w$} {:>w$} {:>w$}",
                "", "total", "used", "free", "shared", "buff/cache", "available"
            )?;
            writeln!(
                out,
                "{:7}{:>w$} {:>w$} {:>w$} {:>w$} {:>w$} {:>w$}",
                "Mem:",
                show(m.total, unit),
                show(m.used(), unit),
                show(m.free, unit),
                show(0, unit),
                show(m.cache, unit),
                show(m.available(), unit)
            )?;
        }
        if lohi {
            writeln!(
                out,
                "{:7}{:>w$} {:>w$} {:>w$}",
                "Low:",
                show(m.total, unit),
                show(m.used(), unit),
                show(m.free, unit)
            )?;
            writeln!(
                out,
                "{:7}{:>w$} {:>w$} {:>w$}",
                "High:",
                show(0, unit),
                show(0, unit),
                show(0, unit)
            )?;
        }
        writeln!(
            out,
            "{:7}{:>w$} {:>w$} {:>w$}",
            "Swap:",
            show(0, unit),
            show(0, unit),
            show(0, unit)
        )?;
        if total {
            writeln!(
                out,
                "{:7}{:>w$} {:>w$} {:>w$}",
                "Total:",
                show(m.total, unit),
                show(m.used(), unit),
                show(m.free, unit)
            )?;
        }
    }
    Ok(0)
}

/// What vmstat(8) shows of one sample, against the one before it (or
/// boot).
struct Sample {
    stat: Stat,
    memory: Memory,
    at_ns: u64,
}

fn sample() -> Result<Sample, Fail> {
    Ok(Sample {
        stat: proc::stat().map_err(|e| util::failed(&"/proc/stat", e))?,
        memory: proc::memory().map_err(|e| util::failed(&"/proc/meminfo", e))?,
        at_ns: util::monotonic_ns(),
    })
}

/// vmstat(8).
pub fn vmstat(args: &mut Args) -> Result<i32, Fail> {
    let mut delay: Option<u64> = None;
    let mut count: Option<u64> = None;
    let mut wide = false;
    let mut once = false;
    let mut timestamp = false;
    let mut summary = false;
    let mut disks = false;
    let mut unit = 1024u64;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('w') => wide = true,
            Arg::Long(l) if l == "wide" => wide = true,
            Arg::Short('n') => once = true,
            Arg::Long(l) if l == "one-header" => once = true,
            Arg::Short('t') => timestamp = true,
            Arg::Long(l) if l == "timestamp" => timestamp = true,
            Arg::Short('s') => summary = true,
            Arg::Long(l) if l == "stats" => summary = true,
            Arg::Short('d') => disks = true,
            Arg::Long(l) if l == "disk" => disks = true,
            Arg::Short('a' | 'y') => {}
            Arg::Short('S') => {
                unit = match args.text()?.as_str() {
                    "k" => 1000,
                    "K" => 1024,
                    "m" => 1_000_000,
                    "M" => 1 << 20,
                    other => return Err(usage(format!("-S requires k, K, m or M (got {other})"))),
                }
            }
            Arg::Operand(n) => {
                let n: u64 = n
                    .to_string_lossy()
                    .parse()
                    .map_err(|_| usage("delay and count are numbers".into()))?;
                if delay.is_none() {
                    delay = Some(n);
                } else {
                    count = Some(n);
                }
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let mut out = io::stdout().lock();
    let first = sample()?;
    if summary {
        let s = &first.stat;
        let m = &first.memory;
        let ticks = |ns: u64| ns / 10_000_000;
        let lines: [(u64, &str); 16] = [
            (m.total, "K total memory"),
            (m.used(), "K used memory"),
            (m.free, "K free memory"),
            (0, "K buffer memory"),
            (m.cache, "K swap cache"),
            (0, "K total swap"),
            (0, "K used swap"),
            (0, "K free swap"),
            (ticks(s.all.user_ns), "non-nice user cpu ticks"),
            (ticks(s.all.system_ns), "system cpu ticks"),
            (ticks(s.all.idle_ns), "idle cpu ticks"),
            (s.disk_read_sectors, "pages paged in"),
            (s.disk_written_sectors, "pages paged out"),
            (0, "interrupts"),
            (s.all.switches, "CPU context switches"),
            (s.boot_time_ns / 1_000_000_000, "boot time"),
        ];
        for (n, what) in lines {
            writeln!(out, "{n:>13} {what}")?;
        }
        return Ok(0);
    }
    if disks {
        let s = &first.stat;
        writeln!(out, "disk- ------------reads------------ ------------writes-----------")?;
        writeln!(out, "       total merged sectors      ms  total merged sectors      ms")?;
        writeln!(
            out,
            "all   {:>6} {:>6} {:>7} {:>7} {:>6} {:>6} {:>7} {:>7}",
            s.disk_reads, 0, s.disk_read_sectors, 0, s.disk_writes, 0, s.disk_written_sectors, 0
        )?;
        return Ok(0);
    }
    let rounds = count.unwrap_or(if delay.is_some() { u64::MAX } else { 1 });
    let height = util::terminal_size(1).map_or(24, |(_, rows)| rows.max(5));
    let mut before: Option<Sample> = None;
    let mut now = first;
    for round in 0..rounds {
        if round > 0 {
            out.flush()?;
            thread::sleep(Duration::from_secs(delay.unwrap_or(1).max(1)));
            before = Some(now);
            now = sample()?;
        }
        if round == 0 || (!once && (round as usize).is_multiple_of(height - 2)) {
            header(&mut out, wide, timestamp)?;
        }
        writeln!(out, "{}", line(&now, before.as_ref(), wide, timestamp, unit))?;
    }
    Ok(0)
}

fn header(out: &mut impl Write, wide: bool, timestamp: bool) -> io::Result<()> {
    let ts = if timestamp { " -----timestamp-----" } else { "" };
    if wide {
        writeln!(
            out,
            "--procs-- -----------------------memory---------------------- ---swap-- -----io---- -system-- ----------cpu----------{ts}"
        )?;
        writeln!(
            out,
            "   r    b         swpd         free         buff        cache   si   so    bi    bo   in   cs  us  sy  id  wa  st  gu{}",
            if timestamp { "                 UTC" } else { "" }
        )
    } else {
        writeln!(
            out,
            "procs -----------memory---------- ---swap-- -----io---- -system-- -------cpu-------{ts}"
        )?;
        writeln!(
            out,
            " r  b   swpd   free   buff  cache   si   so    bi    bo   in   cs us sy id wa st gu{}",
            if timestamp { "                 UTC" } else { "" }
        )
    }
}

fn line(now: &Sample, before: Option<&Sample>, wide: bool, timestamp: bool, unit: u64) -> String {
    let s = &now.stat;
    let (cpu, seconds, read, written, switches) = match before {
        Some(b) => {
            let d = |a: u64, b: u64| a.saturating_sub(b);
            let cpu = (
                d(s.all.user_ns, b.stat.all.user_ns),
                d(s.all.system_ns, b.stat.all.system_ns),
                d(s.all.idle_ns, b.stat.all.idle_ns),
            );
            let seconds = (now.at_ns.saturating_sub(b.at_ns) as f64 / 1e9).max(0.001);
            (
                cpu,
                seconds,
                d(s.disk_read_sectors, b.stat.disk_read_sectors),
                d(s.disk_written_sectors, b.stat.disk_written_sectors),
                d(s.all.switches, b.stat.all.switches),
            )
        }
        // The first line: averages since boot.
        None => {
            let seconds = (proc::uptime_ns() as f64 / 1e9).max(0.001);
            (
                (s.all.user_ns, s.all.system_ns, s.all.idle_ns),
                seconds,
                s.disk_read_sectors,
                s.disk_written_sectors,
                s.all.switches,
            )
        }
    };
    let all = (cpu.0 + cpu.1 + cpu.2).max(1);
    let pct = |part: u64| (part * 100 + all / 2) / all;
    let per = |n: u64| (n as f64 / seconds).round() as u64;
    let m = &now.memory;
    let mem = |kib: u64| kib * 1024 / unit;
    // Blocks of 1 KiB, as Linux's bi and bo count them.
    let (bi, bo) = (per(read / 2), per(written / 2));
    let mut text = if wide {
        format!(
            "{:>4} {:>4} {:>12} {:>12} {:>12} {:>12} {:>4} {:>4} {:>5} {:>5} {:>4} {:>4} {:>3} {:>3} {:>3} {:>3} {:>3} {:>3}",
            s.running,
            s.disk_waiting,
            0,
            mem(m.free),
            0,
            mem(m.cache),
            0,
            0,
            bi,
            bo,
            0,
            per(switches),
            pct(cpu.0),
            pct(cpu.1),
            pct(cpu.2),
            0,
            0,
            0
        )
    } else {
        format!(
            "{:>2} {:>2} {:>6} {:>6} {:>6} {:>6} {:>4} {:>4} {:>5} {:>5} {:>4} {:>4} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2}",
            s.running,
            s.disk_waiting,
            0,
            mem(m.free),
            0,
            mem(m.cache),
            0,
            0,
            bi,
            bo,
            0,
            per(switches),
            pct(cpu.0),
            pct(cpu.1),
            pct(cpu.2),
            0,
            0,
            0
        )
    };
    if timestamp {
        let now_s = (util::now_ns() / 1_000_000_000) as i64;
        text += &format!(" {}", util::local_time(now_s, "%Y-%m-%d %H:%M:%S"));
    }
    text
}
