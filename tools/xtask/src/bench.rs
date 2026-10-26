// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob bench`: the cost of system calls, IPC round trips, context
//! switches, fresh pages, TLB shootdowns, memory bandwidth, files and
//! POSIX processes, pipes, signals, futexes and raw disk transfers on
//! Radix, with Linux as the baseline (docs/design/ipc.md, ADR 0035, ADR
//! 0039, ADR 0106, ADR 0111).
//!
//! It boots a test image with `selftest=bench` on 2 CPUs, where the
//! benchmark programs of `userspace/usertests` print one `BENCH` line per
//! measurement, and again with Spark running the POSIX benchmarks, a C
//! program on the C library (`userspace/posix/bench.c`). Then it measures
//! the same operations with `tools/linux-bench`, which runs the same C
//! program built for Linux last, twice: as the init process of a Linux
//! virtual machine set up exactly like Radix's (same QEMU machine, CPUs,
//! memory and KVM), the baseline that counts, and on the host, for
//! reference.
//! Under KVM it measures Radix and the Linux virtual machine in turn
//! [`RUNS`] times, and each operation counts its fastest run on each side
//! (ADR 0046). The report goes to `out/bench/` as Markdown and JSON, and
//! CI appends the Markdown to its job summary. With `--gate`, `ob bench`
//! fails if Radix is slower than Linux in the virtual machine at any
//! operation ([`GATE_RATIO`], parity): the performance gate of the plan.
//! While an operation is over it, both systems are measured again, up to
//! [`MORE_RUNS`] more times (ADR 0107).
//!
//! With `--instructions` it boots Radix under QEMU's instruction counting
//! instead, where every instruction takes one nanosecond of the guest's
//! time, so each `BENCH` line gives the instructions an operation took:
//! the same on every machine, unlike the timings. With `--gate` it fails
//! if an operation on one CPU takes more than its [`BUDGETS`].

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

/// The kernel command line of the POSIX benchmark boot: Spark runs the C
/// program of [`POSIX_SOURCE`] from the boot archive, and nothing else,
/// with the benchmarks' disk ([`disk_args`]).
const POSIX_CMDLINE: &str = "-- --posix /system/tests/posix/bench --disk /devices/vda";

/// The size of the disk both systems' POSIX benchmarks read and write: the
/// 32 MiB they go through, with room.
const DISK_BYTES: usize = 64 << 20;

/// The POSIX benchmarks: one C program, which Radix's build makes for
/// Radix (`userspace::build`, with the other C test programs) and
/// [`posix_for_linux`] for Linux.
const POSIX_SOURCE: &str = "userspace/posix/bench.c";

/// The Linux virtual machine's command line: the serial console, no
/// restart after a panic, so that QEMU exits, and none of the mitigations
/// of CPU vulnerabilities, which Radix does not have yet either (plan item
/// 3.3): like for like, and the harder baseline to match.
const LINUX_CMDLINE: &str = "console=ttyS0 panic=-1 quiet mitigations=off";

/// What both virtual machines get.
const VM_MEMORY: &str = "1G";
const VM_CPUS: u32 = 2;

/// The most time Radix may take for an operation, as a share of what
/// Linux takes in the same virtual machine, before `--gate` fails: parity
/// (ADR 0107). The noise of a shared CI runner is met with more runs
/// ([`MORE_RUNS`]), never with room above parity.
pub const GATE_RATIO: f64 = 1.00;

/// How many times `ob bench` measures Radix and Linux in turn under KVM,
/// unless `--runs` says otherwise. Each operation counts its fastest run
/// on each side: what the host adds (another job on the machine, two
/// virtual CPUs placed on one core, a virtual CPU waking up late) only
/// ever makes a run slower, and rarely hits the same operation every
/// time, while what the kernel costs is there in every run.
const RUNS: u32 = 3;

/// How many more times, at most, `ob bench --gate` measures Radix and
/// Linux in turn while an operation is over the gate after the first
/// runs. Each run can only lower each side's fastest time: a run the host
/// slowed is set aside on whichever side it fell, while what the kernel
/// costs is in every run, so a real difference stays (ADR 0107).
pub const MORE_RUNS: u32 = 6;

/// How long the benchmark boot may take: minutes under KVM, about three
/// without it.
const TIMEOUT: Duration = Duration::from_mins(15);

/// QEMU's instruction counting: each instruction advances the guest's
/// clock by 2^0 ns, and the clock does not run ahead to catch up with the
/// host's.
const ICOUNT: &str = "shift=0,align=off";

