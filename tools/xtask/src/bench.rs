// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob bench`: the cost of system calls and IPC round trips on Radix, with
//! the host's Linux as a baseline (docs/design/ipc.md, ADR 0035).
//!
//! It boots a test image with `selftest=bench` on 2 CPUs, where the
//! benchmark programs of `userspace/usertests` print one `BENCH` line per
//! measurement, then runs `tools/linux-bench` on the host for the same
//! operations. The report goes to `out/bench/` as Markdown and JSON, and
//! CI appends the Markdown to its job summary on every merge.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::config::{Firmware, Media};
use crate::process::{self, step};
use crate::qemu::{self, Machine, Serial};
use crate::test::{self, Built, TestImage, Vm};
use crate::{Args, Context, Ctx, Result, VERSION, kernel, userspace};

/// The kernel command line of the benchmark boot.
const CMDLINE: &str = "qemu-exit selftest=bench";

/// How long the benchmark boot may take: minutes under KVM, about three
/// without it.
const TIMEOUT: Duration = Duration::from_secs(900);

/// One measurement: a `BENCH name=... ns=... iterations=...` line.
#[derive(Clone, Debug, PartialEq)]
pub struct Measurement {
    pub name: String,
    pub ns: f64,
    pub iterations: u64,
}

/// What the kernel did during one benchmark group: a `BENCHCOUNT
/// group=... rounds=... switches=... ...` line.
#[derive(Clone, Debug, PartialEq)]
pub struct Counts {
    pub group: String,
    pub rounds: u64,
    pub events: Vec<(String, u64)>,
}

/// The events of [`Counts`], in the report's order, with their headings.
const EVENTS: &[(&str, &str)] = &[
    ("switches", "Switches"),
    ("timer_writes", "Timer writes"),
    ("ipis", "IPIs"),
    ("halts", "Halts"),
    ("polls", "Polls"),
];

/// A row of the report: a Radix benchmark and the Linux one closest to it.
struct Row {
    radix: &'static str,
    what: &'static str,
    linux: Option<(&'static str, &'static str)>,
}

const fn row(
    radix: &'static str,
    what: &'static str,
    linux: &'static str,
    how: &'static str,
) -> Row {
    Row { radix, what, linux: Some((linux, how)) }
}

/// The report's rows, in order.
const ROWS: &[Row] = &[
    row("syscall_null", "System call, entry and exit only", "getppid", "getppid(2)"),
    row("syscall_yield", "Yield with nothing else to run", "sched_yield", "sched_yield(2)"),
    Row { radix: "cap_info", what: "Capability method (`info`)", linux: None },
    row("random_words", "64 random bytes", "getrandom_64", "getrandom(2), 64 bytes"),
    row(
        "endpoint_call_same_cpu",
        "Endpoint call and reply, one CPU",
        "pipe_roundtrip_same_cpu",
        "pipe round trip",
    ),
    row(
        "endpoint_call_cross_cpu",
        "Endpoint call and reply, two CPUs",
        "pipe_roundtrip_cross_cpu",
        "pipe round trip",
    ),
    row(
        "notification_pingpong_same_cpu",
        "Notification ping-pong, one CPU",
        "eventfd_roundtrip_same_cpu",
        "eventfd round trip",
    ),
    row(
        "notification_pingpong_cross_cpu",
        "Notification ping-pong, two CPUs",
        "eventfd_roundtrip_cross_cpu",
        "eventfd round trip",
    ),
    row(
        "channel_roundtrip_same_cpu",
        "Shared-memory channel round trip, one CPU",
        "futex_pingpong_same_cpu",
        "futex ping-pong",
    ),
    row(
        "channel_roundtrip_cross_cpu",
        "Shared-memory channel round trip, two CPUs",
        "futex_pingpong_cross_cpu",
        "futex ping-pong",
    ),
    row(
        "ring_roundtrip_same_cpu",
        "Ring round trip (`msg_ring`), one CPU",
        "unix_socket_roundtrip_same_cpu",
        "Unix socket round trip",
    ),
    row(
        "ring_roundtrip_cross_cpu",
        "Ring round trip (`msg_ring`), two CPUs",
        "unix_socket_roundtrip_cross_cpu",
        "Unix socket round trip",
    ),
];

