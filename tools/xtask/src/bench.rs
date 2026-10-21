// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob bench`: the cost of system calls, IPC round trips, context
//! switches, fresh pages, TLB shootdowns and memory bandwidth on Radix,
//! with Linux as the baseline (docs/design/ipc.md, ADR 0035, ADR 0039).
//!
//! It boots a test image with `selftest=bench` on 2 CPUs, where the
//! benchmark programs of `userspace/usertests` print one `BENCH` line per
//! measurement. Then it measures the same operations with
//! `tools/linux-bench` twice: as the init process of a Linux virtual
//! machine set up exactly like Radix's (same QEMU machine, CPUs, memory
//! and KVM), the baseline that counts, and on the host, for reference.
//! The report goes to `out/bench/` as Markdown and JSON, and CI appends
//! the Markdown to its job summary. With `--gate`, `ob bench` fails if
//! Radix is slower than Linux in the virtual machine at any operation
//! (beyond [`GATE_RATIO`]): the performance gate of the plan.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{Firmware, Media};
use crate::process::{self, step};
use crate::qemu::{self, DirectBoot, Machine, Serial};
use crate::test::{self, Built, TestImage, Vm};
use crate::{Args, Context, Ctx, Result, VERSION, kernel, userspace};

/// The kernel command line of the benchmark boot.
const CMDLINE: &str = "qemu-exit selftest=bench";

/// The Linux virtual machine's command line: the serial console, no
/// restart after a panic, so that QEMU exits, and none of the mitigations
/// of CPU vulnerabilities, which Radix does not have yet either (plan item
/// 3.3): like for like, and the harder baseline to match.
const LINUX_CMDLINE: &str = "console=ttyS0 panic=-1 quiet mitigations=off";

/// What both virtual machines get.
const VM_MEMORY: &str = "1G";
const VM_CPUS: u32 = 2;

/// The most time Radix may take for an operation, as a share of what
/// Linux takes in the same virtual machine, before `--gate` fails: parity,
/// plus the run-to-run noise of a shared CI runner.
pub const GATE_RATIO: f64 = 1.05;

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
    row("cap_info", "Capability method (`info`)", "fcntl_getfd", "fcntl(2) F_GETFD"),
    row("random_words", "64 random bytes", "getrandom_64", "getrandom(2), 64 bytes"),
    row(
        "context_switch",
        "Context switch between two programs yielding, one CPU",
        "context_switch",
        "two processes calling sched_yield(2), per switch",
    ),
    row(
        "fresh_page",
        "Fresh zeroed page: made, mapped, written, given back",
        "fresh_page",
        "mmap(2), first-touch fault, munmap(2)",
    ),
    row(
        "tlb_local",
        "Page mapped and unmapped in a space no other CPU runs",
        "tlb_local",
        "mprotect(2) off and on, no other thread",
    ),
    row(
        "tlb_shootdown",
        "Page unmapped while its space runs on the other CPU",
        "tlb_shootdown",
        "mprotect(2) off and on, a thread on the other CPU",
    ),
    row(
        "memory_copy",
        "Memory copy, per MiB (32 MiB buffers, `rep movsb`)",
        "memory_copy",
        "the same copy",
    ),
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
    row(
        "syscall_null_parallel",
        "System call, on both CPUs at once",
        "getppid_parallel",
        "getppid(2) on two threads",
    ),
    row(
        "cap_info_parallel",
        "Capability method, on both CPUs at once",
        "fcntl_getfd_parallel",
        "fcntl(2) F_GETFD on two threads",
    ),
    row(
        "random_words_parallel",
        "64 random bytes, on both CPUs at once",
        "getrandom_64_parallel",
        "getrandom(2) on two threads",
    ),
];