/// The most instructions, user and kernel, that an operation on one CPU
/// may take under `--instructions` before `--gate` fails: what it took
/// when the budget was set, plus a tenth, rounded up to two significant
/// figures (the count it was set from is in the comment). Operations on
/// two CPUs depend on how QEMU interleaves the CPUs, and its clock runs on
/// while one waits, so they have no budget; nor do the disk transfers,
/// which wait for the disk. A change that makes an
/// operation longer raises its budget in the same pull request, and says
/// why.
pub const BUDGETS: &[(&str, u64)] = &[
    ("syscall_null", 190),                     // 165
    ("syscall_yield", 670),                    // 603
    ("cap_info", 470),                         // 425
    ("random_words", 2_500),                   // 2,221
    ("context_switch", 1_100),                 // 959
    ("fresh_page", 9_900),                     // 8,986
    ("tlb_local", 3_500),                      // 3,106
    ("file_open_close", 4_100),                // 3,693
    ("file_stat", 1_600),                      // 1,430
    ("file_read_4k", 5_400),                   // 4,896
    ("file_write_4k", 5_600),                  // 5,061
    ("endpoint_call_same_cpu", 6_200),         // 5,549
    ("notification_pingpong_same_cpu", 6_200), // 5,566
    ("channel_roundtrip_same_cpu", 5_100),     // 4,587
    ("ring_roundtrip_same_cpu", 9_400),        // 8,489
    ("posix_fork_wait", 160_000),              // 138,300
    ("posix_fork_exec_wait", 590_000),         // 531,676
    ("posix_pipe_mib", 2_500_000),             // 2,188,725
    ("posix_signal", 5_900),                   // 5,300
    ("posix_poll", 1_300),                     // 1,092
    ("posix_futex_same_cpu", 5_300),           // 4,787
    ("posix_futex_shared_same_cpu", 5_800),    // 5,248
];

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
    ("shootdown_wait_ns", "Waiting for TLB flushes"),
    ("shootdowns_left", "Flushes left to the host"),
    ("idle_here_ns", "Idle on its CPU"),
    ("busy_elsewhere_ns", "Busy elsewhere"),
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

/// A row of the POSIX benchmarks, which have the same name on both.
const fn posix(name: &'static str, what: &'static str) -> Row {
    Row { radix: name, what, linux: Some((name, "the same C program, on glibc")) }
}

/// A row of the POSIX benchmarks' disk transfers: on Linux, of the same
/// disk opened with `O_DIRECT`.
const fn disk(name: &'static str, what: &'static str) -> Row {
    Row { radix: name, what, linux: Some((name, "the same C program, on glibc, `O_DIRECT`")) }
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
        "file_open_close",
        "File opened by name beneath a directory, then closed (tmpfs)",
        "file_open_close",
        "openat(2) and close(2), tmpfs",
    ),
    row("file_stat", "File status by name (tmpfs)", "file_stat", "fstatat(2), tmpfs"),
    row(
        "file_read_4k",
        "4 KiB read from a file's page in memory (tmpfs)",
        "file_read_4k",
        "pread(2), 4 KiB, tmpfs",
    ),
    row(
        "file_write_4k",
        "4 KiB written over a file's page in memory (tmpfs)",
        "file_write_4k",
        "pwrite(2), 4 KiB, tmpfs",
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
    row(
        "endpoint_call_parallel",
        "Endpoint call and reply, a pair on each CPU at once",
        "pipe_roundtrip_parallel",
        "pipe round trip, a pair of threads on each CPU",
    ),
    posix("posix_fork_wait", "POSIX: process made and waited for (`fork`, `_exit`, `waitpid`)"),
    posix("posix_fork_exec_wait", "POSIX: program run and waited for (`fork`, `execv`, `waitpid`)"),
    posix(
        "posix_pipe_mib",
        "POSIX: pipe throughput, per MiB (64 KiB writes, two processes, one CPU)",
    ),
    posix("posix_signal", "POSIX: signal sent to itself and handled (`kill`, handler, return)"),
    posix("posix_poll", "POSIX: `poll` on a pipe with data to read"),
    posix(
        "posix_futex_same_cpu",
        "POSIX: futex wake and wait each way (semaphores), two threads, one CPU",
    ),
    posix(
        "posix_futex_cross_cpu",
        "POSIX: futex wake and wait each way (semaphores), two threads, two CPUs",
    ),
    posix(
        "posix_futex_shared_same_cpu",
        "POSIX: futex wake and wait each way (semaphores in shared memory), two processes, one CPU",
    ),
    posix(
        "posix_futex_shared_cross_cpu",
        "POSIX: futex wake and wait each way (semaphores in shared memory), two processes, two CPUs",
    ),
    disk("posix_disk_read_4k", "Disk: 4 KiB read raw from a virtio disk (`pread`)"),
    disk("posix_disk_write_4k", "Disk: 4 KiB written raw to a virtio disk (`pwrite`)"),
    disk("posix_disk_read_64k", "Disk: 64 KiB read raw from a virtio disk"),
    disk("posix_disk_write_64k", "Disk: 64 KiB written raw to a virtio disk"),
    disk("posix_disk_read_mib", "Disk: 1 MiB read raw from a virtio disk"),
    disk("posix_disk_write_mib", "Disk: 1 MiB written raw to a virtio disk"),
];

