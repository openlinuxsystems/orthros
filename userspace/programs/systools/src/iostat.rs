// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! iostat(1): what the CPUs and the disks have done, as sysstat's iostat
//! reports it: since boot, then over each interval. The disks' numbers
//! are in `/kernel/block/<disk>/stat`, in Linux's 17 fields
//! (radix-sysfs(5)); the CPUs' times in `/processes/stat`.

use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use crate::args::{Arg, Args};
use crate::probe;
use crate::proc::{self, CpuTimes};
use crate::util::{self, Fail, usage};

/// One disk's numbers, as its `stat` file has them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DiskStat {
    reads: u64,
    read_sectors: u64,
    read_ms: u64,
    writes: u64,
    written_sectors: u64,
    write_ms: u64,
    busy_ms: u64,
    queue_ms: u64,
    discards: u64,
    discarded_sectors: u64,
    discard_ms: u64,
    flushes: u64,
    flush_ms: u64,
}

/// Reads Linux's 17 fields, or the first 11 of an older kernel.
fn parse_stat(text: &str) -> Option<DiskStat> {
    let fields: Vec<u64> =
        text.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
    let at = |i: usize| fields.get(i).copied().unwrap_or(0);
    (fields.len() >= 11).then(|| DiskStat {
        reads: at(0),
        read_sectors: at(2),
        read_ms: at(3),
        writes: at(4),
        written_sectors: at(6),
        write_ms: at(7),
        busy_ms: at(9),
        queue_ms: at(10),
        discards: at(11),
        discarded_sectors: at(13),
        discard_ms: at(14),
        flushes: at(15),
        flush_ms: at(16),
    })
}

/// The disks that keep a `stat`, by name.
fn disks() -> io::Result<Vec<String>> {
    Ok(probe::devices()?
        .into_iter()
        .filter(|(name, part)| !part && probe::block_text(name, "stat").is_some())
        .map(|(name, _)| name)
        .collect())
}

/// How the report is laid out.
#[derive(Clone, Copy)]
struct Options {
    cpu: bool,
    devices: bool,
    extended: bool,
    /// kB or MB.
    megabytes: bool,
    /// Leave out disks that did nothing in the interval.
    active_only: bool,
    time: bool,
}

/// The CPUs' line: user, system and idle time as shares of all of it.
/// Radix counts no nice, I/O wait or stolen time, which show 0.00.
fn cpu_report(out: &mut impl Write, now: CpuTimes, then: CpuTimes) -> io::Result<()> {
    let user = now.user_ns.saturating_sub(then.user_ns) as f64;
    let system = now.system_ns.saturating_sub(then.system_ns) as f64;
    let idle = now.idle_ns.saturating_sub(then.idle_ns) as f64;
    let all = (user + system + idle).max(1.0);
    let share = |part: f64| part * 100.0 / all;
    writeln!(out, "avg-cpu:  %user   %nice %system %iowait  %steal   %idle")?;
    writeln!(
        out,
        "       {:>8.2}{:>8.2}{:>8.2}{:>8.2}{:>8.2}{:>8.2}",
        share(user),
        0.0,
        share(system),
        0.0,
        0.0,
        share(idle)
    )?;
    writeln!(out)
}