/// `ob bench`.
pub fn command(ctx: &Ctx, args: &mut Args) -> Result<()> {
    let linux = !args.flag("--no-linux");
    let gate = args.flag("--gate");
    let summary = args.value("--summary")?;
    let given_kernel = args.value("--linux-kernel")?;
    let profile = args.profile(&ctx.config)?;
    args.finish()?;
    if gate && !linux {
        crate::bail!("--gate compares with Linux, so it cannot go with --no-linux");
    }

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
    let (mut radix, counts) = radix_numbers(ctx, &dir, &image, kvm)?;
    per_switch(&mut radix, &counts);
    let mut numbers = Numbers { radix, counts, linux_vm: Vec::new(), linux_host: Vec::new() };
    if linux {
        match linux_kernel(given_kernel)? {
            Some(kernel) if kvm => numbers.linux_vm = linux_vm_numbers(ctx, &dir, &kernel)?,
            Some(_) => eprintln!(
                "    No KVM: the Linux virtual machine would measure the emulator too, so it \
                 does not run."
            ),
            None => eprintln!(
                "    No Linux kernel to boot: the baseline that counts is missing. Give one \
                 with --linux-kernel (on Debian, Ubuntu and Mint: `sudo install -m 644 \
                 /boot/vmlinuz-$(uname -r) .cache/vmlinuz`, then `--linux-kernel \
                 .cache/vmlinuz`)."
            ),
        }
        numbers.linux_host = linux_numbers(ctx)?;
    }

    let report = report(ctx, &numbers, kvm);
    let markdown = dir.join("report.md");
    let json = dir.join("results.json");
    std::fs::write(&markdown, &report).context(|| format!("writing {}", markdown.display()))?;
    std::fs::write(&json, results_json(ctx, &numbers, kvm))
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
    // DIAG (temporary): what each kernel says about the hypervisor, and
    // where a shootdown's time goes.
    for (file, keys) in [
        ("boot-bench.log", &["SHOOTDIAG", "APIC", "CPU:"][..]),
        ("boot-linux.log", &["KVMLOG"][..]),
    ] {
        if let Ok(text) = std::fs::read_to_string(dir.join(file)) {
            for line in text.lines().filter(|line| keys.iter().any(|key| line.contains(key))) {
                eprintln!("    DIAG {file}: {}", line.trim());
            }
        }
    }
    step(&format!("Benchmark report in {} and {}", ctx.display(&markdown), ctx.display(&json)));
    if gate {
        check_gate(&numbers)?;
        step(&format!(
            "Performance gate passed: Radix is within {GATE_RATIO} times Linux in the same \
             virtual machine at every operation"
        ));
    }
    Ok(())
}

/// Turns the time per yield of Radix's `context_switch` into the time per
/// switch, with the kernel's count of the switches while it ran. A yield
/// switches only when the other program may run first (EEVDF, on Radix as
/// on Linux), and `linux-bench` counts Linux's switches the same way, with
/// getrusage(2). The count includes starting and stopping the two
/// programs, a few switches among hundreds of thousands.
fn per_switch(radix: &mut [Measurement], counts: &[Counts]) {
    let switches = counts
        .iter()
        .find(|group| group.group == "context_switch")
        .and_then(|group| group.events.iter().find(|(event, _)| event == "switches"))
        .map_or(0, |(_, count)| *count);
    let Some(yields) = radix.iter_mut().find(|m| m.name == "context_switch") else { return };
    if switches > 0 {
        yields.ns = yields.ns * yields.iterations as f64 / switches as f64;
        yields.iterations = switches;
    }
}

/// Everything one run measured.
struct Numbers {
    radix: Vec<Measurement>,
    counts: Vec<Counts>,
    /// Linux in a virtual machine like Radix's: the baseline that counts.
    linux_vm: Vec<Measurement>,
    /// Linux on the host, outside any virtual machine, for reference.
    linux_host: Vec<Measurement>,
}

/// Radix's time for a row as a share of Linux's in the virtual machine,
/// once both are measured.
fn ratio(numbers: &Numbers, row: &Row) -> Option<f64> {
    let (linux, _) = row.linux?;
    let ours = find(&numbers.radix, row.radix)?.ns;
    let theirs = find(&numbers.linux_vm, linux)?.ns;
    (theirs > 0.0).then(|| ours / theirs)
}

/// Fails unless every row has both numbers and Radix's is within
/// [`GATE_RATIO`] of Linux's.
fn check_gate(numbers: &Numbers) -> Result<()> {
    if numbers.linux_vm.is_empty() {
        crate::bail!(
            "the performance gate needs Linux measured in the same virtual machine, under KVM, \
             and that did not run"
        );
    }
    let mut failed = Vec::new();
    for row in ROWS.iter().filter(|row| row.linux.is_some()) {
        match ratio(numbers, row) {
            Some(ratio) if ratio <= GATE_RATIO => {}
            Some(ratio) => failed.push(format!("{} ({ratio:.2} times Linux)", row.what)),
            None => failed.push(format!("{} (not measured)", row.what)),
        }
    }
    if !failed.is_empty() {
        crate::bail!(
            "the performance gate failed: Radix is slower than Linux in the same virtual machine \
             (beyond {GATE_RATIO} times) at {}",
            failed.join("; ")
        );
    }
    Ok(())
}

