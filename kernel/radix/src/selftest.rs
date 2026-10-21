//! Self-tests chosen with `selftest=name`, which `ob test` boots to check
//! the paths that only run when something goes wrong.
//!
//! - `page-fault` touches an unmapped address. The page fault must panic
//!   with a register dump and a backtrace that names this module.
//! - `crash-reboot` panics, and after the restart checks that the crash
//!   log kept the panic report. It needs a restart that keeps RAM, which is
//!   why `ob test` runs it without QEMU's `-no-reboot`.
//! - `breakpoint` runs a breakpoint instruction: `kdb` takes it when booted
//!   with the `kdb` flag, otherwise it is logged and skipped.
//! - `mm-stress` runs the heap stress test on every CPU at once, maps and
//!   unmaps areas (with TLB shootdowns) alongside, and then checks that
//!   nothing leaked and nothing was damaged.
//! - `stack-overflow` recurses until the stack runs into its guard page,
//!   which must panic with a report that says so.
//! - `smep` and `smap` make the kernel run and read a user page, which must
//!   fault (or report that the CPU lacks the feature).
//! - `tlb` checks TLB shootdowns and address-space switches (with PCIDs
//!   when the CPU has them) on every CPU.
//! - `caps` runs the capability store's test suite (`captest`): no
//!   capability can be forged or its rights widened, revocation reaches
//!   everything derived, also while other CPUs derive, and nothing leaks.
//! - `fpu`, `sched-fair`, `sched-rt`, `sched-pi`, `sched-balance`,
//!   `lockdep`, `rcu` and `oom` test the scheduler, the lock checker, RCU
//!   and the out-of-memory policy (`schedtest`).

use core::sync::atomic::{AtomicU64, Ordering};

use radix_hal::cmdline::SELFTESTS;
use radix_hal::{Cpu, CpuLocal, FaultTest, PageTable, Platform, Smp, Timer};
use radix_klog::crashlog::Ending;
use radix_klog::{error, info, warn};
use radix_mm::stress::{self, StressReport};
use radix_mm::{FRAMES, HEAP, Owner};

use crate::panic::SELFTEST_MARKER;
use crate::{Arch, captest, crashlog, schedtest, work};

/// Runs the self-test called `name`.
pub fn run(name: &str) {
    match name {
        "page-fault" => fault("page-fault", FaultTest::Unmapped, "touching an unmapped address"),
        "crash-reboot" => crash_reboot(),
        "breakpoint" => breakpoint(),
        "mm-stress" => mm_stress(),
        "stack-overflow" => stack_overflow(),
        "smep" => fault("smep", FaultTest::UserCode, "running code in a user page"),
        "smap" => fault("smap", FaultTest::UserData, "reading a user page"),
        "tlb" => tlb(),
        "caps" => captest::run(),
        "fpu" => schedtest::fpu(),
        "sched-fair" => schedtest::fair(),
        "sched-rt" => schedtest::rt(),
        "sched-pi" => schedtest::pi(),
        "sched-balance" => schedtest::balance(),
        "lockdep" => schedtest::lockdep(),
        "rcu" => schedtest::rcu(),
        "oom" => schedtest::oom(),
        _ => warn!("selftest: no test called `{name}`; the tests are {SELFTESTS:?}"),
    }
}

#[inline(never)]
fn fault(name: &str, test: FaultTest, what: &str) {
    info!("selftest {name}: {what}; a page-fault panic follows");
    let reason = Arch::self_test_fault(test);
    warn!("selftest {name}: skipped; {reason}");
}

#[inline(never)]
fn crash_reboot() {
    if !crashlog::active() {
        error!("selftest crash-reboot: failed; there is no crash log on this boot");
        return;
    }
    match crashlog::previous(SELFTEST_MARKER) {
        Some((Ending::Panicked, true)) => {
            info!(
                "selftest crash-reboot: passed; the previous boot's panic report survived the restart"
            );
        }
        Some((ending, true)) => {
            error!(
                "selftest crash-reboot: failed; the previous boot panicked as planned, but its log \
                 says it {ending}"
            );
        }
        _ => {
            info!("selftest crash-reboot: panicking now; the next boot checks the crash log");
            panic!("{SELFTEST_MARKER}");
        }
    }
}

#[inline(never)]
fn breakpoint() {
    info!("selftest breakpoint: running a breakpoint instruction");
    Arch::breakpoint();
    info!("selftest breakpoint: passed; execution continued after the breakpoint");
}

/// Heap operations per CPU in `mm-stress`: with 4 CPUs, tens of millions
/// of objects are allocated and freed (the Phase 2 exit criterion).
const STRESS_OPERATIONS: u64 = 12_500_000;
/// Areas each CPU maps and unmaps in `mm-stress`.
const STRESS_AREAS: u64 = 200;

