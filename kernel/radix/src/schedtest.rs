//! The scheduler's self-tests (Phase 4), which `selftest` runs by name:
//!
//! - `fpu`: three threads on every CPU fill the vector registers with their
//!   own pattern and switch away, by yielding, sleeping and being
//!   preempted, about a thousand times each; every pattern must survive.
//! - `sched-fair`: threads spinning on one CPU share it by weight: equally
//!   at equal nice levels, about 3:1 between nice 0 and nice 5, and by CPU
//!   weight between resource domains, however many threads each has.
//! - `sched-rt`: waking a more urgent real-time thread runs it before the
//!   wake returns, and a less urgent one only once the waker sleeps; a
//!   sleeping FIFO thread wakes on time on a CPU busy with fair work; FIFO
//!   threads of one priority run one after the other, round-robin ones
//!   take turns.
//! - `sched-pi`: priority inversion, plain and through a chain of two
//!   mutexes, ends as soon as the low-priority holder lets go, because the
//!   holder inherits the waiter's priority.
//! - `sched-balance`: threads started on one CPU spread over every CPU, on
//!   both NUMA nodes.
//! - `lockdep`: the lock checker reports a seeded AB-BA deadlock between
//!   spin locks and one between mutexes, each half in its own thread, and
//!   a mutex taken under a spin lock.
//! - `rcu`: readers on the other CPUs never see an object retired while
//!   they were reading it.
//! - `oom`: a domain's memory limit refuses a charge with an error; when
//!   the machine runs out of memory an app domain is stopped first, then a
//!   service domain, which its supervisor restarts, and no thread of the
//!   system domain is touched.
//!
//! Each test logs `selftest NAME: passed; ...` or one `failed` line per
//! problem. The timing bounds are loose enough for emulation, and tight
//! enough that a scheduler without the property tested would miss them.

use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use radix_cap::{Rights, SlotRef};
use radix_hal::{CpuLocal, Smp, Threads};
use radix_klog::{error, info, warn};
use radix_lockdep::Problem;
use radix_mm::{FRAMES, Flags, HEAP, Kmalloc, Owner};
use radix_sched::CpuSet;
use radix_sync::{Mutex, SpinLock};

use crate::Arch;
use crate::domains::{self, DomainRef, OomClass, Settings};
use crate::sched::{self, Options, Policy, Run, STACK_PAGES, ThreadRef, wait};

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

fn key<T>(value: &'static T) -> usize {
    value as *const T as usize
}

/// Tells the threads of the running test to stop.
static STOP: AtomicBool = AtomicBool::new(false);

/// Problems found by one test.
struct Outcome {
    name: &'static str,
    problems: u32,
}

impl Outcome {
    fn new(name: &'static str) -> Outcome {
        Outcome { name, problems: 0 }
    }

    fn fail(&mut self, what: fmt::Arguments<'_>) {
        error!("selftest {}: failed; {what}", self.name);
        self.problems += 1;
    }

    fn check(&mut self, ok: bool, what: fmt::Arguments<'_>) {
        if !ok {
            self.fail(what);
        }
    }

    fn finish(self, passed: fmt::Arguments<'_>) {
        if self.problems == 0 {
            info!("selftest {}: passed; {passed}", self.name);
        }
    }
}

/// The threads one part of a test starts, stopped and joined together.
struct Crew {
    threads: Vec<ThreadRef, Kmalloc>,
    failed: bool,
}

/// The most threads a crew holds.
const CREW_MAX: usize = 64;

impl Crew {
    fn new() -> Crew {
        STOP.store(false, Ordering::SeqCst);
        let mut threads = Vec::new_in(kmalloc());
        let failed = threads.try_reserve_exact(CREW_MAX).is_err();
        Crew { threads, failed }
    }

    fn spawn(
        &mut self,
        name: &str,
        entry: fn(usize),
        arg: usize,
        options: Options,
    ) -> Option<ThreadRef> {
        if self.failed {
            return None;
        }
        match sched::spawn(name, entry, arg, options) {
            Ok(thread) => {
                if self.threads.push_within_capacity(thread.clone()).is_err() {
                    error!("selftest: more than {CREW_MAX} threads in one part");
                    self.failed = true;
                }
                Some(thread)
            }
            Err(err) => {
                error!("selftest: could not start a `{name}` thread: {err}");
                self.failed = true;
                None
            }
        }
    }

    fn threads(&self) -> &[ThreadRef] {
        &self.threads
    }

    /// Stops every thread and waits until all have exited.
    fn join(self) {
        STOP.store(true, Ordering::SeqCst);
        for thread in &self.threads {
            thread.request_stop();
            sched::wake(thread);
        }
        for thread in &self.threads {
            sched::join(thread);
        }
    }
}

/// The CPU the single-CPU tests run on: the last one, away from `kmain`.
fn test_cpu() -> usize {
    sched::online().iter().last().unwrap_or(0)
}

/// Waits up to two seconds for `thread` to block.
fn wait_blocked(thread: &ThreadRef) -> bool {
    wait_for(|| thread.info().is_some_and(|info| matches!(info.run, Run::Blocked(_))))
}

/// Waits up to two seconds for `condition`, polling.
fn wait_for(condition: impl Fn() -> bool) -> bool {
    let deadline = sched::now_ns() + 2_000_000_000;
    while !condition() {
        if sched::now_ns() >= deadline {
            return false;
        }
        sched::sleep_ns(200_000);
    }
    true
}

/// Spins until the time comes or the test stops.
fn spin_until(deadline_ns: u64) {
    while sched::now_ns() < deadline_ns && !STOP.load(Ordering::Relaxed) {
        core::hint::spin_loop();
    }
}