/// The Linux kernel to boot: `--linux-kernel`, `OB_LINUX_KERNEL`, or the
/// host's own if this user may read it.
fn linux_kernel(given: Option<String>) -> Result<Option<PathBuf>> {
    let given = given.or_else(|| std::env::var("OB_LINUX_KERNEL").ok());
    if let Some(path) = given {
        let path = PathBuf::from(path);
        if std::fs::File::open(&path).is_err() {
            crate::bail!("cannot read the Linux kernel {}", path.display());
        }
        return Ok(Some(path));
    }
    let own = PathBuf::from(format!("/boot/vmlinuz-{}", host_kernel()));
    Ok(std::fs::File::open(&own).is_ok().then_some(own))
}

/// Boots `kernel` in a virtual machine set up like Radix's, with a static
/// `linux-bench` as its init process, and reads its `BENCH` lines.
fn linux_vm_numbers(ctx: &Ctx, dir: &Path, kernel: &Path) -> Result<Vec<Measurement>> {
    step("Building linux-bench as a static program, to run as Linux's init");
    let target = ctx.target.join("linux-guest");
    let mut cmd = process::cargo(ctx);
    cmd.args(["build", "--quiet", "--release", "-p", "linux-bench"])
        .args(["--target", "x86_64-unknown-linux-gnu"])
        .env("CARGO_TARGET_DIR", &target)
        .env("RUSTFLAGS", "-C target-feature=+crt-static");
    process::run(ctx, &mut cmd)?;
    let program = target.join("x86_64-unknown-linux-gnu/release/linux-bench");
    let init = std::fs::read(&program).context(|| format!("reading {}", program.display()))?;
    let initrd = dir.join("linux-bench.cpio");
    std::fs::write(&initrd, initramfs(&init))
        .context(|| format!("writing {}", initrd.display()))?;

    step(&format!(
        "Measuring the same operations on Linux ({}) in a virtual machine like Radix's",
        ctx.display(kernel)
    ));
    let machine = Machine {
        firmware: Firmware::Bios,
        media: Media::Usb,
        ovmf: None,
        image: PathBuf::new(),
        memory: VM_MEMORY.into(),
        cpus: VM_CPUS,
        kvm: true,
        cpu_model: None,
        serial: Serial::Pipe,
        display: false,
        reboot: false,
        gdb: false,
        qmp: None,
        extra: Vec::new(),
        direct: Some(DirectBoot {
            kernel: kernel.to_path_buf(),
            initrd: initrd.clone(),
            cmdline: LINUX_CMDLINE.into(),
        }),
    };
    let mut vm = Vm::start(ctx, &machine, dir, "linux", TIMEOUT).map_err(crate::Error::new)?;
    let outcome = vm.expect("linux-bench: done");
    let log = vm.log();
    drop(vm);
    if let Err(reason) = outcome {
        show_trouble(&log);
        crate::bail!(
            "the Linux benchmark boot failed: {reason}; serial log: {}",
            ctx.display(&dir.join("boot-linux.log"))
        );
    }
    Ok(parse(&log))
}

/// An initramfs (a cpio archive in the "newc" format Linux reads) that
/// holds `init` and the console device it writes to.
pub fn initramfs(init: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    newc_entry(&mut out, 1, "dev", 0o040_755, (0, 0), &[]);
    newc_entry(&mut out, 2, "dev/console", 0o020_600, (5, 1), &[]);
    newc_entry(&mut out, 3, "init", 0o100_755, (0, 0), init);
    newc_entry(&mut out, 0, "TRAILER!!!", 0, (0, 0), &[]);
    out
}