/// The disks' table over `seconds`, from `then` to `now`.
fn disk_report(
    out: &mut impl Write,
    rows: &[(String, DiskStat, DiskStat)],
    seconds: f64,
    options: Options,
) -> io::Result<()> {
    let seconds = seconds.max(1e-9);
    let (unit, per) = if options.megabytes { ("MB", 2048.0) } else { ("kB", 2.0) };
    if options.extended {
        writeln!(
            out,
            "{:<13} {:>7} {:>9} {:>8} {:>6} {:>7} {:>8} {:>7} {:>9} {:>8} {:>6} {:>7} {:>8} {:>7} {:>9} {:>8} {:>6} {:>7} {:>8} {:>7} {:>7} {:>7} {:>6}",
            "Device",
            "r/s",
            format!("r{unit}/s"),
            "rrqm/s",
            "%rrqm",
            "r_await",
            "rareq-sz",
            "w/s",
            format!("w{unit}/s"),
            "wrqm/s",
            "%wrqm",
            "w_await",
            "wareq-sz",
            "d/s",
            format!("d{unit}/s"),
            "drqm/s",
            "%drqm",
            "d_await",
            "dareq-sz",
            "f/s",
            "f_await",
            "aqu-sz",
            "%util"
        )?;
    } else {
        writeln!(
            out,
            "{:<13} {:>8} {:>12} {:>12} {:>12} {:>10} {:>10} {:>10}",
            "Device",
            "tps",
            format!("{unit}_read/s"),
            format!("{unit}_wrtn/s"),
            format!("{unit}_dscd/s"),
            format!("{unit}_read"),
            format!("{unit}_wrtn"),
            format!("{unit}_dscd")
        )?;
    }
    for (name, now, then) in rows {
        let d = |f: fn(&DiskStat) -> u64| f(now).saturating_sub(f(then)) as f64;
        let (reads, writes, discards, flushes) =
            (d(|s| s.reads), d(|s| s.writes), d(|s| s.discards), d(|s| s.flushes));
        if options.active_only && reads + writes + discards + flushes == 0.0 {
            continue;
        }
        let (read, written, discarded) =
            (d(|s| s.read_sectors), d(|s| s.written_sectors), d(|s| s.discarded_sectors));
        if options.extended {
            let wait = |ms: f64, n: f64| if n > 0.0 { ms / n } else { 0.0 };
            let size = |sectors: f64, n: f64| if n > 0.0 { sectors / 2.0 / n } else { 0.0 };
            let busy = (d(|s| s.busy_ms) / 10.0 / seconds).min(100.0);
            writeln!(
                out,
                "{:<13} {:>7.2} {:>9.2} {:>8.2} {:>6.2} {:>7.2} {:>8.2} {:>7.2} {:>9.2} {:>8.2} {:>6.2} {:>7.2} {:>8.2} {:>7.2} {:>9.2} {:>8.2} {:>6.2} {:>7.2} {:>8.2} {:>7.2} {:>7.2} {:>7.2} {:>6.2}",
                name,
                reads / seconds,
                read / per / seconds,
                0.0,
                0.0,
                wait(d(|s| s.read_ms), reads),
                size(read, reads),
                writes / seconds,
                written / per / seconds,
                0.0,
                0.0,
                wait(d(|s| s.write_ms), writes),
                size(written, writes),
                discards / seconds,
                discarded / per / seconds,
                0.0,
                0.0,
                wait(d(|s| s.discard_ms), discards),
                size(discarded, discards),
                flushes / seconds,
                wait(d(|s| s.flush_ms), flushes),
                d(|s| s.queue_ms) / 1000.0 / seconds,
                busy
            )?;
        } else {
            let total = |sectors: f64| (sectors / per) as u64;
            writeln!(
                out,
                "{:<13} {:>8.2} {:>12.2} {:>12.2} {:>12.2} {:>10} {:>10} {:>10}",
                name,
                (reads + writes + discards) / seconds,
                read / per / seconds,
                written / per / seconds,
                discarded / per / seconds,
                total(read),
                total(written),
                total(discarded)
            )?;
        }
    }
    writeln!(out)
}