fn spin(_: usize) {
    while !STOP.load(Ordering::Relaxed) {
        core::hint::spin_loop();
    }
}

/// `part` of `whole`, in tenths of a per cent.
fn permille(part: u64, whole: u64) -> u64 {
    (u128::from(part) * 1000 / u128::from(whole.max(1))) as u64
}

/// Whether `got_num / got_den` is within `percent` per cent of
/// `want_num / want_den`.
fn near(got_num: u64, got_den: u64, want_num: u64, want_den: u64, percent: u64) -> bool {
    let got = u128::from(got_num) * u128::from(want_den) * 100;
    let want = u128::from(want_num) * u128::from(got_den);
    got >= want * u128::from(100 - percent) && got <= want * u128::from(100 + percent)
}

// ---- fpu ----

const FPU_THREADS_PER_CPU: usize = 3;
const FPU_ROUNDS: u64 = 1000;
static FPU_BAD: AtomicU64 = AtomicU64::new(0);

fn fpu_thread(index: usize) {
    for round in 0..FPU_ROUNDS {
        let seed = ((index as u64) << 32) | round;
        Arch::self_test_fill_vectors(seed);
        match round % 64 {
            // Blocks and is woken by the timer.
            0 => sched::sleep_ns(200_000),
            // Runs past its slice, so the timer preempts it.
            1 => spin_until(sched::now_ns() + 4_000_000),
            _ => sched::yield_now(),
        }
        if !Arch::self_test_vectors_hold(seed) {
            FPU_BAD.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `selftest=fpu`.
pub fn fpu() {
    let mut outcome = Outcome::new("fpu");
    FPU_BAD.store(0, Ordering::Relaxed);
    let mut crew = Crew::new();
    let mut index = 0;
    for cpu in sched::online().iter() {
        for _ in 0..FPU_THREADS_PER_CPU {
            crew.spawn("fpu", fpu_thread, index, Options::new().pinned(cpu));
            index += 1;
        }
    }
    for thread in crew.threads() {
        sched::join(thread);
    }
    let switches: u64 = crew.threads().iter().map(|t| t.switches()).sum();
    let threads = crew.threads().len();
    let failed = crew.failed;
    crew.join();
    outcome.check(!failed, format_args!("not every thread started"));
    let bad = FPU_BAD.load(Ordering::Relaxed);
    outcome.check(bad == 0, format_args!("{bad} vector-register patterns were lost in a switch"));
    outcome.finish(format_args!(
        "{threads} threads kept their vector registers through {switches} switches ({})",
        <Arch as Threads>::extended_state()
    ));
}

// ---- sched-fair ----

/// How long the spinners run before and while they are measured.
const WARM_UP_NS: u64 = 60_000_000;
const WINDOW_NS: u64 = 600_000_000;

/// Runs one spinner per entry of `setups` on `cpu`, lets `adjust` change
/// them once they run, and returns the CPU time each got while measured.
fn shares(
    cpu: usize,
    setups: &[(Policy, Option<&DomainRef>)],
    adjust: fn(&[ThreadRef]) -> bool,
) -> Option<Vec<u64, Kmalloc>> {
    let mut crew = Crew::new();
    for (index, (policy, domain)) in setups.iter().enumerate() {
        let mut options = Options::new().pinned(cpu).policy(*policy);
        if let Some(domain) = domain {
            options = options.domain((*domain).clone());
        }
        crew.spawn("fair-spin", spin, index, options);
    }
    if crew.failed || !adjust(crew.threads()) {
        crew.join();
        return None;
    }
    sched::sleep_ns(WARM_UP_NS);
    let mut got = Vec::new_in(kmalloc());
    got.try_reserve_exact(setups.len()).ok()?;
    for thread in crew.threads() {
        let _ = got.push_within_capacity(thread.runtime_ns());
    }
    sched::sleep_ns(WINDOW_NS);
    for (before, thread) in got.iter_mut().zip(crew.threads()) {
        *before = thread.runtime_ns().saturating_sub(*before);
    }
    crew.join();
    Some(got)
}

/// `selftest=sched-fair`.
pub fn fair() {
    let mut outcome = Outcome::new("sched-fair");
    let cpu = test_cpu();
    let fair = |nice| (Policy::Fair { nice }, None);

    let leave = |_: &[ThreadRef]| true;

    // Equal nice levels: equal shares.
    let Some(equal) = shares(cpu, &[fair(0), fair(0), fair(0), fair(0)], leave) else {
        return outcome.fail(format_args!("could not start the spinners"));
    };
    let total: u64 = equal.iter().sum();
    for (index, &got) in equal.iter().enumerate() {
        outcome.check(
            near(got, total, 1, 4, 20),
            format_args!(
                "spinner {index} of 4 at nice 0 got {}.{}% of the CPU, not 25%",
                permille(got, total) / 10,
                permille(got, total) % 10
            ),
        );
    }

    // Nice 0 against nice 5, set while both run: weights 1024 and 335.
    let renice = |threads: &[ThreadRef]| {
        threads.get(1).is_some_and(|t| sched::set_policy(t, Policy::Fair { nice: 5 }).is_ok())
    };
    let Some(nice) = shares(cpu, &[fair(0), fair(0)], renice) else {
        return outcome.fail(format_args!("could not start the spinners"));
    };
    outcome.check(
        near(nice[0], nice[1], 1024, 335, 20),
        format_args!(
            "nice 0 against nice 5 got {} ms to {} ms, not about 3:1",
            nice[0] / 1_000_000,
            nice[1] / 1_000_000
        ),
    );

    // Domains of CPU weight 200 (one thread) and 100 (two threads), and a
    // thread of the system domain, whose own threads count as one child of
    // weight 100: shares 2:1:1, the weight-100 domain's split in two.
    let root = domains::root();
    let heavy = domains::create("fair-200", &root, Settings::new(OomClass::App), None);
    let light = domains::create("fair-100", &root, Settings::new(OomClass::App), None);
    let (Ok(heavy), Ok(light)) = (heavy, light) else {
        return outcome.fail(format_args!("could not make the domains"));
    };
    if heavy.set_weight(200).is_err() || heavy.set_weight(0).is_ok() {
        outcome.fail(format_args!("a domain's CPU weight could not be set, or 0 was taken"));
    }
    let got = shares(
        cpu,
        &[
            (Policy::DEFAULT, Some(&heavy)),
            (Policy::DEFAULT, Some(&light)),
            (Policy::DEFAULT, Some(&light)),
            (Policy::DEFAULT, None),
        ],
        leave,
    );
    domains::remove(&heavy);
    domains::remove(&light);
    let Some(got) = got else {
        return outcome.fail(format_args!("could not start the spinners"));
    };
    let (heavy_ns, light_ns, system_ns) = (got[0], got[1] + got[2], got[3]);
    outcome.check(
        near(heavy_ns, light_ns, 2, 1, 20) && near(light_ns, system_ns, 1, 1, 20),
        format_args!(
            "domains of weight 200 and 100 and the system domain's thread got {} ms, {} ms and {} \
             ms, not 2:1:1",
            heavy_ns / 1_000_000,
            light_ns / 1_000_000,
            system_ns / 1_000_000
        ),
    );
    outcome.check(
        near(got[1], got[2], 1, 1, 20),
        format_args!(
            "the two threads of one domain got {} ms and {} ms",
            got[1] / 1_000_000,
            got[2] / 1_000_000
        ),
    );
    if let Err(what) = sched::check_queues() {
        outcome.fail(format_args!("a run queue is inconsistent: {what}"));
    }
    outcome.finish(format_args!(
        "on CPU {cpu}, 4 equal threads got {}/{}/{}/{} ms, nice 0 and 5 got {}:{} ms, domains of \
         weight 200 and 100 and a system thread got {}:{}:{} ms",
        equal[0] / 1_000_000,
        equal[1] / 1_000_000,
        equal[2] / 1_000_000,
        equal[3] / 1_000_000,
        nice[0] / 1_000_000,
        nice[1] / 1_000_000,
        heavy_ns / 1_000_000,
        light_ns / 1_000_000,
        system_ns / 1_000_000
    ));
}

// ---- sched-rt ----

const RT_ROUNDS: u64 = 100;
static SEQ: AtomicU64 = AtomicU64::new(0);
static ROUND: AtomicU64 = AtomicU64::new(0);
static ACK: AtomicU64 = AtomicU64::new(0);
static WOKEN: AtomicU64 = AtomicU64::new(0);
static RT_BAD: AtomicU64 = AtomicU64::new(0);

fn tick() -> u64 {
    SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

fn rt_sleeper(_: usize) {
    let mut seen = 0;
    loop {
        wait::wait_until(key(&ROUND), "selftest", || {
            ROUND.load(Ordering::Acquire) != seen || STOP.load(Ordering::Acquire)
        });
        if STOP.load(Ordering::Acquire) {
            return;
        }
        seen = ROUND.load(Ordering::Acquire);
        WOKEN.store(tick(), Ordering::SeqCst);
        ACK.store(seen, Ordering::Release);
        wait::wake_all(key(&ACK));
    }
}

/// Wakes the sleeper `RT_ROUNDS` times; `higher` says whether it is the
/// more urgent of the two.
fn rt_waker(higher: usize) {
    for round in 1..=RT_ROUNDS {
        let before = tick();
        ROUND.store(round, Ordering::Release);
        wait::wake_all(key(&ROUND));
        let after = tick();
        wait::wait_until(key(&ACK), "selftest", || ACK.load(Ordering::Acquire) == round);
        let woken = WOKEN.load(Ordering::SeqCst);
        let right = if higher == 1 { before < woken && woken < after } else { woken > after };
        if !right {
            RT_BAD.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Runs the waker at `waker` and the sleeper at `sleeper` on `cpu`;
/// returns the rounds that ran in the wrong order.
fn wake_order(cpu: usize, waker: u8, sleeper: u8) -> Option<u64> {
    ROUND.store(0, Ordering::SeqCst);
    ACK.store(0, Ordering::SeqCst);
    RT_BAD.store(0, Ordering::SeqCst);
    let mut crew = Crew::new();
    let options = |prio| Options::new().pinned(cpu).policy(Policy::Fifo { prio });
    let sleeping = crew.spawn("rt-sleeper", rt_sleeper, 0, options(sleeper));
    let blocked = sleeping.as_ref().is_some_and(wait_blocked);
    let waking = blocked
        .then(|| crew.spawn("rt-waker", rt_waker, usize::from(sleeper > waker), options(waker)))
        .flatten();
    if let Some(waking) = &waking {
        sched::join(waking);
    }
    let ok = !crew.failed && waking.is_some();
    crew.join();
    ok.then(|| RT_BAD.load(Ordering::Relaxed))
}

const LATENCY_SAMPLES: usize = 200;
static LATENCIES: [AtomicU64; LATENCY_SAMPLES] = [const { AtomicU64::new(0) }; LATENCY_SAMPLES];

fn rt_timer(_: usize) {
    for (index, latency) in LATENCIES.iter().enumerate() {
        let target = sched::now_ns() + 1_000_000 + (index as u64 % 7) * 137_000;
        sched::sleep_until(target);
        latency.store(sched::now_ns().saturating_sub(target), Ordering::Relaxed);
    }
}

static RT_START: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static RT_END: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static RT_UNTIL: AtomicU64 = AtomicU64::new(0);

/// Spins for 60 ms of wall time.
fn rt_spin_for(index: usize) {
    RT_START[index].store(sched::now_ns(), Ordering::SeqCst);
    spin_until(sched::now_ns() + 60_000_000);
    RT_END[index].store(sched::now_ns(), Ordering::SeqCst);
}

/// Spins until `RT_UNTIL`.
fn rt_spin_until(index: usize) {
    RT_START[index].store(sched::now_ns(), Ordering::SeqCst);
    spin_until(RT_UNTIL.load(Ordering::SeqCst));
    RT_END[index].store(sched::now_ns(), Ordering::SeqCst);
}

/// `selftest=sched-rt`.
pub fn rt() {
    let mut outcome = Outcome::new("sched-rt");
    let cpu = test_cpu();

    // Waking a more urgent thread switches to it at once; a less urgent
    // one waits for the waker to sleep.
    match wake_order(cpu, 10, 20) {
        Some(0) => {}
        Some(bad) => outcome.fail(format_args!(
            "a FIFO 20 thread woken by a FIFO 10 one did not run at once in {bad} of \
             {RT_ROUNDS} rounds"
        )),
        None => outcome.fail(format_args!("could not run the wake-up test")),
    }
    match wake_order(cpu, 20, 10) {
        Some(0) => {}
        Some(bad) => outcome.fail(format_args!(
            "a FIFO 10 thread woken by a FIFO 20 one ran before its waker slept in {bad} of \
             {RT_ROUNDS} rounds"
        )),
        None => outcome.fail(format_args!("could not run the wake-up test")),
    }

    // A sleeping FIFO thread wakes on time on a CPU kept busy by a fair
    // thread (which, alone, has no slice to end: without preemption the
    // waker would wait for the next tick).
    let mut crew = Crew::new();
    crew.spawn("rt-busy", spin, 0, Options::new().pinned(cpu));
    let timer = crew.spawn(
        "rt-timer",
        rt_timer,
        0,
        Options::new().pinned(cpu).policy(Policy::Fifo { prio: 50 }),
    );
    if let Some(timer) = &timer {
        sched::join(timer);
    }
    crew.join();
    let mut late = [0u64; LATENCY_SAMPLES];
    for (late, latency) in late.iter_mut().zip(&LATENCIES) {
        *late = latency.load(Ordering::Relaxed);
    }
    late.sort_unstable();
    let (median, p90, max) =
        (late[LATENCY_SAMPLES / 2], late[LATENCY_SAMPLES * 9 / 10], late[LATENCY_SAMPLES - 1]);
    outcome.check(
        timer.is_some() && median < 1_000_000 && p90 < 3_000_000,
        format_args!(
            "a FIFO thread woke {} us late (median) and {} us (90th percentile) on a busy CPU",
            median / 1000,
            p90 / 1000
        ),
    );

    // FIFO threads of one priority run one after the other.
    let mut crew = Crew::new();
    for index in 0..2 {
        crew.spawn(
            "rt-fifo",
            rt_spin_for,
            index,
            Options::new().pinned(cpu).policy(Policy::Fifo { prio: 30 }),
        );
    }
    for thread in crew.threads() {
        sched::join(thread);
    }
    crew.join();
    let fifo_ok = RT_START[1].load(Ordering::SeqCst) >= RT_END[0].load(Ordering::SeqCst);
    outcome.check(
        fifo_ok,
        format_args!("a FIFO thread ran before its peer of equal priority finished"),
    );

    // Round-robin threads take turns, a quantum each.
    RT_UNTIL.store(sched::now_ns() + 450_000_000, Ordering::SeqCst);
    let mut crew = Crew::new();
    for index in 0..2 {
        crew.spawn(
            "rt-rr",
            rt_spin_until,
            index,
            Options::new().pinned(cpu).policy(Policy::RoundRobin { prio: 30 }),
        );
    }
    for thread in crew.threads() {
        sched::join(thread);
    }
    let runtimes: [u64; 2] =
        core::array::from_fn(|i| crew.threads().get(i).map_or(0, |t| t.runtime_ns()));
    crew.join();
    let total = runtimes[0] + runtimes[1];
    outcome.check(
        runtimes.iter().all(|&ns| permille(ns, total) >= 300),
        format_args!(
            "two round-robin threads got {} ms and {} ms, not turns",
            runtimes[0] / 1_000_000,
            runtimes[1] / 1_000_000
        ),
    );
    outcome.finish(format_args!(
        "wake-ups preempt by priority in {} of {} rounds; a FIFO thread on a busy CPU woke {} us \
         late (median; 90th percentile {} us, worst {} us); FIFO peers ran in turn and \
         round-robin peers got {} ms and {} ms",
        2 * RT_ROUNDS,
        2 * RT_ROUNDS,
        median / 1000,
        p90 / 1000,
        max / 1000,
        runtimes[0] / 1_000_000,
        runtimes[1] / 1_000_000
    ));
}

// ---- sched-pi ----

static PI_A: Mutex<u32> = Mutex::new(0);
static PI_B: Mutex<u32> = Mutex::new(0);
static PI_HOLDING: AtomicBool = AtomicBool::new(false);
static PI_BOOST: AtomicU8 = AtomicU8::new(0);
static PI_WAITED: AtomicU64 = AtomicU64::new(0);
static PI_GOT: AtomicU64 = AtomicU64::new(0);
static PI_MEDIUM_DONE: AtomicU64 = AtomicU64::new(u64::MAX);
/// CPU time the low-priority thread needs with the lock held.
const PI_HOLD_NS: u64 = 30_000_000;
/// How long the medium-priority thread spins.
const PI_MEDIUM_NS: u64 = 300_000_000;

fn pi_low(_: usize) {
    let Some(me) = sched::current() else { return };
    let mut guard = PI_A.lock();
    *guard += 1;
    PI_HOLDING.store(true, Ordering::SeqCst);
    let start = me.runtime_ns();
    while me.runtime_ns().saturating_sub(start) < PI_HOLD_NS && !STOP.load(Ordering::Relaxed) {
        if let Some(info) = me.info() {
            PI_BOOST.fetch_max(info.boost, Ordering::Relaxed);
        }
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }
    drop(guard);
}

/// Holds `PI_B` while it waits for `PI_A`.
fn pi_middle(_: usize) {
    let mut b = PI_B.lock();
    *b += 1;
    let mut a = PI_A.lock();
    *a += 1;
    drop(a);
    drop(b);
}

/// Takes `PI_A` (0) or `PI_B` (1), timing the wait.
fn pi_high(which: usize) {
    let start = sched::now_ns();
    let mut guard = if which == 0 { PI_A.lock() } else { PI_B.lock() };
    let now = sched::now_ns();
    *guard += 1;
    PI_GOT.store(now, Ordering::SeqCst);
    PI_WAITED.store(now - start, Ordering::SeqCst);
    drop(guard);
}

fn pi_medium(_: usize) {
    spin_until(sched::now_ns() + PI_MEDIUM_NS);
    PI_MEDIUM_DONE.store(sched::now_ns(), Ordering::SeqCst);
}

/// A low-priority holder, optionally a middle thread chained behind it,
/// a high-priority waiter, then a medium-priority spinner. Returns the
/// waiter's wait, whether it ended before the spinner did, and the highest
/// priority the holder was lent.
fn inversion(cpu: usize, chain: bool) -> Option<(u64, bool, u8)> {
    PI_HOLDING.store(false, Ordering::SeqCst);
    PI_BOOST.store(0, Ordering::SeqCst);
    PI_GOT.store(0, Ordering::SeqCst);
    PI_MEDIUM_DONE.store(u64::MAX, Ordering::SeqCst);
    let options = |prio| Options::new().pinned(cpu).policy(Policy::Fifo { prio });
    let mut crew = Crew::new();
    let started = (|| {
        crew.spawn("pi-low", pi_low, 0, options(10))?;
        wait_for(|| PI_HOLDING.load(Ordering::SeqCst)).then_some(())?;
        if chain {
            let middle = crew.spawn("pi-middle", pi_middle, 0, options(15))?;
            wait_blocked(&middle).then_some(())?;
        }
        let high = crew.spawn("pi-high", pi_high, usize::from(chain), options(30))?;
        wait_blocked(&high).then_some(())?;
        crew.spawn("pi-medium", pi_medium, 0, options(20))?;
        sched::join(&high);
        Some(())
    })();
    crew.join();
    started?;
    let got = PI_GOT.load(Ordering::SeqCst);
    Some((
        PI_WAITED.load(Ordering::SeqCst),
        got < PI_MEDIUM_DONE.load(Ordering::SeqCst),
        PI_BOOST.load(Ordering::SeqCst),
    ))
}

/// `selftest=sched-pi`.
pub fn pi() {
    let mut outcome = Outcome::new("sched-pi");
    if sched::online().len() < 2 {
        warn!("selftest sched-pi: skipped; it needs two CPUs");
        return;
    }
    let cpu = test_cpu();
    let mut waits = [0u64; 2];
    for (chain, wait) in [false, true].into_iter().zip(&mut waits) {
        let what = if chain { "through a chain of two mutexes" } else { "on one mutex" };
        let Some((waited, before_medium, boost)) = inversion(cpu, chain) else {
            outcome.fail(format_args!("could not set up the inversion {what}"));
            continue;
        };
        *wait = waited;
        outcome.check(
            boost == 30,
            format_args!("the low-priority holder was lent priority {boost}, not 30, {what}"),
        );
        outcome.check(
            before_medium && waited < 150_000_000,
            format_args!(
                "the FIFO 30 waiter {what} waited {} ms{}",
                waited / 1_000_000,
                if before_medium { "" } else { ", until the medium-priority spinner finished" }
            ),
        );
    }
    outcome.check(
        !PI_A.is_locked() && !PI_B.is_locked(),
        format_args!("a mutex is still held after the test"),
    );
    outcome.finish(format_args!(
        "a FIFO 30 thread waited {} ms for a FIFO 10 holder despite a FIFO 20 spinner ({} ms \
         through a chain of two mutexes); the holder was lent priority 30",
        waits[0] / 1_000_000,
        waits[1] / 1_000_000
    ));
}

// ---- sched-balance ----

/// `selftest=sched-balance`.
pub fn balance() {
    let mut outcome = Outcome::new("sched-balance");
    let online = sched::online();
    let cpus = online.len();
    if cpus < 2 {
        warn!("selftest sched-balance: skipped; it needs two CPUs");
        return;
    }
    let start = test_cpu();
    let pulled_before = pulled();
    let mut crew = Crew::new();
    for index in 0..2 * cpus {
        crew.spawn("balance", spin, index, Options::new().pinned(start));
    }
    sched::sleep_ns(20_000_000);
    for thread in crew.threads() {
        if let Err(err) = sched::set_affinity(thread, online) {
            outcome.fail(format_args!("could not widen a thread's affinity: {err}"));
        }
    }
    sched::sleep_ns(1_000_000_000);
    let mut counts = [0usize; radix_hal::MAX_CPUS];
    for thread in crew.threads() {
        counts[thread.cpu().min(radix_hal::MAX_CPUS - 1)] += 1;
    }
    let threads = crew.threads().len();
    crew.join();
    let pulled = pulled().map2(pulled_before);
    let (min, max) = online
        .iter()
        .map(|cpu| counts[cpu])
        .fold((usize::MAX, 0), |(min, max), n| (min.min(n), max.max(n)));
    let mut spread = radix_klog::TextBuf::<128>::new();
    for (index, cpu) in online.iter().enumerate() {
        let _ = fmt::Write::write_fmt(
            &mut spread,
            format_args!("{}{}", if index == 0 { "" } else { "/" }, counts[cpu]),
        );
    }
    outcome.check(
        min >= 1 && max - min <= 2,
        format_args!("{threads} threads started on CPU {start} ended up spread {spread}"),
    );
    let node_of = |cpu: usize| <Arch as Smp>::cpu_place(cpu).map_or(0, |place| place.node);
    let elsewhere: usize =
        online.iter().filter(|&cpu| node_of(cpu) != node_of(start)).map(|cpu| counts[cpu]).sum();
    if FRAMES.nodes() > 1 {
        outcome.check(elsewhere > 0, format_args!("no thread moved to the other NUMA node"));
    }
    outcome.finish(format_args!(
        "{threads} threads started on CPU {start} spread {spread} over {cpus} CPUs, {elsewhere} \
         to another NUMA node; pulled {} within cores, {} within packages, {} within nodes and \
         {} across them",
        pulled[0], pulled[1], pulled[2], pulled[3]
    ));
}

/// Threads pulled by balancing, by level, over every CPU.
fn pulled() -> Pulled {
    let mut total = [0u64; 4];
    for cpu in sched::online().iter() {
        if let Some(pulled) = sched::pulled(cpu) {
            for (total, n) in total.iter_mut().zip(pulled) {
                *total += n;
            }
        }
    }
    Pulled(total)
}

struct Pulled([u64; 4]);

impl Pulled {
    fn map2(self, before: Pulled) -> [u64; 4] {
        core::array::from_fn(|i| self.0[i].saturating_sub(before.0[i]))
    }
}

// ---- lockdep ----

static LD_A: SpinLock<u32> = SpinLock::new(0);
static LD_B: SpinLock<u32> = SpinLock::new(0);
static LD_MA: Mutex<u32> = Mutex::new(0);
static LD_MB: Mutex<u32> = Mutex::new(0);
static LD_S: SpinLock<u32> = SpinLock::new(0);
static LD_M: Mutex<u32> = Mutex::new(0);

/// Takes two locks in the order `which` says.
fn lock_pair(which: usize) {
    match which {
        0 => {
            let mut a = LD_A.lock();
            let mut b = LD_B.lock();
            *a += 1;
            *b += 1;
        }
        1 => {
            let mut b = LD_B.lock();
            let mut a = LD_A.lock();
            *a += 1;
            *b += 1;
        }
        2 => {
            let mut a = LD_MA.lock();
            let mut b = LD_MB.lock();
            *a += 1;
            *b += 1;
        }
        3 => {
            let mut b = LD_MB.lock();
            let mut a = LD_MA.lock();
            *a += 1;
            *b += 1;
        }
        _ => {
            let mut spin = LD_S.lock();
            let mut mutex = LD_M.lock();
            *spin += 1;
            *mutex += 1;
        }
    }
}

/// Runs `lock_pair` for each of `steps`, one thread after the other, and
/// returns the reports the checker made and its last problem.
fn seeded(steps: &[usize]) -> Option<(u32, Option<Problem>)> {
    let before = sched::lockdep_reports();
    for &step in steps {
        let mut crew = Crew::new();
        let thread = crew.spawn("lockdep", lock_pair, step, Options::new())?;
        sched::join(&thread);
        crew.join();
    }
    Some((sched::lockdep_reports() - before, sched::lockdep_last_problem()))
}

/// `selftest=lockdep`.
pub fn lockdep() {
    let mut outcome = Outcome::new("lockdep");
    sched::set_lockdep_expecting(true);
    let cases: [(&[usize], Problem, &str); 3] = [
        (&[0, 1], Problem::Inversion, "an AB-BA deadlock between spin locks"),
        (&[2, 3], Problem::Inversion, "an AB-BA deadlock between mutexes"),
        (&[4], Problem::SleepInAtomic, "a mutex taken under a spin lock"),
    ];
    for (steps, problem, what) in cases {
        info!("selftest lockdep: seeding {what}; the lock checker should report it");
        match seeded(steps) {
            Some((1, Some(found))) if found == problem => {}
            Some((reports, found)) => outcome.fail(format_args!(
                "seeding {what} made {reports} reports (last: {found:?}), not one {problem:?}"
            )),
            None => outcome.fail(format_args!("could not start the threads for {what}")),
        }
    }
    sched::set_lockdep_expecting(false);
    outcome.finish(format_args!(
        "the lock checker caught an AB-BA deadlock between spin locks and one between mutexes, \
         each half in its own thread, and a mutex taken under a spin lock"
    ));
}

// ---- rcu ----

const RCU_ROUNDS: u64 = 100;
static RCU_CURRENT: AtomicUsize = AtomicUsize::new(0);
static RCU_ALIVE: [AtomicBool; 2] = [AtomicBool::new(true), AtomicBool::new(false)];
static RCU_BAD: AtomicU64 = AtomicU64::new(0);
static RCU_READS: AtomicU64 = AtomicU64::new(0);

fn rcu_reader(_: usize) {
    let mut sections = 0u64;
    while !STOP.load(Ordering::Relaxed) {
        {
            let _reading = radix_sync::rcu::read_lock();
            let slot = RCU_CURRENT.load(Ordering::Acquire);
            for _ in 0..64 {
                if !RCU_ALIVE[slot].load(Ordering::Acquire) {
                    RCU_BAD.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                core::hint::spin_loop();
            }
        }
        sections += 1;
        if sections.is_multiple_of(16) {
            sched::yield_now();
        }
    }
    RCU_READS.fetch_add(sections, Ordering::Relaxed);
}

/// `selftest=rcu`.
pub fn rcu() {
    let mut outcome = Outcome::new("rcu");
    let me = Arch::current_cpu();
    let others = sched::online().minus(&CpuSet::single(me));
    if others.is_empty() {
        warn!("selftest rcu: skipped; it needs two CPUs");
        return;
    }
    RCU_BAD.store(0, Ordering::Relaxed);
    RCU_READS.store(0, Ordering::Relaxed);
    let before = (sched::rcu::completed(), sched::now_ns());
    let mut crew = Crew::new();
    for cpu in others.iter() {
        crew.spawn("rcu-reader", rcu_reader, cpu, Options::new().pinned(cpu));
    }
    sched::sleep_ns(10_000_000);
    for _ in 0..RCU_ROUNDS {
        let old = RCU_CURRENT.load(Ordering::Acquire);
        let new = 1 - old;
        RCU_ALIVE[new].store(true, Ordering::Release);
        RCU_CURRENT.store(new, Ordering::Release);
        radix_sync::rcu::synchronize();
        // No reader can still be looking at the old one.
        RCU_ALIVE[old].store(false, Ordering::Release);
    }
    let elapsed = sched::now_ns() - before.1;
    let readers = crew.threads().len();
    let failed = crew.failed;
    crew.join();
    let grace_periods = sched::rcu::completed() - before.0;
    let (bad, reads) = (RCU_BAD.load(Ordering::Relaxed), RCU_READS.load(Ordering::Relaxed));
    outcome.check(!failed, format_args!("not every reader started"));
    outcome.check(bad == 0, format_args!("readers saw a retired object {bad} times"));
    outcome.check(reads > 0, format_args!("the readers read nothing"));
    outcome.check(
        grace_periods >= RCU_ROUNDS,
        format_args!("{RCU_ROUNDS} synchronizations took only {grace_periods} grace periods"),
    );
    outcome.finish(format_args!(
        "{readers} readers on other CPUs made {reads} read sections while {RCU_ROUNDS} objects \
         were retired, none too early; {grace_periods} grace periods in {} ms ({} us each on \
         average)",
        elapsed / 1_000_000,
        sched::rcu::average_ns() / 1000
    ));
}

// ---- oom ----

/// The hog's block size: 2^3 pages, the largest the policy acts for.
const HOG_ORDER: u32 = 3;
static HOG_FAILED: AtomicBool = AtomicBool::new(false);
static HOG_STOPPED: AtomicBool = AtomicBool::new(false);
static HOG_PAGES: AtomicU64 = AtomicU64::new(0);
static RESTARTS: AtomicU32 = AtomicU32::new(0);
static RESTARTED: SpinLock<Option<ThreadRef>> = SpinLock::new(None);

/// Allocates blocks until an allocation fails, then gives them all back.
fn hog(_: usize) {
    let Some(me) = sched::current() else { return };
    let room = (FRAMES.free_pages() >> HOG_ORDER) as usize + 64;
    let mut blocks = Vec::new_in(kmalloc());
    if blocks.try_reserve_exact(room).is_err() {
        error!("selftest oom: no memory for the hog's list");
        return;
    }
    let pages = 1u64 << HOG_ORDER;
    while !me.should_stop() {
        let Ok(phys) = FRAMES.alloc(HOG_ORDER, Flags::MAY_SLEEP, Owner::Kernel) else {
            HOG_FAILED.store(true, Ordering::SeqCst);
            break;
        };
        if me.domain().charge(pages).is_err() || blocks.push_within_capacity(phys).is_err() {
            let _ = FRAMES.free(phys, HOG_ORDER, Owner::Kernel);
            break;
        }
    }
    HOG_STOPPED.store(me.should_stop(), Ordering::SeqCst);
    HOG_PAGES.store(blocks.len() as u64 * pages, Ordering::SeqCst);
    while let Some(phys) = blocks.pop() {
        let _ = FRAMES.free(phys, HOG_ORDER, Owner::Kernel);
        me.domain().uncharge(pages);
    }
}

/// Sleeps until asked to stop.
fn sleeper(_: usize) {
    while !sched::should_stop() {
        sched::sleep_ns(5_000_000);
    }
}

/// The supervisor of the test's service domain.
fn restart_service(domain: &DomainRef) {
    match sched::spawn("oom-service", sleeper, 0, Options::new().domain(domain.clone())) {
        Ok(thread) => {
            *RESTARTED.lock_irq() = Some(thread);
            RESTARTS.fetch_add(1, Ordering::SeqCst);
        }
        Err(err) => error!("selftest oom: could not restart the service: {err}"),
    }
}

/// Runs a hog in `domain` until it is stopped.
fn run_hog(domain: &DomainRef) -> Option<u64> {
    HOG_FAILED.store(false, Ordering::SeqCst);
    HOG_STOPPED.store(false, Ordering::SeqCst);
    let mut crew = Crew::new();
    let hog = crew.spawn("oom-hog", hog, 0, Options::new().domain(domain.clone()))?;
    sched::join(&hog);
    crew.join();
    Some(HOG_PAGES.load(Ordering::SeqCst))
}

/// `selftest=oom`.
pub fn oom() {
    let mut outcome = Outcome::new("oom");
    let root = domains::root();
    let app = |name, limit| {
        domains::create(
            name,
            &root,
            Settings { limit_pages: limit, ..Settings::new(OomClass::App) },
            None,
        )
    };

    // A domain's limit refuses charges with an error.
    let Ok(limited) = app("oom-limited", None) else {
        return outcome.fail(format_args!("could not make the domains"));
    };
    limited.set_limit_pages(Some(64));
    let mut crew = Crew::new();
    let mut made = 0;
    let refusal = loop {
        match sched::spawn("oom-limited", sleeper, 0, Options::new().domain(limited.clone())) {
            Ok(thread) => {
                made += 1;
                if crew.threads.push_within_capacity(thread).is_err() || made > 64 {
                    break None;
                }
            }
            Err(err) => break Some(err),
        }
    };
    crew.join();
    let expected = 64 / (STACK_PAGES as u64 + 1);
    outcome.check(
        refusal == Some(sched::SpawnError::OverLimit) && made == expected && limited.refused() == 1,
        format_args!(
            "a domain limited to 64 pages took {made} threads, not {expected}, then said {refusal:?}"
        ),
    );
    outcome.check(
        limited.usage_pages() == 0,
        format_args!("the limited domain still has {} pages charged", limited.usage_pages()),
    );
    domains::remove(&limited);

    // Out of memory: the app goes first.
    let free_before = FRAMES.free_pages();
    let service = domains::create(
        "oom-service",
        &root,
        Settings { supervisor: Some(restart_service), ..Settings::new(OomClass::Service) },
        None,
    );
    // The app's domain is made the way programs will get theirs: as a
    // kernel object behind a capability.
    let Ok(space) = crate::caps::with(|store| store.create_space(4)).and_then(|made| made) else {
        return outcome.fail(format_args!("could not make a capability space"));
    };
    let slot = SlotRef::new(space, 0);
    let apps = crate::caps::create_domain(slot, &root, "oom-app", Settings::new(OomClass::App));
    let (Ok(apps), Ok(service)) = (apps, service) else {
        let _ = crate::caps::destroy_space(space);
        return outcome.fail(format_args!("could not make the domains"));
    };
    let named = crate::caps::domain_at(slot, Rights::READ);
    outcome.check(
        named.is_ok_and(|named| DomainRef::ptr_eq(&named, &apps)),
        format_args!("the domain capability does not name the domain it made"),
    );
    let first_service =
        sched::spawn("oom-service", sleeper, 0, Options::new().domain(service.clone()));
    let Ok(first_service) = first_service else {
        return outcome.fail(format_args!("could not start the service"));
    };
    info!("selftest oom: an app runs the machine out of memory");
    let hogged = run_hog(&apps);
    outcome.check(
        hogged.is_some() && HOG_FAILED.load(Ordering::SeqCst) && HOG_STOPPED.load(Ordering::SeqCst),
        format_args!("the app's hog was not stopped when memory ran out"),
    );
    outcome.check(
        apps.oom_stops() == 1 && service.oom_stops() == 0 && !first_service.should_stop(),
        format_args!(
            "the app was stopped {} times and the service {} times, not once and never",
            apps.oom_stops(),
            service.oom_stops()
        ),
    );

    // Then a service hogs, with another app around: the app goes, then the
    // service, which its supervisor restarts.
    let Ok(other_app) = app("oom-app2", None) else {
        return outcome.fail(format_args!("could not make the domains"));
    };
    let bystander = sched::spawn("oom-app2", sleeper, 0, Options::new().domain(other_app.clone()));
    info!("selftest oom: a service runs the machine out of memory while another app runs");
    let hogged_again = run_hog(&service);
    if let Ok(bystander) = &bystander {
        sched::join(bystander);
    }
    sched::join(&first_service);
    outcome.check(
        hogged_again.is_some() && HOG_STOPPED.load(Ordering::SeqCst),
        format_args!("the service's hog was not stopped when memory ran out"),
    );
    outcome.check(
        other_app.oom_stops() == 1 && service.oom_stops() == 1,
        format_args!(
            "the other app was stopped {} times and the service {} times, not once each",
            other_app.oom_stops(),
            service.oom_stops()
        ),
    );
    let restarted = wait_for(|| RESTARTS.load(Ordering::SeqCst) > 0);
    outcome.check(restarted, format_args!("the supervisor did not restart the service"));

    // No system thread was asked to stop, and the memory came back.
    let mut touched = 0;
    sched::for_each_thread(|thread| {
        if thread.domain().class() == OomClass::System && thread.should_stop() {
            touched += 1;
        }
    });
    outcome.check(touched == 0, format_args!("{touched} system threads were asked to stop"));
    let free_after = FRAMES.free_pages();
    outcome.check(
        free_after + 1024 >= free_before,
        format_args!("{} pages did not come back", free_before - free_after),
    );

    let restarted = RESTARTED.lock_irq().take();
    if let Some(thread) = restarted {
        thread.request_stop();
        sched::wake(&thread);
        sched::join(&thread);
    }
    for domain in [&other_app, &service] {
        domains::remove(domain);
    }
    // Deleting the last capability to a domain takes it off the table.
    let destroyed = crate::caps::destroy_space(space);
    let mut listed = false;
    domains::for_each(|domain| listed |= DomainRef::ptr_eq(domain, &apps));
    outcome.check(
        destroyed.is_ok() && !listed,
        format_args!("the app's domain outlived its capability"),
    );
    outcome.finish(format_args!(
        "a 64-page domain refused its {}th thread; out of memory, the policy stopped the app \
         holding {} pages, then an idle app before the service holding {} pages, which its \
         supervisor restarted; no system thread was touched",
        expected + 1,
        hogged.unwrap_or(0),
        hogged_again.unwrap_or(0)
    ));
}