/// `ob bench`.
pub fn command(ctx: &Ctx, args: &mut Args) -> Result<()> {
    let linux = !args.flag("--no-linux");
    let instructions = args.flag("--instructions");
    let gate = args.flag("--gate");
    let runs = match args.value("--runs")? {
        Some(value) => value.parse::<u32>().ok().filter(|&runs| runs > 0).ok_or_else(|| {
            crate::Error::new(format!("--runs takes a number of runs, not `{value}`"))
        })?,
        None if instructions => 1,
        None => RUNS,
    };
    let summary = args.value("--summary")?;
    let given_kernel = args.value("--linux-kernel")?;
    let profile = args.profile(&ctx.config)?;
    args.finish()?;
    if gate && !linux && !instructions {
        crate::bail!("--gate compares with Linux, so it cannot go with --no-linux");
    }
    if instructions && runs > 1 {
        crate::bail!("--instructions counts the same every run, so it takes no --runs");
    }

    qemu::require_qemu()?;
    let built =
        Built { kernel: kernel::build(ctx, profile)?, programs: userspace::build(ctx, profile)? };
    let dir = ctx.out.join("bench");
    process::create_dir(&dir)?;
    let image = test::build_test_image(ctx, &built, &dir, TestImage::Plain)?;
    if instructions {
        return count_instructions(ctx, &dir, &image, gate, summary.as_deref());
    }
    let kvm = qemu::kvm_available();
    if !kvm {
        eprintln!(
            "    No KVM: QEMU emulates every instruction, so the Radix numbers measure the \
             emulator, not the kernel. One run only."
        );
    }
    let runs = if kvm { runs } else { 1 };
    let posix_linux = if linux { Some(posix_for_linux(ctx, &dir)?) } else { None };
    let vm_kernel = if let Some(posix) = &posix_linux {
        match linux_kernel(given_kernel)? {
            Some(kernel) if kvm => Some((kernel.clone(), linux_init(ctx, &dir, posix)?)),
            Some(_) => {
                eprintln!(
                    "    No KVM: the Linux virtual machine would measure the emulator too, so \
                     it does not run."
                );
                None
            }
            None => {
                eprintln!(
                    "    No Linux kernel to boot: the baseline that counts is missing. Give one \
                     with --linux-kernel (on Debian, Ubuntu and Mint: `sudo install -m 644 \
                     /boot/vmlinuz-$(uname -r) .cache/vmlinuz`, then `--linux-kernel \
                     .cache/vmlinuz`)."
                );
                None
            }
        }
    } else {
        None
    };
    let mut numbers = Numbers {
        runs: 0,
        more: 0,
        radix: Vec::new(),
        counts: Vec::new(),
        linux_vm: Vec::new(),
        linux_host: Vec::new(),
    };
    // One run: Radix, then Linux in the virtual machine; each operation
    // keeps its fastest so far on each side.
    let measure = |numbers: &mut Numbers, of: &str| -> Result<()> {
        step(&format!(
            "Booting Radix with selftest=bench on 2 CPUs ({}{of})",
            if kvm { "KVM" } else { "no KVM, which takes minutes" }
        ));
        let (mut radix, counts) = radix_numbers(ctx, &dir, &image, Accel::of(kvm))?;
        per_switch(&mut radix, &counts);
        keep_best(&mut numbers.radix, radix);
        step(&format!("Booting Radix with the POSIX benchmarks on 2 CPUs{of}"));
        let (posix, posix_counts) = radix_posix_numbers(ctx, &dir, &image, Accel::of(kvm))?;
        keep_best(&mut numbers.radix, posix);
        if numbers.counts.is_empty() {
            numbers.counts = counts;
            numbers.counts.extend(posix_counts);
        }
        if let Some((kernel, initrd)) = &vm_kernel {
            step(&format!(
                "Measuring the same operations on Linux ({}) in a virtual machine like \
                 Radix's{of}",
                ctx.display(kernel)
            ));
            let (linux, linux_counts) = linux_vm_numbers(ctx, &dir, kernel, initrd)?;
            keep_best(&mut numbers.linux_vm, linux);
            if numbers.runs == 0 {
                numbers.counts.extend(linux_counts);
            }
        }
        numbers.runs += 1;
        Ok(())
    };
    for run in 1..=runs {
        let of = if runs > 1 { format!(", run {run} of {runs}") } else { String::new() };
        measure(&mut numbers, &of)?;
    }
    if gate && kvm && !numbers.linux_vm.is_empty() {
        while numbers.more < MORE_RUNS {
            let over = over_gate(&numbers);
            if over.is_empty() {
                break;
            }
            numbers.more += 1;
            step(&format!(
                "Over the gate after {} runs: {}. Measuring both again",
                numbers.runs,
                over.join("; ")
            ));
            let of = format!(", run {} of at most {}", numbers.runs + 1, runs + MORE_RUNS);
            measure(&mut numbers, &of)?;
        }
    }
    if let Some(posix) = &posix_linux {
        numbers.linux_host = linux_numbers(ctx, posix)?;
    }

    let report = report(ctx, &numbers, kvm);
    let markdown = dir.join("report.md");
    let json = dir.join("results.json");
    std::fs::write(&markdown, &report).context(|| format!("writing {}", markdown.display()))?;
    std::fs::write(&json, results_json(ctx, &numbers, kvm))
        .context(|| format!("writing {}", json.display()))?;
    if let Some(path) = summary {
        append(&path, &report)?;
    }
    eprintln!("\n{report}");
    step(&format!("Benchmark report in {} and {}", ctx.display(&markdown), ctx.display(&json)));
    if gate {
        check_gate(&numbers)?;
        step(
            "Performance gate passed: Radix is as fast as Linux in the same virtual machine, \
             or faster, at every operation",
        );
    }
    Ok(())
}