/// Each disk's numbers now; a disk that cannot be read has none.
fn sample(names: &[String]) -> Vec<(String, DiskStat)> {
    names
        .iter()
        .map(|name| {
            let stat = probe::block_text(name, "stat").and_then(|t| parse_stat(&t));
            (name.clone(), stat.unwrap_or_default())
        })
        .collect()
}

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut options = Options {
        cpu: true,
        devices: true,
        extended: false,
        megabytes: false,
        active_only: false,
        time: false,
    };
    let (mut only_cpu, mut only_devices, mut skip_first) = (false, false, false);
    let (mut numbers, mut names) = (Vec::new(), Vec::new());
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('c') => only_cpu = true,
            Arg::Short('d') => only_devices = true,
            Arg::Short('x') => options.extended = true,
            Arg::Short('k') => options.megabytes = false,
            Arg::Short('m') => options.megabytes = true,
            Arg::Short('y') => skip_first = true,
            Arg::Short('z') => options.active_only = true,
            Arg::Short('t') => options.time = true,
            Arg::Operand(word) => {
                let word = word.to_string_lossy();
                match word.parse::<u64>() {
                    Ok(n) => numbers.push(n),
                    Err(_) => {
                        let name =
                            word.strip_prefix("/devices/").or_else(|| word.strip_prefix("/dev/"));
                        names.push(name.unwrap_or(&word).to_string());
                    }
                }
            }
            other => return Err(args.unknown(&other)),
        }
    }
    if only_cpu || only_devices {
        options.cpu = only_cpu;
        options.devices = only_devices;
    }
    let (interval, count) = match numbers.as_slice() {
        [] => (None, Some(1)),
        [interval] => (Some(*interval), None),
        [interval, count] => (Some(*interval), Some(*count)),
        _ => return Err(usage("iostat [-cdkmtxyz] [device...] [interval [count]]".into())),
    };
    if interval == Some(0) {
        return Err(usage("the interval must be at least a second".into()));
    }
    let all = disks().map_err(|e| util::failed(&probe::BLOCKS, e))?;
    for name in &names {
        if !all.contains(name) {
            return Err(util::fail(format!("{name}: no such disk keeps a stat")));
        }
    }
    let names = if names.is_empty() { all } else { names };
    let mut out = io::stdout().lock();
    let (sysname, release, machine) = util::uname();
    let cpus = proc::stat().map_or(1, |s| s.cpus.len());
    let date = util::local_time((util::now_ns() / 1_000_000_000) as i64, "%m/%d/%Y");
    writeln!(
        out,
        "{sysname} {release} ({}) \t{date} \t_{machine}_\t({cpus} CPU)\n",
        util::hostname()
    )?;
    // The first report is since boot.
    let (mut then_cpu, mut then_disks, mut then_ns) = (
        CpuTimes::default(),
        names.iter().map(|n| (n.clone(), DiskStat::default())).collect::<Vec<_>>(),
        util::monotonic_ns().saturating_sub(proc::uptime_ns()),
    );
    let mut reports = 0u64;
    loop {
        let now_cpu = proc::stat().map_or_default(|s| s.all);
        let now_disks = sample(&names);
        let now_ns = util::monotonic_ns();
        if !(skip_first && reports == 0 && interval.is_some()) {
            if options.time {
                let now = (util::now_ns() / 1_000_000_000) as i64;
                writeln!(out, "{}", util::local_time(now, "%m/%d/%Y %H:%M:%S"))?;
            }
            if options.cpu {
                cpu_report(&mut out, now_cpu, then_cpu)?;
            }
            if options.devices {
                let rows: Vec<(String, DiskStat, DiskStat)> = now_disks
                    .iter()
                    .zip(&then_disks)
                    .map(|((name, now), (_, then))| (name.clone(), *now, *then))
                    .collect();
                let seconds = now_ns.saturating_sub(then_ns) as f64 / 1e9;
                disk_report(&mut out, &rows, seconds, options)?;
            }
            out.flush()?;
        }
        reports += 1;
        if count.is_some_and(|c| reports >= c + u64::from(skip_first && interval.is_some())) {
            return Ok(0);
        }
        let Some(interval) = interval else { return Ok(0) };
        (then_cpu, then_disks, then_ns) = (now_cpu, now_disks, now_ns);
        thread::sleep(Duration::from_secs(interval));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_linuxs_17_fields() {
        let text = "     120        0     4096       33       10        0       80        5        0       40       38        2        0      512        1        3        0\n";
        let stat = parse_stat(text).unwrap();
        assert_eq!((stat.reads, stat.read_sectors, stat.read_ms), (120, 4096, 33));
        assert_eq!((stat.writes, stat.written_sectors, stat.busy_ms), (10, 80, 40));
        assert_eq!((stat.discards, stat.discarded_sectors, stat.flushes), (2, 512, 3));
        assert_eq!(parse_stat("1 2 3"), None);
    }

    #[test]
    fn reports_rates_over_the_interval() {
        let then = DiskStat::default();
        let now = DiskStat { reads: 20, read_sectors: 4096, busy_ms: 500, ..DiskStat::default() };
        let mut out = Vec::new();
        let options = Options {
            cpu: false,
            devices: true,
            extended: false,
            megabytes: false,
            active_only: true,
            time: false,
        };
        let rows = [("vda".to_string(), now, then), ("vdb".to_string(), then, then)];
        disk_report(&mut out, &rows, 2.0, options).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("Device             tps    kB_read/s    kB_wrtn/s"), "{text}");
        assert!(text.contains("vda              10.00      1024.00         0.00         0.00       2048          0          0"), "{text}");
        assert!(!text.contains("vdb"), "{text}");
    }
}