/// `ob bench`.
pub fn command(ctx: &Ctx, args: &mut Args) -> Result<()> {
    let linux = !args.flag("--no-linux");
    let summary = args.value("--summary")?;
    let profile = args.profile(&ctx.config)?;
    args.finish()?;

    qemu::require_qemu()?;
    let built =
        Built { kernel: kernel::build(ctx, profile)?, usertests: userspace::build(ctx, profile)? };
    let dir = ctx.out.join("bench");
    process::create_dir(&dir)?;
    let image = test::build_test_image(ctx, &built, &dir, TestImage::Plain)?;
    let kvm = qemu::kvm_available();
    if !kvm {
        eprintln!(
            "    No KVM: QEMU emulates every instruction, so the Radix numbers measure the \
             emulator, not the kernel."
        );
    }
    step(&format!(
        "Booting Radix with selftest=bench on 2 CPUs ({})",
        if kvm { "KVM" } else { "no KVM, which takes minutes" }
    ));
    let (radix, counts) = radix_numbers(ctx, &dir, &image, kvm)?;
    let linux = if linux { linux_numbers(ctx)? } else { Vec::new() };

    let report = report(ctx, &radix, &counts, &linux, kvm);
    let markdown = dir.join("report.md");
    let json = dir.join("results.json");
    std::fs::write(&markdown, &report).context(|| format!("writing {}", markdown.display()))?;
    std::fs::write(&json, results_json(ctx, &radix, &linux, kvm))
        .context(|| format!("writing {}", json.display()))?;
    if let Some(path) = summary {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context(|| format!("opening {path}"))?;
        file.write_all(report.as_bytes()).context(|| format!("writing {path}"))?;
    }
    eprintln!("\n{report}");
    step(&format!("Benchmark report in {} and {}", ctx.display(&markdown), ctx.display(&json)));
    Ok(())
}

/// Boots the benchmark image and reads its `BENCH` lines.
fn radix_numbers(
    ctx: &Ctx,
    dir: &Path,
    base: &[u8],
    kvm: bool,
) -> Result<(Vec<Measurement>, Vec<Counts>)> {
    let image = dir.join("bench.img");
    let bytes = test::with_cmdline(base, CMDLINE).map_err(crate::Error::new)?;
    std::fs::write(&image, bytes).context(|| format!("writing {}", image.display()))?;
    let machine = Machine {
        firmware: Firmware::Bios,
        media: Media::Usb,
        ovmf: None,
        image: image.clone(),
        memory: "1G".into(),
        cpus: 2,
        kvm,
        cpu_model: None,
        serial: Serial::Pipe,
        display: false,
        reboot: false,
        gdb: false,
        qmp: None,
        extra: Vec::new(),
    };
    let mut vm = Vm::start(ctx, &machine, dir, "bench", TIMEOUT).map_err(crate::Error::new)?;
    let outcome = vm
        .expect("selftest bench: passed")
        .and_then(|()| vm.expect("Boot complete."))
        .and_then(|()| vm.expect_exit(test::QEMU_EXIT_SUCCESS));
    let log = vm.log();
    drop(vm);
    let _ = std::fs::remove_file(&image);
    if let Err(reason) = outcome {
        // CI keeps the log as an artifact, but the job's own output is
        // where people look first.
        let text = test::clean_log(&log);
        for line in text.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev() {
            eprintln!("    | {line}");
        }
        let said: Vec<&str> = log.lines().filter(|line| line.contains("selftest bench")).collect();
        crate::bail!(
            "the benchmark boot failed: {reason}{}{}; serial log: {}",
            if said.is_empty() { "" } else { ": " },
            said.join(" | "),
            ctx.display(&dir.join("boot-bench.log"))
        );
    }
    Ok((parse(&log), parse_counts(&log)))
}

/// Builds and runs the Linux baseline.
fn linux_numbers(ctx: &Ctx) -> Result<Vec<Measurement>> {
    step("Measuring the same operations on this machine's Linux (tools/linux-bench)");
    let mut cmd = process::cargo(ctx);
    cmd.args(["run", "--quiet", "--release", "-p", "linux-bench", "--"]);
    let output = process::output(ctx, &mut cmd)?;
    Ok(parse(&String::from_utf8_lossy(&output.stdout)))
}