/// Adds the measurements of one run to `best`, keeping the fastest of
/// each operation.
fn keep_best(best: &mut Vec<Measurement>, run: Vec<Measurement>) {
    for measured in run {
        match best.iter_mut().find(|kept| kept.name == measured.name) {
            Some(kept) if measured.ns < kept.ns => *kept = measured,
            Some(_) => {}
            None => best.push(measured),
        }
    }
}

/// `ob bench --instructions`: boots Radix counting instructions, reports
/// what each operation took against its budget, and with `gate` fails if
/// one went over.
fn count_instructions(
    ctx: &Ctx,
    dir: &Path,
    image: &[u8],
    gate: bool,
    summary: Option<&str>,
) -> Result<()> {
    step("Booting Radix with selftest=bench on 2 CPUs, counting instructions (minutes)");
    let (mut radix, counts) = radix_numbers(ctx, dir, image, Accel::Count)?;
    per_switch(&mut radix, &counts);
    step("Booting Radix with the POSIX benchmarks on 2 CPUs, counting instructions (minutes)");
    radix.extend(radix_posix_numbers(ctx, dir, image, Accel::Count)?.0);
    let report = budget_report(&radix);
    let markdown = dir.join("instructions.md");
    std::fs::write(&markdown, &report).context(|| format!("writing {}", markdown.display()))?;
    if let Some(path) = summary {
        append(path, &report)?;
    }
    eprintln!("\n{report}");
    step(&format!("Instruction report in {}", ctx.display(&markdown)));
    if gate {
        check_budgets(&radix)?;
        step("Instruction budgets met: no operation on one CPU takes more than its budget");
    }
    Ok(())
}

/// The instructions each operation took, against its budget, in Markdown.
fn budget_report(radix: &[Measurement]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## Radix instructions per operation\n");
    let _ = writeln!(
        out,
        "Counted by QEMU (`-icount {ICOUNT}`): user and kernel instructions, the same on any \
         machine. Operations on one CPU have a budget; `--gate` fails beyond it.\n"
    );
    let _ = writeln!(out, "| Operation | Instructions | Budget | Of the budget |");
    let _ = writeln!(out, "| --- | ---: | ---: | ---: |");
    for row in ROWS {
        let Some(measured) = find(radix, row.radix) else { continue };
        let count = measured.ns.round() as u64;
        match budget(row.radix) {
            Some(budget) => {
                let share = count as f64 / budget as f64;
                let shown = if share > 1.0 {
                    format!("**{:.0}%**", share * 100.0)
                } else {
                    format!("{:.0}%", share * 100.0)
                };
                let _ = writeln!(out, "| {} | {count} | {budget} | {shown} |", row.what);
            }
            None => {
                let _ = writeln!(out, "| {} | {count} | | |", row.what);
            }
        }
    }
    out
}

/// The budget of the operation `name`, if it has one.
fn budget(name: &str) -> Option<u64> {
    BUDGETS.iter().find(|(row, _)| *row == name).map(|&(_, budget)| budget)
}

/// Fails unless every operation with a budget was counted and kept to it.
fn check_budgets(radix: &[Measurement]) -> Result<()> {
    let mut failed = Vec::new();
    for &(name, budget) in BUDGETS {
        match find(radix, name).map(|m| m.ns.round() as u64) {
            Some(count) if count <= budget => {}
            Some(count) => failed.push(format!("{name} ({count} instructions, budget {budget})")),
            None => failed.push(format!("{name} (not measured)")),
        }
    }
    if !failed.is_empty() {
        crate::bail!("instruction budgets exceeded: {}", failed.join("; "));
    }
    Ok(())
}