/// Every CPU's results, added up.
static STRESS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// One CPU's part of `mm-stress`.
fn stress_worker() {
    let cpu = Arch::current_cpu();
    let report = stress::run(&HEAP, 0x5eed_0000 + cpu as u64, STRESS_OPERATIONS);
    // Areas in the mapping window, checked to be zeroed, then unmapped:
    // each unmapping shoots the area down on every CPU.
    let mut area_errors = 0;
    for round in 0..STRESS_AREAS {
        let pages = 1 + (round as usize + cpu) % 8;
        let Some(area) = Arch::map_area(pages) else {
            area_errors += 1;
            continue;
        };
        let (mut first, mut last) = ([0xffu8; 8], [0xffu8; 8]);
        let end = area + pages * 4096 - 8;
        if !Arch::read(area, &mut first)
            || !Arch::read(end, &mut last)
            || first != [0; 8]
            || last != [0; 8]
        {
            area_errors += 1;
        }
        Arch::unmap_area(area, pages);
    }
    for (total, value) in STRESS.iter().zip([
        report.allocations,
        report.frees,
        report.failures,
        report.mismatches,
        area_errors,
    ]) {
        total.fetch_add(value, Ordering::Relaxed);
    }
}

#[inline(never)]
fn mm_stress() {
    let me = Arch::current_cpu();
    let cpus = (0..Arch::cpu_count())
        .filter(|&cpu| Arch::cpu_info(cpu).is_some_and(|info| info.online))
        .count();
    info!(
        "selftest mm-stress: {STRESS_OPERATIONS} heap operations and {STRESS_AREAS} mapped areas \
         on each of {cpus} CPUs"
    );
    // Free frames, plus the ones the run may keep for good: page tables of
    // the mapping window and caches of CPUs that had not used the heap.
    let frames = || {
        FRAMES.free_pages()
            + FRAMES.owner_pages(Owner::PageTable)
            + FRAMES.owner_pages(Owner::PerCpu)
    };
    HEAP.drain();
    let before = (HEAP.stats(), frames());
    let started = Arch::now_ns();
    work::on_all(me, stress_worker);
    let millis = (Arch::now_ns() - started) / 1_000_000;
    HEAP.drain();
    let after = (HEAP.stats(), frames());
    let [allocations, frees, failures, mismatches, area_errors] =
        STRESS.each_ref().map(|total| total.load(Ordering::Relaxed));
    let report = StressReport { allocations, frees, failures, mismatches };
    info!(
        "selftest mm-stress: {} objects allocated and {} freed in {}.{:03} s ({} failed for lack \
         of memory)",
        report.allocations,
        report.frees,
        millis / 1000,
        millis % 1000,
        report.failures
    );
    let mut problems = 0;
    let mut problem = |what: core::fmt::Arguments<'_>| {
        error!("selftest mm-stress: {what}");
        problems += 1;
    };
    if report.mismatches > 0 {
        problem(format_args!("{} objects were damaged or handed out twice", report.mismatches));
    }
    if report.allocations != report.frees {
        problem(format_args!(
            "{} objects were allocated but never freed",
            report.allocations.abs_diff(report.frees)
        ));
    }
    if area_errors > 0 {
        problem(format_args!("{area_errors} mapped areas failed or were not zeroed"));
    }
    if after.0.objects != before.0.objects || after.0.large_blocks != before.0.large_blocks {
        problem(format_args!(
            "the heap holds {} objects and {} large blocks, {} and {} before",
            after.0.objects, after.0.large_blocks, before.0.objects, before.0.large_blocks
        ));
    }
    if after.0.corruptions > before.0.corruptions {
        problem(format_args!("the heap found {} damaged objects", after.0.corruptions));
    }
    if after.1 != before.1 {
        problem(format_args!("{} frames were lost", before.1.abs_diff(after.1)));
    }
    if let Err(err) = FRAMES.check() {
        problem(format_args!("the frame allocator is inconsistent: {err}"));
    }
    if problems == 0 {
        info!("selftest mm-stress: passed; nothing leaked and nothing was damaged");
    }
}

/// Uses `depth` KiB of stack, then more, until the guard page stops it.
#[inline(never)]
fn recurse(depth: u64) -> u64 {
    let mut frame = [0u8; 1024];
    frame[0] = depth as u8;
    let frame = core::hint::black_box(frame);
    if depth == u64::MAX {
        return u64::from(frame[1]);
    }
    recurse(depth + 1).wrapping_add(u64::from(frame[0]))
}

#[inline(never)]
fn stack_overflow() {
    info!("selftest stack-overflow: recursing until the stack guard page; a panic follows");
    let result = recurse(0);
    error!("selftest stack-overflow: failed; the recursion returned {result}");
}

#[inline(never)]
fn tlb() {
    let me = Arch::current_cpu();
    let on_others = |job: fn()| work::on_others(me, job);
    match Arch::self_test_tlb(&on_others) {
        Ok(()) => info!("selftest tlb: passed; shootdowns and address-space switches are correct"),
        Err(what) => error!("selftest tlb: failed; {what}"),
    }
}