/// One cpio "newc" entry: a header of hexadecimal fields, the name and the
/// data, each padded to 4 bytes.
fn newc_entry(out: &mut Vec<u8>, ino: u32, name: &str, mode: u32, rdev: (u32, u32), data: &[u8]) {
    let pad = |out: &mut Vec<u8>| out.resize(out.len().next_multiple_of(4), 0);
    let nlink = if mode & 0o040_000 != 0 { 2 } else { 1 };
    // Magic, inode, mode, owner, group, links, time, size, device, the
    // device a special file stands for, the name's size, and a checksum.
    let header = format!(
        "070701{ino:08x}{mode:08x}{:08x}{:08x}{nlink:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}\
         {:08x}{:08x}",
        0,
        0,
        0,
        data.len(),
        0,
        0,
        rdev.0,
        rdev.1,
        name.len() + 1,
        0,
    );
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    pad(out);
    out.extend_from_slice(data);
    pad(out);
}

/// Prints a failed boot's log, from the first sign of trouble or its last
/// lines: CI keeps the log as an artifact, but the job's own output is
/// where people look first.
fn show_trouble(log: &str) {
    let text = test::clean_log(log);
    let lines: Vec<&str> = text.lines().collect();
    let trouble = lines.iter().position(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("panic") || lower.contains("exception") || lower.contains("failed")
    });
    let from = trouble.map_or(lines.len().saturating_sub(40), |i| i.saturating_sub(10));
    for line in lines.iter().skip(from).take(60) {
        eprintln!("    | {line}");
    }
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
        memory: VM_MEMORY.into(),
        cpus: VM_CPUS,
        kvm,
        cpu_model: None,
        serial: Serial::Pipe,
        display: false,
        reboot: false,
        gdb: false,
        qmp: None,
        extra: Vec::new(),
        direct: None,
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
        show_trouble(&log);
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