/// Appends `text` to the file at `path`.
fn append(path: &str, text: &str) -> Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context(|| format!("opening {path}"))?;
    file.write_all(text.as_bytes()).context(|| format!("writing {path}"))
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

/// Everything `ob bench` measured: each operation's fastest run.
struct Numbers {
    /// How many times Radix and the Linux virtual machine were measured.
    runs: u32,
    /// How many of those runs were more, for operations over the gate.
    more: u32,
    radix: Vec<Measurement>,
    /// What the kernel did, in the first run, and Linux's TLB interrupts.
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

/// The rows over the gate, each with its ratio (or that it is missing).
fn over_gate(numbers: &Numbers) -> Vec<String> {
    let mut over = Vec::new();
    for row in ROWS.iter().filter(|row| row.linux.is_some()) {
        match ratio(numbers, row) {
            Some(ratio) if ratio <= GATE_RATIO => {}
            Some(ratio) => over.push(format!("{} ({} times Linux)", row.what, shown(ratio))),
            None => over.push(format!("{} (not measured)", row.what)),
        }
    }
    over
}

/// A ratio to two decimals, or to three where two would hide that it is
/// over the gate (1.004 is not 1.00).
fn shown(ratio: f64) -> String {
    let two = format!("{ratio:.2}");
    if ratio > GATE_RATIO && two.parse::<f64>().is_ok_and(|r| r <= GATE_RATIO) {
        format!("{ratio:.3}")
    } else {
        two
    }
}

/// Fails unless every row has both numbers and Radix's is at most
/// [`GATE_RATIO`] times Linux's.
fn check_gate(numbers: &Numbers) -> Result<()> {
    if numbers.linux_vm.is_empty() {
        crate::bail!(
            "the performance gate needs Linux measured in the same virtual machine, under KVM, \
             and that did not run"
        );
    }
    let over = over_gate(numbers);
    if !over.is_empty() {
        crate::bail!(
            "the performance gate failed: Radix is slower than Linux in the same virtual machine \
             after {} runs at {}",
            numbers.runs,
            over.join("; ")
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

/// Builds the POSIX benchmarks for Linux, static, with this machine's C
/// library and the Clang that builds them for Radix (or else its C
/// compiler), at the same optimisation, and returns the program.
fn posix_for_linux(ctx: &Ctx, dir: &Path) -> Result<PathBuf> {
    step("Building the POSIX benchmarks for Linux as a static program");
    let compiler = crate::ports::find_clang()
        .map(|(clang, _)| clang)
        .or_else(|| process::find_program("cc"))
        .ok_or_else(|| {
            crate::Error::new("no C compiler is installed, for the POSIX benchmarks on Linux")
                .hint("run `./ob doctor`")
        })?;
    let program = dir.join("posix-bench");
    let mut cmd = std::process::Command::new(compiler);
    cmd.args(["-std=c11", "-D_GNU_SOURCE", "-O2", "-Wall", "-static", "-pthread", "-o"])
        .arg(&program)
        .arg(ctx.root.join(POSIX_SOURCE));
    process::run(ctx, &mut cmd)?;
    Ok(program)
}

/// Builds `linux-bench` as a static program and writes the initramfs
/// that makes it the Linux virtual machine's init process, with the
/// POSIX benchmarks at `posix` beside it.
fn linux_init(ctx: &Ctx, dir: &Path, posix: &Path) -> Result<PathBuf> {
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
    let posix = std::fs::read(posix).context(|| format!("reading {}", posix.display()))?;
    let initrd = dir.join("linux-bench.cpio");
    std::fs::write(&initrd, initramfs(&init, &posix))
        .context(|| format!("writing {}", initrd.display()))?;
    Ok(initrd)
}

/// Boots `kernel` in a virtual machine set up like Radix's, with the
/// `initrd` of [`linux_init`], and reads the `BENCH` and `BENCHCOUNT`
/// lines of `linux-bench`.
fn linux_vm_numbers(
    ctx: &Ctx,
    dir: &Path,
    kernel: &Path,
    initrd: &Path,
) -> Result<(Vec<Measurement>, Vec<Counts>)> {
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
        extra: disk_args(dir)?,
        direct: Some(DirectBoot {
            kernel: kernel.to_path_buf(),
            initrd: initrd.to_path_buf(),
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
    Ok((parse(&log), parse_counts(&log)))
}

/// An initramfs (a cpio archive in the "newc" format Linux reads) that
/// holds `init`, the console device it writes to, and the POSIX
/// benchmarks it runs, `posix`, at `/posix-bench`.
pub fn initramfs(init: &[u8], posix: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    newc_entry(&mut out, 1, "dev", 0o040_755, (0, 0), &[]);
    newc_entry(&mut out, 2, "dev/console", 0o020_600, (5, 1), &[]);
    newc_entry(&mut out, 3, "init", 0o100_755, (0, 0), init);
    newc_entry(&mut out, 4, "posix-bench", 0o100_755, (0, 0), posix);
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

/// How QEMU runs Radix for the benchmarks.
#[derive(Clone, Copy, PartialEq)]
enum Accel {
    /// Under KVM: the timings that count.
    Kvm,
    /// Emulated: timings of the emulator.
    Tcg,
    /// Emulated, counting instructions (`--instructions`).
    Count,
}

impl Accel {
    fn of(kvm: bool) -> Accel {
        if kvm { Accel::Kvm } else { Accel::Tcg }
    }

    /// What QEMU is given for it, beyond the machine.
    fn extra(self) -> Vec<String> {
        match self {
            Accel::Count => vec!["-icount".into(), ICOUNT.into()],
            Accel::Kvm | Accel::Tcg => Vec::new(),
        }
    }
}

/// Boots the benchmark image and reads its `BENCH` and `BENCHCOUNT`
/// lines.
fn radix_numbers(
    ctx: &Ctx,
    dir: &Path,
    base: &[u8],
    accel: Accel,
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
        kvm: accel == Accel::Kvm,
        cpu_model: None,
        serial: Serial::Pipe,
        display: false,
        reboot: false,
        gdb: false,
        qmp: None,
        extra: accel.extra(),
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

/// Boots the test image `base` with Spark running the POSIX benchmarks
/// and reads their `BENCH` and `BENCHCOUNT` lines, which Spark copies from
/// the program's terminal to the kernel log.
fn radix_posix_numbers(
    ctx: &Ctx,
    dir: &Path,
    base: &[u8],
    accel: Accel,
) -> Result<(Vec<Measurement>, Vec<Counts>)> {
    let image = dir.join("posix-bench.img");
    let bytes = test::with_cmdline(base, POSIX_CMDLINE).map_err(crate::Error::new)?;
    std::fs::write(&image, bytes).context(|| format!("writing {}", image.display()))?;
    let mut extra = accel.extra();
    extra.extend(disk_args(dir)?);
    let machine = Machine {
        firmware: Firmware::Bios,
        media: Media::Usb,
        ovmf: None,
        image: image.clone(),
        memory: VM_MEMORY.into(),
        cpus: VM_CPUS,
        kvm: accel == Accel::Kvm,
        cpu_model: None,
        serial: Serial::Pipe,
        display: false,
        reboot: false,
        gdb: false,
        qmp: None,
        extra,
        direct: None,
    };
    let mut vm =
        Vm::start(ctx, &machine, dir, "posix-bench", TIMEOUT).map_err(crate::Error::new)?;
    let outcome = vm.expect("posix-bench: done");
    let log = vm.log();
    drop(vm);
    let _ = std::fs::remove_file(&image);
    if let Err(reason) = outcome {
        show_trouble(&log);
        crate::bail!(
            "the POSIX benchmark boot failed: {reason}; serial log: {}",
            ctx.display(&dir.join("boot-posix-bench.log"))
        );
    }
    Ok((parse(&log), parse_counts(&log)))
}

/// Makes the benchmarks' disk afresh in `dir`, filled with bytes rather
/// than holes, which the host would read faster, and returns QEMU's
/// arguments for it: a virtio block device, `vda` on both systems
/// (`/devices/vda` on Radix, `/dev/vda` on Linux).
/// Each boot gets the same disk.
fn disk_args(dir: &Path) -> Result<Vec<String>> {
    let disk = dir.join("bench-disk.img");
    std::fs::write(&disk, vec![0x5a; DISK_BYTES])
        .context(|| format!("writing {}", disk.display()))?;
    Ok(vec![
        "-drive".into(),
        format!("file={},if=none,id=bench,format=raw", qemu::option_value(&disk)),
        "-device".into(),
        "virtio-blk-pci,drive=bench".into(),
    ])
}

/// Builds and runs the Linux baseline on the host, with the POSIX
/// benchmarks at `posix`.
fn linux_numbers(ctx: &Ctx, posix: &Path) -> Result<Vec<Measurement>> {
    step("Measuring the same operations on this machine's own Linux, for reference");
    let mut cmd = process::cargo(ctx);
    cmd.args(["run", "--quiet", "--release", "-p", "linux-bench", "--", "--posix-bench"])
        .arg(posix);
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
                    // A time is a finite number, never below 0: `nan`
                    // and `inf` parse as f64 but no JSON reader takes them.
                    Some(("ns", value)) => {
                        ns = value.parse::<f64>().ok().filter(|ns| ns.is_finite() && *ns >= 0.0);
                    }
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
        .map_or_else(|_| "unknown".into(), |release| release.trim().to_string())
}

/// The Markdown report.
fn report(ctx: &Ctx, numbers: &Numbers, kvm: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## Radix benchmarks\n");
    let baseline = if numbers.linux_vm.is_empty() {
        "Linux was not measured in a virtual machine this time, so there is no ratio".to_string()
    } else {
        format!(
            "Linux (VM) is Linux in a virtual machine set up the same way, the baseline Radix \
             must match: the gate fails at any ratio over {GATE_RATIO:.2} (lower is better)"
        )
    };
    let _ = writeln!(
        out,
        "Radix {VERSION} at {}, in QEMU {} with {VM_CPUS} CPUs, on {}. {baseline}. Linux (host) \
         is Linux {} on the same machine, outside any virtual machine, for reference. Each \
         number is the average cost of one operation{}; lower is better.\n",
        commit(ctx),
        if kvm { "under KVM" } else { "without KVM (emulated, so not comparable)" },
        host_cpu(),
        host_kernel(),
        match (numbers.runs, numbers.more) {
            (0 | 1, _) => String::new(),
            (runs, 0) => {
                format!(", in the fastest of {runs} runs (Radix and Linux (VM) measured in turn)")
            }
            (runs, more) => format!(
                ", in the fastest of {runs} runs (Radix and Linux (VM) measured in turn; {more} \
                 of them because an operation was over the gate after the first {})",
                runs - more
            ),
        },
    );
    let _ = writeln!(
        out,
        "| Operation | Radix | Linux (VM) | Ratio | Linux (host) | Linux equivalent |"
    );
    let _ = writeln!(out, "| --- | ---: | ---: | ---: | ---: | --- |");
    let measured =
        |list: &[Measurement], name| find(list, name).map_or(String::new(), |m| show(m.ns));
    for row in ROWS {
        let ours = find(&numbers.radix, row.radix).map_or("missing".into(), |m| show(m.ns));
        let (vm, host, how) = match row.linux {
            Some((name, how)) => {
                (measured(&numbers.linux_vm, name), measured(&numbers.linux_host, name), how)
            }
            None => (String::new(), String::new(), "none"),
        };
        let ratio = match ratio(numbers, row) {
            Some(ratio) if ratio > GATE_RATIO => format!("**{}**", shown(ratio)),
            Some(ratio) => shown(ratio),
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
         gate's {GATE_RATIO:.2}. docs/design/ipc.md describes each measurement."
    );
    if !numbers.counts.is_empty() {
        counts_table(&mut out, &numbers.counts, numbers.runs > 1);
    }
    out
}

/// What the kernel did per operation: under a hypervisor each timer write,
/// interrupt sent and halt is an exit to the host, so these explain most
/// of the numbers above.
fn counts_table(out: &mut String, counts: &[Counts], first_of_several: bool) {
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
                Some(value) if name.ends_with("_ns") => {
                    let _ = write!(out, " {} |", show(value as f64 / group.rounds as f64));
                }
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
        "\nCounted over all CPUs while each benchmark ran{}, including starting and stopping \
         its programs, and divided by its rounds. A round trip on one CPU needs two switches; \
         anything else is overhead. For the POSIX benchmarks, idle on its CPU is how long the \
         CPU they run on sat idle, waiting, and busy elsewhere how long the other CPUs \
         worked meanwhile. Waiting for TLB flushes is how long a CPU that changed a mapping \
         waited for the other CPUs to flush it, and flushes left to the host how many of \
         them it neither interrupted nor waited for, because the host had stopped them and \
         flushes for them when it runs them again. The tlb shootdown linux row is Linux's \
         own count in its virtual machine: the TLB interrupts its other CPU took.",
        if first_of_several { " in the first run" } else { "" }
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

/// `text` as a JSON string, quoted.
pub(crate) fn json_string(text: &str) -> String {
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
    let _ = writeln!(out, "  \"gate_ratio\": {GATE_RATIO:.2},");
    let _ = writeln!(out, "  \"runs\": {},", numbers.runs);
    let _ = writeln!(out, "  \"more_runs\": {},", numbers.more);
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
                   BENCH name=broken ns=x iterations=1\n\
                   BENCH name=nan ns=nan iterations=1\n\
                   BENCH name=inf ns=inf iterations=1\n\
                   BENCH name=negative ns=-1 iterations=1\n";
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
    fn events_name_what_the_kernel_and_linux_bench_print() {
        let programs = [
            include_str!("../../../kernel/radix/src/usertest.rs"),
            include_str!("../../../userspace/posix/bench.c"),
        ];
        for (name, _) in EVENTS {
            assert!(programs.iter().any(|source| source.contains(&format!("{name}="))), "{name}");
        }
        let linux = include_str!("../../linux-bench/src/main.rs");
        assert!(linux.contains("BENCHCOUNT group=tlb_shootdown_linux rounds={rounds} ipis="));
    }

    #[test]
    fn counts_show_per_round_and_leave_missing_events_empty() {
        let counts = parse_counts(
            "BENCHCOUNT group=tlb_shootdown rounds=100 switches=0 ipis=100 \
             shootdown_wait_ns=500000 shootdowns_left=3\n\
             BENCHCOUNT group=tlb_shootdown_linux rounds=100 ipis=100\n",
        );
        let mut out = String::new();
        counts_table(&mut out, &counts, false);
        assert!(
            out.contains("| tlb shootdown | 0.00 | | 1.00 | | | 5000 ns | 0.03 | | |"),
            "{out}"
        );
        assert!(out.contains("| tlb shootdown linux | | | 1.00 | | | | | | |"), "{out}");
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
            include_str!("../../../userspace/posix/bench.c"),
        ];
        for row in ROWS {
            assert!(prints(&programs, row.radix), "{}", row.radix);
        }
    }

    #[test]
    fn linux_rows_name_what_linux_bench_prints() {
        let program = [
            include_str!("../../linux-bench/src/main.rs"),
            include_str!("../../../userspace/posix/bench.c"),
        ];
        for row in ROWS {
            let (name, _) = row.linux.expect("every row has a Linux equivalent");
            assert!(prints(&program, name), "{name}");
        }
    }

    /// Whether one of the programs prints a measurement called `name`:
    /// whole, or as a name its place on the CPUs is added to.
    fn prints(programs: &[&str], name: &str) -> bool {
        let base = name.trim_end_matches("_same_cpu").trim_end_matches("_cross_cpu");
        let forms = [
            format!("\"{name}\""),
            format!("\"{base}\""),
            format!("name={base} "),
            format!("{base}_{{place}}"),
        ];
        programs.iter().any(|source| forms.iter().any(|form| source.contains(form.as_str())))
    }

    fn measurement(name: &str, ns: f64) -> Measurement {
        Measurement { name: name.into(), ns, iterations: 1 }
    }

    fn numbers(radix_ns: f64, linux_ns: f64) -> Numbers {
        let mut numbers = Numbers {
            runs: 1,
            more: 0,
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
        assert!(check_gate(&numbers(99.0, 100.0)).is_ok());
        assert!(check_gate(&numbers(100.1, 100.0)).is_err());
        assert!(check_gate(&numbers(104.0, 100.0)).is_err());
        assert_eq!(
            over_gate(&numbers(100.1, 100.0)).len(),
            ROWS.iter().filter(|r| r.linux.is_some()).count()
        );
        let mut missing = numbers(90.0, 100.0);
        missing.radix.pop();
        assert!(check_gate(&missing).is_err());
        let mut no_vm = numbers(90.0, 100.0);
        no_vm.linux_vm.clear();
        assert!(check_gate(&no_vm).is_err());
    }

    #[test]
    fn a_ratio_over_the_gate_never_shows_as_parity() {
        assert_eq!(shown(0.934), "0.93");
        assert_eq!(shown(1.0), "1.00");
        assert_eq!(shown(1.004), "1.004");
        assert_eq!(shown(1.006), "1.01");
    }

    #[test]
    fn each_operation_keeps_its_fastest_run() {
        let mut best = Vec::new();
        keep_best(&mut best, vec![measurement("a", 100.0), measurement("b", 50.0)]);
        keep_best(&mut best, vec![measurement("a", 90.0), measurement("b", 70.0)]);
        keep_best(&mut best, vec![measurement("c", 10.0), measurement("a", 95.0)]);
        assert_eq!(best, [measurement("a", 90.0), measurement("b", 50.0), measurement("c", 10.0)]);
    }

    #[test]
    fn budgets_name_rows_on_one_cpu() {
        for &(name, budget) in BUDGETS {
            assert!(ROWS.iter().any(|row| row.radix == name), "{name}");
            assert!(!name.ends_with("_cross_cpu") && !name.ends_with("_parallel"), "{name}");
            assert!(budget > 0, "{name}");
        }
    }

    #[test]
    fn budgets_pass_up_to_the_budget() {
        let counts = |extra: u64| -> Vec<Measurement> {
            BUDGETS
                .iter()
                .map(|&(name, budget)| measurement(name, (budget + extra) as f64))
                .collect()
        };
        assert!(check_budgets(&counts(0)).is_ok());
        assert!(check_budgets(&counts(1)).is_err());
        let mut missing = counts(0);
        missing.pop();
        assert!(check_budgets(&missing).is_err());
    }

    #[test]
    fn initramfs_is_newc() {
        let archive = initramfs(b"\x7fELF", b"\x7fELF");
        assert!(archive.starts_with(b"070701"));
        assert_eq!(archive.len() % 4, 0);
        let text = String::from_utf8_lossy(&archive);
        for name in ["dev\0", "dev/console\0", "init\0", "posix-bench\0", "TRAILER!!!\0"] {
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