/// The measurements in `text`, from lines like
/// `bench[7]: BENCH name=syscall_null ns=120.50 iterations=200000`.
pub fn parse(text: &str) -> Vec<Measurement> {
    text.lines()
        .filter_map(|line| {
            let fields = line.split_once("BENCH ")?.1;
            let (mut name, mut ns, mut iterations) = (None, None, None);
            for field in fields.split_whitespace() {
                match field.split_once('=') {
                    Some(("name", value)) => name = Some(value.to_string()),
                    Some(("ns", value)) => ns = value.parse::<f64>().ok(),
                    Some(("iterations", value)) => iterations = value.parse::<u64>().ok(),
                    _ => {}
                }
            }
            Some(Measurement { name: name?, ns: ns?, iterations: iterations? })
        })
        .collect()
}

/// The kernel's event counts in `text`.
pub fn parse_counts(text: &str) -> Vec<Counts> {
    text.lines()
        .filter_map(|line| {
            let fields = line.split_once("BENCHCOUNT ")?.1;
            let (mut group, mut rounds, mut events) = (None, None, Vec::new());
            for field in fields.split_whitespace() {
                match field.split_once('=')? {
                    ("group", value) => group = Some(value.to_string()),
                    ("rounds", value) => rounds = value.parse::<u64>().ok(),
                    (name, value) => events.push((name.to_string(), value.parse::<u64>().ok()?)),
                }
            }
            Some(Counts { group: group?, rounds: rounds?.max(1), events })
        })
        .collect()
}

fn find<'a>(measurements: &'a [Measurement], name: &str) -> Option<&'a Measurement> {
    measurements.iter().find(|m| m.name == name)
}

/// A duration in nanoseconds, the way the report shows it.
fn show(ns: f64) -> String {
    if ns >= 10_000.0 { format!("{:.1} µs", ns / 1000.0) } else { format!("{ns:.0} ns") }
}

/// The commit measured: CI's, else the checkout's.
fn commit(ctx: &Ctx) -> String {
    if let Ok(sha) = std::env::var("GITHUB_SHA") {
        return sha.chars().take(12).collect();
    }
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(&ctx.root).args(["rev-parse", "--short=12", "HEAD"]);
    match cmd.output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => "an unknown commit".into(),
    }
}

/// The host CPU's name, from /proc/cpuinfo.
fn host_cpu() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find_map(|line| line.strip_prefix("model name")?.split_once(':'))
                .map(|(_, name)| name.trim().to_string())
        })
        .unwrap_or_else(|| "an unknown CPU".into())
}

fn host_kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|release| release.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// The Markdown report.
fn report(
    ctx: &Ctx,
    radix: &[Measurement],
    counts: &[Counts],
    linux: &[Measurement],
    kvm: bool,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## Radix benchmarks\n");
    let _ = writeln!(
        out,
        "Radix {VERSION} at {}, in QEMU {} with 2 CPUs, on {}. Linux {} on the same machine, \
         outside the virtual machine. Each number is the average cost of one operation; lower \
         is better.\n",
        commit(ctx),
        if kvm { "under KVM" } else { "without KVM (emulated, so not comparable)" },
        host_cpu(),
        host_kernel(),
    );
    let _ = writeln!(out, "| Operation | Radix | Linux | Linux equivalent |");
    let _ = writeln!(out, "| --- | ---: | ---: | --- |");
    for row in ROWS {
        let ours = find(radix, row.radix).map_or("missing".into(), |m| show(m.ns));
        let (theirs, how) = match row.linux {
            Some((name, how)) => {
                (find(linux, name).map_or("not measured".into(), |m| show(m.ns)), how)
            }
            None => ("".into(), "none"),
        };
        let _ = writeln!(out, "| {} | {ours} | {theirs} | {how} |", row.what);
    }
    let _ = writeln!(
        out,
        "\nRound trips pass one message there and back between two programs (two threads \
         on Linux), pinned to one CPU or to two. Under a hypervisor, a wake-up on another \
         CPU is an interrupt the host delivers, so two-CPU numbers in the virtual machine \
         carry the host's cost on top. docs/design/ipc.md describes each measurement."
    );
    if !counts.is_empty() {
        counts_table(&mut out, counts);
    }
    out
}