/// Builds and runs the Linux baseline on the host.
fn linux_numbers(ctx: &Ctx) -> Result<Vec<Measurement>> {
    step("Measuring the same operations on this machine's own Linux, for reference");
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
fn report(ctx: &Ctx, numbers: &Numbers, kvm: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## Radix benchmarks\n");
    let baseline = if numbers.linux_vm.is_empty() {
        "Linux was not measured in a virtual machine this time, so there is no ratio"
    } else {
        "Linux (VM) is Linux in a virtual machine set up the same way, the baseline Radix must \
         match (ratio at most 1.00, lower is better)"
    };
    let _ = writeln!(
        out,
        "Radix {VERSION} at {}, in QEMU {} with {VM_CPUS} CPUs, on {}. {baseline}. Linux (host) \
         is Linux {} on the same machine, outside any virtual machine, for reference. Each \
         number is the average cost of one operation; lower is better.\n",
        commit(ctx),
        if kvm { "under KVM" } else { "without KVM (emulated, so not comparable)" },
        host_cpu(),
        host_kernel(),
    );
    let _ = writeln!(
        out,
        "| Operation | Radix | Linux (VM) | Ratio | Linux (host) | Linux equivalent |"
    );
    let _ = writeln!(out, "| --- | ---: | ---: | ---: | ---: | --- |");
    let measured = |list: &[Measurement], name| find(list, name).map_or("".into(), |m| show(m.ns));
    for row in ROWS {
        let ours = find(&numbers.radix, row.radix).map_or("missing".into(), |m| show(m.ns));
        let (vm, host, how) = match row.linux {
            Some((name, how)) => {
                (measured(&numbers.linux_vm, name), measured(&numbers.linux_host, name), how)
            }
            None => (String::new(), String::new(), "none"),
        };
        let ratio = match ratio(numbers, row) {
            Some(ratio) if ratio > GATE_RATIO => format!("**{ratio:.2}**"),
            Some(ratio) => format!("{ratio:.2}"),
            None => String::new(),
        };
        let _ = writeln!(out, "| {} | {ours} | {vm} | {ratio} | {host} | {how} |", row.what);
    }
    let _ = writeln!(
        out,
        "\nRound trips pass one message there and back between two programs (two threads \
         on Linux), pinned to one CPU or to two. Under a hypervisor, a wake-up on another \
         CPU is an interrupt the host delivers, so two-CPU numbers in a virtual machine \
         carry the host's cost on top; Linux (VM) pays it too. A ratio in bold is over the \
         gate's {GATE_RATIO}. docs/design/ipc.md describes each measurement."
    );
    if !numbers.counts.is_empty() {
        counts_table(&mut out, &numbers.counts);
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
fn results_json(ctx: &Ctx, numbers: &Numbers, kvm: bool) -> String {
    let mut out = String::from("{\n");
    let _ = writeln!(out, "  \"version\": {},", json_string(VERSION));
    let _ = writeln!(out, "  \"commit\": {},", json_string(&commit(ctx)));
    let _ = writeln!(out, "  \"kvm\": {kvm},");
    let _ = writeln!(out, "  \"host_cpu\": {},", json_string(&host_cpu()));
    let _ = writeln!(out, "  \"host_kernel\": {},", json_string(&host_kernel()));
    let _ = writeln!(out, "  \"gate_ratio\": {GATE_RATIO},");
    out.push_str("  \"radix\": ");
    json_list(&mut out, &numbers.radix);
    out.push_str(",\n  \"linux_vm\": ");
    json_list(&mut out, &numbers.linux_vm);
    out.push_str(",\n  \"linux\": ");
    json_list(&mut out, &numbers.linux_host);
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
            assert!(prints(&programs, row.radix), "{}", row.radix);
        }
    }

    #[test]
    fn linux_rows_name_what_linux_bench_prints() {
        let program = [include_str!("../../linux-bench/src/main.rs")];
        for row in ROWS {
            let (name, _) = row.linux.expect("every row has a Linux equivalent");
            assert!(prints(&program, name), "{name}");
        }
    }

    /// Whether one of the programs prints a measurement called `name`:
    /// whole, or as a name its place on the CPUs is added to.
    fn prints(programs: &[&str], name: &str) -> bool {
        let base = name.trim_end_matches("_same_cpu").trim_end_matches("_cross_cpu");
        let forms = [format!("\"{base}\""), format!("name={base} "), format!("{base}_{{place}}")];
        programs.iter().any(|source| forms.iter().any(|form| source.contains(form.as_str())))
    }

    fn measurement(name: &str, ns: f64) -> Measurement {
        Measurement { name: name.into(), ns, iterations: 1 }
    }

    fn numbers(radix_ns: f64, linux_ns: f64) -> Numbers {
        let mut numbers = Numbers {
            radix: Vec::new(),
            counts: Vec::new(),
            linux_vm: Vec::new(),
            linux_host: Vec::new(),
        };
        for row in ROWS {
            numbers.radix.push(measurement(row.radix, radix_ns));
            numbers.linux_vm.push(measurement(row.linux.expect("a Linux row").0, linux_ns));
        }
        numbers
    }

    #[test]
    fn gate_passes_at_parity_and_fails_beyond_it() {
        assert!(check_gate(&numbers(100.0, 100.0)).is_ok());
        assert!(check_gate(&numbers(104.0, 100.0)).is_ok());
        assert!(check_gate(&numbers(120.0, 100.0)).is_err());
        let mut missing = numbers(90.0, 100.0);
        missing.radix.pop();
        assert!(check_gate(&missing).is_err());
        let mut no_vm = numbers(90.0, 100.0);
        no_vm.linux_vm.clear();
        assert!(check_gate(&no_vm).is_err());
    }

    #[test]
    fn initramfs_is_newc() {
        let archive = initramfs(b"\x7fELF");
        assert!(archive.starts_with(b"070701"));
        assert_eq!(archive.len() % 4, 0);
        let text = String::from_utf8_lossy(&archive);
        for name in ["dev\0", "dev/console\0", "init\0", "TRAILER!!!\0"] {
            assert!(text.contains(name), "{name}");
        }
        // Every header starts on a 4-byte boundary: the next magic number
        // follows "init"'s padded data.
        let init = text.find("init\0").expect("init's entry");
        let header = init - 110;
        assert_eq!(header % 4, 0);
        assert_eq!(&text[header + 54..header + 62], "00000004", "init's size");
        assert_eq!(&text[header + 14..header + 22], "000081ed", "init's mode");
    }

    #[test]
    fn context_switches_are_counted_per_switch() {
        let mut radix = vec![measurement("context_switch", 100.0), measurement("cap_info", 50.0)];
        radix[0].iterations = 200;
        let counts = parse_counts("BENCHCOUNT group=context_switch rounds=100 switches=160\n");
        per_switch(&mut radix, &counts);
        assert_eq!(radix[0].iterations, 160);
        assert!((radix[0].ns - 125.0).abs() < 1e-9);
        assert_eq!(radix[1].ns, 50.0);
    }

    #[test]
    fn escapes_json() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