/// What the kernel did per operation: under a hypervisor each timer write,
/// interrupt sent and halt is an exit to the host, so these explain most
/// of the numbers above.
fn counts_table(out: &mut String, counts: &[Counts]) {
    let _ = writeln!(out, "\n### Kernel events per operation\n");
    let _ = write!(out, "| Benchmark |");
    for (_, heading) in EVENTS {
        let _ = write!(out, " {heading} |");
    }
    let _ = write!(out, "\n| --- |");
    for _ in EVENTS {
        let _ = write!(out, " ---: |");
    }
    out.push('\n');
    for group in counts {
        let _ = write!(out, "| {} |", group.group.replace('_', " "));
        for (name, _) in EVENTS {
            let value = group.events.iter().find(|(event, _)| event == name).map(|(_, v)| *v);
            match value {
                Some(value) => {
                    let _ = write!(out, " {:.2} |", value as f64 / group.rounds as f64);
                }
                None => out.push_str(" |"),
            }
        }
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "\nCounted over all CPUs while each benchmark ran, including starting and stopping its \
         programs, and divided by its rounds. A round trip on one CPU needs two switches; \
         anything else is overhead."
    );
}

fn json_list(out: &mut String, measurements: &[Measurement]) {
    out.push('[');
    for (i, m) in measurements.iter().enumerate() {
        let comma = if i == 0 { "" } else { "," };
        // Names are [a-z0-9_] by construction, so need no escaping.
        let _ = write!(
            out,
            "{comma}\n    {{\"name\": \"{}\", \"ns\": {:.2}, \"iterations\": {}}}",
            m.name, m.ns, m.iterations
        );
    }
    out.push_str("\n  ]");
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The results as JSON, for tools that track them over time.
fn results_json(ctx: &Ctx, radix: &[Measurement], linux: &[Measurement], kvm: bool) -> String {
    let mut out = String::from("{\n");
    let _ = writeln!(out, "  \"version\": {},", json_string(VERSION));
    let _ = writeln!(out, "  \"commit\": {},", json_string(&commit(ctx)));
    let _ = writeln!(out, "  \"kvm\": {kvm},");
    let _ = writeln!(out, "  \"host_cpu\": {},", json_string(&host_cpu()));
    let _ = writeln!(out, "  \"host_kernel\": {},", json_string(&host_kernel()));
    out.push_str("  \"radix\": ");
    json_list(&mut out, radix);
    out.push_str(",\n  \"linux\": ");
    json_list(&mut out, linux);
    out.push_str("\n}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bench_lines() {
        let log = "[    0.6] bench[7]: BENCH name=syscall_null ns=965.01 iterations=200000\n\
                   noise\n\
                   BENCH name=getppid ns=98.00 iterations=40000\n\
                   BENCH name=broken ns=x iterations=1\n";
        assert_eq!(
            parse(log),
            [
                Measurement { name: "syscall_null".into(), ns: 965.01, iterations: 200_000 },
                Measurement { name: "getppid".into(), ns: 98.0, iterations: 40_000 },
            ]
        );
    }

    #[test]
    fn reads_count_lines() {
        let log = "[ 1.2] BENCHCOUNT group=endpoint_same_cpu rounds=100 switches=200 ipis=0\n\
                   BENCHCOUNT group=broken rounds=1 switches=x\n";
        assert_eq!(
            parse_counts(log),
            [Counts {
                group: "endpoint_same_cpu".into(),
                rounds: 100,
                events: vec![("switches".into(), 200), ("ipis".into(), 0)],
            }]
        );
    }

    #[test]
    fn rows_are_unique() {
        for (i, row) in ROWS.iter().enumerate() {
            assert!(ROWS[i + 1..].iter().all(|other| other.radix != row.radix), "{}", row.radix);
        }
    }

    #[test]
    fn rows_name_what_the_programs_print() {
        let programs = [
            include_str!("../../../userspace/usertests/src/bench.rs"),
            include_str!("../../../userspace/usertests/src/pairs.rs"),
        ];
        for row in ROWS {
            let base = row.radix.trim_end_matches("_same_cpu").trim_end_matches("_cross_cpu");
            let quoted = format!("\"{base}\"");
            assert!(programs.iter().any(|source| source.contains(&quoted)), "{}", row.radix);
        }
    }

    #[test]
    fn escapes_json() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
