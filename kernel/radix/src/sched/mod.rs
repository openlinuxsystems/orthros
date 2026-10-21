// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Threads and scheduling (Phase 4).
//!
//! Every CPU has a run queue (`queue`) of the threads that may run there:
//! real-time threads by priority, first in first out or round robin, ahead
//! of the EEVDF fair class, whose weights come from nice levels and the
//! CPU weights of resource domains (`crate::domains`). The scheduling
//! policy itself, the queues and the topology decisions, is `radix-sched`,
//! which runs and is tested on the host; this module is the mechanism.
//!
//! A CPU switches threads only in [`reschedule`]: when the running thread
//! blocks, yields or exits, and when something more urgent is waiting
//! (`need_resched`), which a timer interrupt or a wake-up sets. The switch
//! happens on the way out of the interrupt, when the last spin lock is let
//! go, or at once, whichever comes first: spin locks and RCU read sections
//! hold off preemption, nothing else does. No lock is held across the
//! switch itself: a thread's context has an `on_cpu` word that the switch
//! takes and lets go of (`radix-hal`'s `Threads`), so a thread woken onto
//! another CPU while its old CPU is still saving it is simply waited for.
//!
//! Blocking and waking meet under the thread's lock: [`block`] sleeps only
//! if no [`wake`] came since the caller decided to sleep, and a sleeper may
//! wake without cause, so every wait loops on its condition (`wait`).
//! Timers ([`sleep_until`]) are per CPU and end on time: the timer is
//! re-armed for the next sleeper, slice end or tick, whichever is first.
//!
//! Wake-ups place threads with the topology (`balance`): an idle CPU
//! sharing caches first, then one on the same node. A CPU balances its
//! load against the others, level by level, at every tick (less often for
//! farther levels) and whenever it is about to go idle. Idle CPUs wait with
//! MWAIT where it works (and wakers then need no interrupt), else HLT.
//!
//! Exited threads are freed by the reaper thread once their CPU has
//! switched away. The correctness argument, and the TLA+ model of blocking
//! and waking, are in `docs/design/scheduler.md`.
//!
//! Lock order: wait table, then registry or a thread's lock, then a run
//! queue's (two at once in CPU order, for balancing). Every scheduler lock
//! is taken with interrupts off, because the timer interrupt takes them.

mod balance;
mod hooks;
mod queue;
pub mod rcu;
mod thread;
pub mod wait;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use radix_hal::{Cpu, CpuLocal, InterruptController, Ipi, MAX_CPUS, Smp, Threads, Timer};
use radix_klog::{error, info};
use radix_mm::{FRAMES, Flags, HEAP, Kmalloc};
use radix_sched::{CpuLoad, CpuSet, Place, Topology};
use radix_sync::{Mutex, Once, SpinLock};

use crate::Arch;
use crate::domains::{self, DomainError, DomainRef};
use crate::percpu::{PerCpu, Slot};
pub use hooks::{lockdep_last_problem, lockdep_reports, set_lockdep_expecting, write_lockdep};
use queue::{Class, Rq, Weighing};
use thread::{Context, State};
pub use thread::{Name, Policy, Run, Thread, ThreadRef};

/// Pages of stack a kernel thread gets unless it asks otherwise: 16 KiB.
pub const STACK_PAGES: usize = 4;

/// The mutex owner token used before threads exist (as `radix_sync`'s).
const NO_TOKEN: usize = 2;

/// Room for threads that the scheduler's lists start with.
const INITIAL_ROOM: usize = 64;

/// The scheduler's state for one CPU.
struct CpuSched {
    rq: SpinLock<Option<Rq>>,
    /// Set when the running thread should give way; cleared by
    /// [`reschedule`].
    need_resched: AtomicBool,
    /// Written by wakers while the CPU idles in MWAIT.
    wake: AtomicU32,
    /// Whether the idle thread is waiting in `idle_until`.
    polling: AtomicBool,
    /// The token of the running thread.
    current: AtomicUsize,
    /// The published load: queued threads, and the running real-time
    /// priority.
    running: AtomicU32,
    rt_prio: AtomicU8,
    online: AtomicBool,
    /// This CPU's sleepers.
    timers: SpinLock<Option<Vec<(u64, ThreadRef), Kmalloc>>>,
    /// The earliest sleeper's deadline.
    timer_next: AtomicU64,
    /// When the running thread's slice or quantum ends.
    slice_end: AtomicU64,
    ticks: AtomicU64,
    preemptions: AtomicU64,
    idle_ns: AtomicU64,
    /// Balancing attempts that found nothing to move, by level.
    balance_failures: [AtomicU32; 4],
}

impl CpuSched {
    const fn new() -> CpuSched {
        CpuSched {
            rq: SpinLock::new(None),
            need_resched: AtomicBool::new(false),
            wake: AtomicU32::new(0),
            polling: AtomicBool::new(false),
            current: AtomicUsize::new(0),
            running: AtomicU32::new(0),
            rt_prio: AtomicU8::new(0),
            online: AtomicBool::new(false),
            timers: SpinLock::new(None),
            timer_next: AtomicU64::new(u64::MAX),
            slice_end: AtomicU64::new(u64::MAX),
            ticks: AtomicU64::new(0),
            preemptions: AtomicU64::new(0),
            idle_ns: AtomicU64::new(0),
            balance_failures: [const { AtomicU32::new(0) }; 4],
        }
    }

    fn publish(&self, rq: &Rq) {
        let load = rq.load();
        self.running.store(load.running, Ordering::Relaxed);
        self.rt_prio.store(load.rt_prio, Ordering::Relaxed);
    }
}

static CPUS: PerCpu<CpuSched> = PerCpu::new([const { Slot::new(CpuSched::new()) }; MAX_CPUS]);
static TOPOLOGY: Once<Topology> = Once::new();
/// Every thread until the reaper frees it.
static REGISTRY: SpinLock<Option<Vec<ThreadRef, Kmalloc>>> = SpinLock::new(None);
/// Exited threads waiting for the reaper.
static ZOMBIES: SpinLock<Option<Vec<ThreadRef, Kmalloc>>> = SpinLock::new(None);
static ZOMBIE_COUNT: AtomicUsize = AtomicUsize::new(0);
/// What the reaper waits on.
static REAPER: AtomicU64 = AtomicU64::new(0);
/// Threads every list has room for.
static ROOM: AtomicUsize = AtomicUsize::new(0);
/// Held while the lists grow.
static GROWING: Mutex<()> = Mutex::new(());
static THREADS: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
/// Set once the hooks are registered and the boot CPU runs threads.
static STARTED: AtomicBool = AtomicBool::new(false);
/// The CPUs that run threads.
static ONLINE: SpinLock<CpuSet> = SpinLock::new(CpuSet::EMPTY);
static TICK_NS: AtomicU64 = AtomicU64::new(10_000_000);

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

/// The kernel's clock.
pub fn now_ns() -> u64 {
    Arch::now_ns()
}

/// Why a thread could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// No memory for its stack or bookkeeping.
    NoMemory,
    /// Its domain's memory limit has no room for its stack.
    OverLimit,
    /// A nice level or priority out of range.
    InvalidPolicy,
    /// Its affinity names no CPU that runs threads.
    NoCpu,
    /// Its domain is being stopped for want of memory.
    Stopping,
    /// The scheduler is not running yet.
    NotStarted,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SpawnError::NoMemory => "out of memory",
            SpawnError::OverLimit => "the domain's memory limit has no room for the stack",
            SpawnError::InvalidPolicy => "the nice level or priority is out of range",
            SpawnError::NoCpu => "the affinity names no CPU that runs threads",
            SpawnError::Stopping => "the domain is being stopped for want of memory",
            SpawnError::NotStarted => "the scheduler is not running yet",
        })
    }
}

/// How to make a thread.
#[derive(Clone)]
pub struct Options {
    policy: Policy,
    affinity: Option<CpuSet>,
    domain: Option<DomainRef>,
    stack_pages: usize,
}

impl Options {
    /// Fair at nice 0, on any CPU, in the root domain, with
    /// [`STACK_PAGES`] of stack.
    pub fn new() -> Options {
        Options { policy: Policy::DEFAULT, affinity: None, domain: None, stack_pages: STACK_PAGES }
    }

    /// With this policy.
    pub fn policy(mut self, policy: Policy) -> Options {
        self.policy = policy;
        self
    }

    /// Only on these CPUs.
    pub fn affinity(mut self, cpus: CpuSet) -> Options {
        self.affinity = Some(cpus);
        self
    }

    /// Only on CPU `cpu`.
    pub fn pinned(self, cpu: usize) -> Options {
        self.affinity(CpuSet::single(cpu))
    }

    /// In this resource domain.
    pub fn domain(mut self, domain: DomainRef) -> Options {
        self.domain = Some(domain);
        self
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::new()
    }
}

// ---- Setting up ----

/// Turns the code running on the boot CPU into the first thread, `kmain`,
/// pinned to that CPU, gives the CPU its idle thread, and registers the
/// timer and preemption handlers and the lock hooks. Call once, after
/// `Arch::init` and `domains::init`, with interrupts off.
pub fn init(lockdep: Option<bool>) {
    let cpu = Arch::current_cpu();
    let mut topology = Topology::flat(Arch::cpu_count());
    for index in 0..Arch::cpu_count() {
        if let Some(place) = Arch::cpu_place(index) {
            topology.set_place(
                index,
                Place { package: place.package, core: place.core, node: place.node },
            );
        }
    }
    let nodes = FRAMES.nodes();
    for a in 0..nodes {
        for b in 0..nodes {
            topology.set_distance(a, b, FRAMES.distance(a, b));
        }
    }
    let _ = TOPOLOGY.set(topology);
    if Arch::tick_hz() > 0 {
        TICK_NS.store(1_000_000_000 / u64::from(Arch::tick_hz()), Ordering::Relaxed);
    }
    // Room for every CPU's idle thread and worker, and some more.
    let room = INITIAL_ROOM.max(2 * Arch::cpu_count() + INITIAL_ROOM / 2);
    ROOM.store(room, Ordering::Relaxed);
    // `kmain` and this CPU's idle thread.
    THREADS.store(2, Ordering::Relaxed);
    let made = (|| {
        let mut registry = Vec::new_in(kmalloc());
        registry.try_reserve_exact(room).ok()?;
        let mut zombies = Vec::new_in(kmalloc());
        zombies.try_reserve_exact(room).ok()?;
        *REGISTRY.lock_irq() = Some(registry);
        *ZOMBIES.lock_irq() = Some(zombies);
        wait::init(room, kmalloc()).then_some(())?;
        let kmain = make_thread(
            Name::new("kmain"),
            Arch::adopt_current()?,
            idle_main,
            0,
            Policy::DEFAULT,
            CpuSet::single(cpu),
            domains::root(),
            0,
        )?;
        let idle = idle_thread(cpu, Arch::new_context(thread_start, 0, STACK_PAGES)?)?;
        kmain.state.lock_irq().run = Run::Ready;
        start_cpu_with(cpu, kmain.clone(), idle, true)?;
        Some(())
    })();
    if made.is_none() {
        panic!("Scheduler: no memory for the boot CPU's threads");
    }
    Arch::set_timer_handler(on_timer);
    <Arch as Threads>::set_preempt_handler(on_preempt);
    hooks::init(lockdep.unwrap_or(cfg!(debug_assertions)));
    STARTED.store(true, Ordering::Release);
    let (_, idle) = Arch::idle_mode();
    info!(
        "Scheduler: EEVDF fair class ({} ms slices) and FIFO and round-robin real-time classes; \
         timer {}, idle with {idle}; extended state {}",
        radix_sched::BASE_SLICE_NS / 1_000_000,
        Arch::timer_mode(),
        <Arch as Threads>::extended_state()
    );
    info!(
        "Lock checker: {}",
        if hooks::lockdep_enabled() {
            "on; every spin lock and mutex is checked for lock-order and interrupt-safety bugs"
        } else {
            "off (the lockdep flag turns it on)"
        }
    );
}

/// The CPU `cpu` becomes a scheduler CPU: the code running now, on its
/// start-up stack, becomes its idle thread. Called by each other CPU as it
/// starts, with interrupts off; never returns.
pub fn run_cpu(cpu: usize) -> ! {
    reserve_thread();
    let started = (|| {
        let idle = idle_thread(cpu, Arch::adopt_current()?)?;
        start_cpu_with(cpu, idle.clone(), idle, false)
    })();
    if started.is_none() {
        error!("Scheduler: no memory to start CPU {cpu}; it stays idle");
        loop {
            Arch::wait_for_interrupt();
        }
    }
    idle_loop(cpu)
}

/// Starts the scheduler's own threads: the reaper, the workers of `work`,
/// and the out-of-memory supervisor. Call from `kmain` once the other CPUs
/// are up.
pub fn start_threads() {
    let made = spawn("reaper", reaper_main, 0, Options::new().policy(Policy::Fair { nice: -5 }));
    if let Err(err) = made {
        panic!("Scheduler: could not start the reaper: {err}");
    }
    let nodes = FRAMES.nodes();
    info!(
        "Scheduler: {} CPUs run threads, on {nodes} NUMA node{}",
        online().len(),
        if nodes == 1 { "" } else { "s" }
    );
}

fn idle_thread(cpu: usize, context: Context) -> Option<ThreadRef> {
    let idle = make_thread(
        Name::numbered("idle/", cpu),
        context,
        idle_main,
        cpu,
        Policy::Fair { nice: 19 },
        CpuSet::single(cpu),
        domains::root(),
        0,
    )?;
    // Never queued, always ready: it runs whenever nothing else is.
    idle.state.lock_irq().run = Run::Ready;
    Some(idle)
}

#[allow(clippy::too_many_arguments)]
fn make_thread(
    name: Name,
    context: Context,
    entry: fn(usize),
    arg: usize,
    policy: Policy,
    affinity: CpuSet,
    domain: DomainRef,
    charged: u64,
) -> Option<ThreadRef> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    domain.thread_joined();
    let thread = Thread::new(id, name, context, entry, arg, policy, affinity, domain, charged);
    let thread = Arc::try_new_in(thread, kmalloc()).ok()?;
    let mut registry = REGISTRY.lock_irq();
    registry.as_mut()?.push_within_capacity(thread.clone()).ok()?;
    Some(thread)
}

/// Counts one more thread for the lists' room, before making it.
fn reserve_thread() -> usize {
    THREADS.fetch_add(1, Ordering::Relaxed) + 1
}

fn start_cpu_with(cpu: usize, current: ThreadRef, idle: ThreadRef, boot: bool) -> Option<()> {
    let now = Arch::now_ns();
    let room = ROOM.load(Ordering::Relaxed);
    let tick = TICK_NS.load(Ordering::Relaxed);
    let mut rq = Rq::new(current.clone(), idle, now, room, kmalloc(), tick)?;
    let mut timers = Vec::new_in(kmalloc());
    timers.try_reserve_exact(room).ok()?;
    if boot {
        // `kmain` is a ready fair thread like any other.
        let state = current.state.lock_irq();
        rq.enqueue(current.clone(), Class::of(&state, true));
        drop(state);
        rq.pick();
    }
    current.cpu.store(cpu, Ordering::Relaxed);
    let slot = CPUS.get(cpu);
    *slot.timers.lock_irq() = Some(timers);
    slot.current.store(current.token(), Ordering::Relaxed);
    {
        let mut guard = slot.rq.lock_irq();
        slot.publish(&rq);
        *guard = Some(rq);
    }
    slot.online.store(true, Ordering::Release);
    ONLINE.lock_irq().insert(cpu);
    Some(())
}

/// The CPUs that run threads.
pub fn online() -> CpuSet {
    *ONLINE.lock_irq()
}

/// Whether the scheduler runs.
pub fn started() -> bool {
    STARTED.load(Ordering::Acquire)
}

// ---- Making room ----

/// Makes sure every queue, timer list and table has room for `threads`
/// threads, allocating outside their locks.
fn make_room(threads: usize) -> Result<(), SpawnError> {
    if threads <= ROOM.load(Ordering::Acquire) {
        return Ok(());
    }
    let _growing = GROWING.lock();
    let room = ROOM.load(Ordering::Acquire);
    if threads <= room {
        return Ok(());
    }
    let room = threads.max(room * 2);
    grow_list(&REGISTRY, room)?;
    grow_list(&ZOMBIES, room)?;
    if !wait::grow(room, kmalloc()) {
        return Err(SpawnError::NoMemory);
    }
    for cpu in online().iter() {
        let slot = CPUS.get(cpu);
        let (mut fair, mut rt) = (Vec::new_in(kmalloc()), Vec::new_in(kmalloc()));
        fair.try_reserve_exact(room).map_err(|_| SpawnError::NoMemory)?;
        rt.try_reserve_exact(room).map_err(|_| SpawnError::NoMemory)?;
        let old = slot.rq.lock_irq().as_mut().map(|rq| rq.grow(fair, rt));
        drop(old);
        let mut timers = Vec::new_in(kmalloc());
        timers.try_reserve_exact(room).map_err(|_| SpawnError::NoMemory)?;
        let old = {
            let mut list = slot.timers.lock_irq();
            match list.as_mut() {
                Some(list) => {
                    while let Some(entry) = list.pop() {
                        let _ = timers.push_within_capacity(entry);
                    }
                    Some(core::mem::replace(list, timers))
                }
                None => None,
            }
        };
        drop(old);
    }
    ROOM.store(room, Ordering::Release);
    Ok(())
}

fn grow_list(
    list: &SpinLock<Option<Vec<ThreadRef, Kmalloc>>>,
    room: usize,
) -> Result<(), SpawnError> {
    let mut fresh = Vec::new_in(kmalloc());
    fresh.try_reserve_exact(room).map_err(|_| SpawnError::NoMemory)?;
    let old = {
        let mut guard = list.lock_irq();
        let Some(list) = guard.as_mut() else { return Err(SpawnError::NotStarted) };
        while let Some(thread) = list.pop() {
            let _ = fresh.push_within_capacity(thread);
        }
        fresh.reverse();
        core::mem::replace(list, fresh)
    };
    drop(old);
    Ok(())
}

// ---- Threads ----

/// Makes a thread that runs `entry(arg)` and starts it. It exits when
/// `entry` returns, or by calling [`exit`].
pub fn spawn(
    name: &str,
    entry: fn(usize),
    arg: usize,
    options: Options,
) -> Result<ThreadRef, SpawnError> {
    if !started() {
        return Err(SpawnError::NotStarted);
    }
    if !options.policy.is_valid() {
        return Err(SpawnError::InvalidPolicy);
    }
    let allowed = options.affinity.unwrap_or(CpuSet::first(MAX_CPUS)).and(&online());
    if allowed.is_empty() {
        return Err(SpawnError::NoCpu);
    }
    let domain = options.domain.unwrap_or_else(domains::root);
    if domain.is_stopping() {
        return Err(SpawnError::Stopping);
    }
    let charged = options.stack_pages as u64 + 1;
    domain.charge(charged).map_err(|err| match err {
        DomainError::OverLimit => SpawnError::OverLimit,
        _ => SpawnError::NoMemory,
    })?;
    let made = (|| {
        make_room(reserve_thread())?;
        let context =
            Arch::new_context(thread_start, 0, options.stack_pages).ok_or(SpawnError::NoMemory)?;
        make_thread(
            Name::new(name),
            context,
            entry,
            arg,
            options.policy,
            allowed,
            domain.clone(),
            charged,
        )
        .ok_or(SpawnError::NoMemory)
    })();
    let thread = match made {
        Ok(thread) => thread,
        Err(err) => {
            THREADS.fetch_sub(1, Ordering::Relaxed);
            domain.uncharge(charged);
            return Err(err);
        }
    };
    let mut state = thread.state.lock_irq();
    state.run = Run::Ready;
    enqueue_ready(&thread, &mut state, true);
    drop(state);
    Ok(thread)
}

/// The running thread, once the scheduler runs on this CPU.
pub fn current() -> Option<ThreadRef> {
    let were = Arch::disable_interrupts();
    let thread = CPUS.this().rq.lock().as_ref().map(|rq| rq.current.clone());
    Arch::restore_interrupts(were);
    thread
}

/// The running thread's token (its address), or 0 before threads run.
pub fn current_token() -> usize {
    // Preemption held off, so the thread cannot move to another CPU
    // between finding its CPU and reading what runs there.
    CPUS.with(|slot| slot.current.load(Ordering::Relaxed))
}

/// Whether the running code may sleep: it is a thread, with interrupts on
/// and preemption allowed.
pub fn can_sleep() -> bool {
    started()
        && current_token() != 0
        && Arch::interrupts_enabled()
        && Arch::preempt_count() == 0
        && Arch::interrupt_depth() == 0
}

/// Whether the running thread has been asked to stop (the out-of-memory
/// policy asks every thread of a domain it stops): long-running kernel
/// threads check it and return.
pub fn should_stop() -> bool {
    current().is_some_and(|me| me.should_stop())
}

/// Gives the CPU to another thread of the same class, if one waits.
pub fn yield_now() {
    reschedule(Switch::Yield);
}

/// Sleeps until [`wake`] is called for the running thread, unless it has
/// been already since the thread last woke. It may also return without
/// cause, so callers loop until what they wait for has happened.
pub fn block(reason: &'static str) {
    reschedule(Switch::Block(reason));
}

/// Wakes `thread` if it is blocked; otherwise its next [`block`] returns
/// at once. Returns whether it was blocked. Works from interrupt handlers.
pub fn wake(thread: &ThreadRef) -> bool {
    thread.wake_pending.store(true, Ordering::SeqCst);
    let mut state = thread.state.lock_irq();
    if !matches!(state.run, Run::Blocked(_)) {
        return false;
    }
    thread.wake_pending.store(false, Ordering::Relaxed);
    state.run = Run::Ready;
    thread.stats.wakeups.fetch_add(1, Ordering::Relaxed);
    enqueue_ready(thread, &mut state, false);
    true
}

/// Puts a thread that has just become ready on the run queue of the CPU
/// that suits it best, and asks that CPU to switch if the thread should
/// run at once. With the thread's lock held.
fn enqueue_ready(thread: &ThreadRef, state: &mut State, new: bool) {
    let previous = thread.cpu.load(Ordering::Relaxed);
    let target = choose_cpu(previous, state);
    let slot = CPUS.get(target);
    let mut guard = slot.rq.lock();
    let Some(rq) = guard.as_mut() else {
        // `choose_cpu` only picks CPUs that run threads.
        panic!("Scheduler: CPU {target} has no run queue");
    };
    if target != previous && !new {
        thread.stats.migrations.fetch_add(1, Ordering::Relaxed);
    }
    thread.cpu.store(target, Ordering::Relaxed);
    rq.enqueue(thread.clone(), Class::of(state, new));
    let preempt = rq.wants_preempt();
    slot.publish(rq);
    drop(guard);
    if preempt {
        kick(target);
    }
}

/// The CPU a ready thread should go to.
fn choose_cpu(previous: usize, state: &State) -> usize {
    let allowed = state.affinity.and(&online());
    let Some(first) = allowed.lowest() else { return previous };
    let Some(topology) = TOPOLOGY.get() else { return first };
    let loads = loads();
    let cpu = match state.rt_prio() {
        0 => topology.select_cpu(previous, &allowed, &loads),
        prio => topology.select_rt_cpu(previous, prio, &allowed, &loads),
    };
    if allowed.contains(cpu) { cpu } else { first }
}

/// Every CPU's published load.
fn loads() -> [CpuLoad; MAX_CPUS] {
    let mut loads = [CpuLoad::default(); MAX_CPUS];
    for (cpu, load) in loads.iter_mut().enumerate().take(Arch::cpu_count()) {
        let slot = CPUS.get(cpu);
        if slot.online.load(Ordering::Relaxed) {
            *load = CpuLoad {
                running: slot.running.load(Ordering::Relaxed),
                rt_prio: slot.rt_prio.load(Ordering::Relaxed),
            };
        } else {
            // A CPU that runs no threads looks fully busy.
            *load = CpuLoad { running: u32::MAX / 2, rt_prio: u8::MAX };
        }
    }
    loads
}

/// Asks CPU `cpu` to look for a more urgent thread.
fn kick(cpu: usize) {
    let slot = CPUS.get(cpu);
    slot.need_resched.store(true, Ordering::SeqCst);
    if cpu == Arch::current_cpu() {
        // The switch happens when this CPU lets go of its last spin lock or
        // leaves the interrupt it is in.
        return;
    }
    if slot.polling.load(Ordering::SeqCst) {
        slot.wake.store(1, Ordering::Release);
    } else {
        Arch::send_ipi(cpu, Ipi::Reschedule);
    }
}

/// The real-time priority `thread` runs at now (0 for fair).
pub(super) fn rt_prio_of(thread: &ThreadRef) -> u8 {
    thread.state.lock_irq().rt_prio()
}

/// Sets the priority lent to `thread` (priority inheritance), moving it
/// between classes if that changes where it runs. Returns whether the
/// priority it runs at changed.
pub(super) fn set_boost(thread: &ThreadRef, boost: u8) -> bool {
    let mut state = thread.state.lock_irq();
    let before = state.rt_prio();
    state.boost = boost;
    if state.rt_prio() == before {
        return false;
    }
    requeue(thread, &mut state);
    true
}

/// Puts a queued thread back with the class its state now says, keeping
/// its place in the fair class. With its lock held.
fn requeue(thread: &ThreadRef, state: &mut State) {
    if state.run != Run::Ready {
        return;
    }
    loop {
        let cpu = thread.cpu.load(Ordering::Relaxed);
        let slot = CPUS.get(cpu);
        let mut guard = slot.rq.lock();
        // The balancer moves queued threads without their lock.
        if thread.cpu.load(Ordering::Relaxed) != cpu {
            continue;
        }
        let Some(rq) = guard.as_mut() else { return };
        let running = ThreadRef::ptr_eq(&rq.current, thread);
        if let Some(lag) = rq.dequeue(thread) {
            state.lag_ns = lag;
            thread.weight.store(state.policy.weight(), Ordering::Relaxed);
            rq.enqueue(thread.clone(), Class::of(state, false));
            if running {
                // Still the one on the CPU: mark it so, and let the CPU
                // choose again.
                rq.mark_current();
                slot.publish(rq);
                drop(guard);
                kick(cpu);
            } else {
                let preempt = rq.wants_preempt();
                slot.publish(rq);
                drop(guard);
                if preempt {
                    kick(cpu);
                }
            }
        }
        return;
    }
}

/// Changes how `thread` is scheduled.
pub fn set_policy(thread: &ThreadRef, policy: Policy) -> Result<(), SpawnError> {
    if !policy.is_valid() {
        return Err(SpawnError::InvalidPolicy);
    }
    {
        let mut state = thread.state.lock_irq();
        state.policy = policy;
        if state.run != Run::Ready {
            thread.weight.store(policy.weight(), Ordering::Relaxed);
        }
        requeue(thread, &mut state);
    }
    wait::priority_changed(thread);
    Ok(())
}

/// Changes where `thread` may run. If it is queued on a CPU it may no
/// longer use, it moves; if it runs there, it moves at its next switch.
pub fn set_affinity(thread: &ThreadRef, cpus: CpuSet) -> Result<(), SpawnError> {
    let allowed = cpus.and(&online());
    if allowed.is_empty() {
        return Err(SpawnError::NoCpu);
    }
    let mut state = thread.state.lock_irq();
    state.affinity = allowed;
    thread.set_allowed(&allowed);
    if state.run != Run::Ready {
        return Ok(());
    }
    loop {
        let cpu = thread.cpu.load(Ordering::Relaxed);
        if allowed.contains(cpu) {
            return Ok(());
        }
        let slot = CPUS.get(cpu);
        let mut guard = slot.rq.lock();
        if thread.cpu.load(Ordering::Relaxed) != cpu {
            continue;
        }
        let Some(rq) = guard.as_mut() else { return Ok(()) };
        if ThreadRef::ptr_eq(&rq.current, thread) {
            drop(guard);
            kick(cpu);
            return Ok(());
        }
        if let Some(lag) = rq.dequeue(thread) {
            state.lag_ns = lag;
            slot.publish(rq);
            drop(guard);
            enqueue_ready(thread, &mut state, false);
        }
        return Ok(());
    }
}

/// Ends the running thread. Its stack is freed by the reaper once the CPU
/// has switched away from it.
pub fn exit() -> ! {
    let Some(me) = current() else {
        panic!("sched::exit called before the scheduler runs");
    };
    if !me.lockdep.lock().is_empty() {
        error!("Threads: `{}` exits holding mutexes", me.name());
    }
    me.leave_domain();
    {
        let mut zombies = ZOMBIES.lock_irq();
        if let Some(zombies) = zombies.as_mut() {
            let _ = zombies.push_within_capacity(me.clone());
        }
        ZOMBIE_COUNT.fetch_add(1, Ordering::Release);
    }
    drop(me);
    wait::wake_all(reaper_key());
    reschedule(Switch::Exit);
    unreachable!("a thread ran after it exited");
}

/// Waits until `thread` has exited and been reaped.
pub fn join(thread: &ThreadRef) {
    let key = thread.token();
    wait::wait_until(key, "join", || thread.is_reaped());
}

/// Sleeps until the kernel's clock reaches `deadline_ns`, or until the
/// thread is asked to stop ([`Thread::request_stop`]).
pub fn sleep_until(deadline_ns: u64) {
    let Some(me) = current() else {
        while Arch::now_ns() < deadline_ns {
            Arch::delay_us(10);
        }
        return;
    };
    while Arch::now_ns() < deadline_ns && !me.should_stop() {
        sleep_until_inner(&me, deadline_ns, "sleep");
    }
}

/// Sleeps for `ns` nanoseconds.
pub fn sleep_ns(ns: u64) {
    sleep_until(Arch::now_ns().saturating_add(ns));
}

/// Blocks `me`, the running thread, until it is woken or the clock
/// reaches `deadline_ns`, whichever is first.
fn sleep_until_inner(me: &ThreadRef, deadline_ns: u64, reason: &'static str) {
    let were = Arch::disable_interrupts();
    let cpu = Arch::current_cpu();
    let slot = CPUS.get(cpu);
    {
        let mut timers = slot.timers.lock();
        if let Some(timers) = timers.as_mut() {
            if timers.push_within_capacity((deadline_ns, me.clone())).is_err() {
                panic!("Scheduler: a timer list had no room");
            }
            let next = slot.timer_next.load(Ordering::Relaxed).min(deadline_ns);
            slot.timer_next.store(next, Ordering::Relaxed);
        }
    }
    arm(cpu);
    Arch::restore_interrupts(were);
    block(reason);
    // Woken early, or by the timer: either way the entry goes.
    let slot = CPUS.get(cpu);
    let mut timers = slot.timers.lock_irq();
    if let Some(timers) = timers.as_mut()
        && let Some(index) =
            timers.iter().position(|(at, t)| *at == deadline_ns && ThreadRef::ptr_eq(t, me))
    {
        timers.swap_remove(index);
    }
}

/// Wakes this CPU's sleepers whose time has come.
fn expire_timers(cpu: usize, now: u64) {
    let slot = CPUS.get(cpu);
    let mut timers = slot.timers.lock();
    let Some(list) = timers.as_mut() else { return };
    let mut next = u64::MAX;
    let mut index = 0;
    while index < list.len() {
        let (at, _) = list[index];
        if at <= now {
            let (_, thread) = list.swap_remove(index);
            wake(&thread);
        } else {
            next = next.min(at);
            index += 1;
        }
    }
    slot.timer_next.store(next, Ordering::Relaxed);
}

/// Programs this CPU's timer for its next sleeper or slice end. With
/// interrupts off, on CPU `cpu`.
fn arm(cpu: usize) {
    let slot = CPUS.get(cpu);
    let at = slot.timer_next.load(Ordering::Relaxed).min(slot.slice_end.load(Ordering::Relaxed));
    Arch::set_deadline(at);
}

// ---- Switching ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Switch {
    /// Something more urgent may be waiting.
    Preempt,
    /// The thread gives way to others of its class.
    Yield,
    /// The thread sleeps.
    Block(&'static str),
    /// The thread ends.
    Exit,
}

/// Lets this CPU run whatever should run now. See the module comment.
fn reschedule(why: Switch) {
    let were = Arch::disable_interrupts();
    let count = Arch::preempt_count();
    // Holding preemption off keeps the locks' preemption hook from
    // switching inside the switch.
    Arch::set_preempt_count(count + 1);
    let cpu = Arch::current_cpu();
    let slot = CPUS.get(cpu);
    let Some(me) = slot.rq.lock().as_ref().map(|rq| rq.current.clone()) else {
        Arch::set_preempt_count(count);
        Arch::restore_interrupts(were);
        return;
    };
    if count != 0 && why != Switch::Preempt {
        error!(
            "Threads: `{}` would sleep or yield with preemption held off ({count}); a spin lock or \
             an RCU read section is held",
            me.name()
        );
    }
    let mut state = matches!(why, Switch::Block(_) | Switch::Exit).then(|| me.state.lock());
    if matches!(why, Switch::Block(_)) && me.wake_pending.swap(false, Ordering::AcqRel) {
        // Woken since it decided to sleep: carry on.
        drop(state);
        drop(me);
        Arch::set_preempt_count(count);
        Arch::restore_interrupts(were);
        return;
    }
    let mut guard = slot.rq.lock();
    let Some(rq) = guard.as_mut() else { unreachable!("the run queue was there a moment ago") };
    let now = Arch::now_ns();
    rq.charge(now);
    slot.need_resched.store(false, Ordering::Relaxed);
    let mut moving = None;
    match (why, state.as_mut()) {
        (Switch::Block(reason), Some(state)) => {
            state.run = Run::Blocked(reason);
            if let Some(lag) = rq.dequeue(&me) {
                state.lag_ns = lag;
            }
        }
        (Switch::Exit, Some(state)) => {
            state.run = Run::Dead;
            rq.dequeue(&me);
        }
        (Switch::Yield, _) => rq.yield_current(),
        _ => {
            // A ready thread on a CPU it may no longer use moves on.
            if !rq.idling() && !me.allowed().contains(cpu) {
                moving = rq.dequeue(&me);
            }
        }
    }
    let next = rq.pick();
    let switching = !ThreadRef::ptr_eq(&next, &me);
    if switching {
        slot.preemptions.fetch_add(u64::from(why == Switch::Preempt), Ordering::Relaxed);
        rq.switches += 1;
        next.stats.switches.fetch_add(1, Ordering::Relaxed);
        next.stats.last_ran_ns.store(now, Ordering::Relaxed);
        me.stats.last_ran_ns.store(now, Ordering::Relaxed);
    }
    let prev_ptr = ThreadRef::as_ptr(&me);
    let next_ptr = ThreadRef::as_ptr(&next);
    rq.current = next;
    slot.current.store(next_ptr as usize, Ordering::Relaxed);
    slot.slice_end.store(rq.slice_end(now), Ordering::Relaxed);
    slot.publish(rq);
    drop(guard);
    drop(state);
    arm(cpu);
    if count == 0 {
        // Outside any RCU read section, switching or not.
        rcu::quiescent(cpu);
    }
    if let Some(lag) = moving {
        let mut state = me.state.lock();
        state.lag_ns = lag;
        enqueue_ready(&me, &mut state, false);
    }
    if !switching {
        drop(me);
        Arch::set_preempt_count(count);
        Arch::restore_interrupts(were);
        return;
    }
    hooks::switch_out(cpu, &me);
    drop(me);
    switch(prev_ptr, next_ptr);
    // Back on this thread, maybe on another CPU.
    after_switch();
    Arch::set_preempt_count(count);
    Arch::restore_interrupts(were);
}

/// The switch itself.
#[allow(unsafe_code)]
fn switch(prev: *const Thread, next: *const Thread) {
    // SAFETY: both pointers come from `ThreadRef`s that outlive the switch,
    // although this frame no longer holds them. `prev` runs here, and the
    // registry (or, once it has exited, the zombie list) keeps it until
    // the reaper frees it, which waits until its context is off every CPU:
    // after the switch below has saved it. `next` is this CPU's
    // `rq.current`, which only this CPU changes, in `reschedule`, so not
    // before the switch has gone over to it; it may exit and be freed while
    // `prev` is switched out, but the switch does not touch `next` again
    // once it runs it.
    unsafe { Arch::switch(&(*prev).context, &(*next).context) }
}

/// What a thread does when a switch brings it back, or first starts it.
fn after_switch() {
    let cpu = Arch::current_cpu();
    if let Some(me) = CPUS.get(cpu).rq.lock().as_ref().map(|rq| rq.current.clone()) {
        hooks::switch_in(cpu, &me);
    }
}

/// Where every new thread starts, from its first switch.
fn thread_start(_: usize) -> ! {
    after_switch();
    let me = current().expect("a new thread runs on a scheduler CPU");
    let (entry, arg) = (me.entry, me.arg);
    drop(me);
    Arch::set_preempt_count(0);
    Arch::enable_interrupts();
    entry(arg);
    exit()
}

/// The locks' preemption hook: preemption was just allowed again.
pub(crate) fn preempt_enable() {
    if Arch::preempt_enable() == 0
        && CPUS.this().need_resched.load(Ordering::Relaxed)
        && Arch::interrupts_enabled()
        && started()
    {
        reschedule(Switch::Preempt);
    }
}

/// Called on the way out of an interrupt that came in with preemption
/// allowed.
fn on_preempt() {
    if CPUS.this().need_resched.load(Ordering::Relaxed) {
        reschedule(Switch::Preempt);
    }
}

/// The timer interrupt: sleepers, slices and quanta, RCU and balancing.
fn on_timer(tick: bool) {
    let cpu = Arch::current_cpu();
    let slot = CPUS.get(cpu);
    if !slot.online.load(Ordering::Acquire) {
        return;
    }
    let now = Arch::now_ns();
    expire_timers(cpu, now);
    {
        let mut guard = slot.rq.lock();
        if let Some(rq) = guard.as_mut() {
            if rq.charge(now) {
                slot.need_resched.store(true, Ordering::Relaxed);
            }
            slot.slice_end.store(rq.slice_end(now), Ordering::Relaxed);
        }
    }
    if tick {
        slot.ticks.fetch_add(1, Ordering::Relaxed);
        // The interrupted code could have been preempted, so it was not in
        // an RCU read section.
        if Arch::preempt_count() == 0 && Arch::interrupt_depth() <= 1 {
            rcu::quiescent(cpu);
        }
        balance::tick(cpu, now);
    }
    arm(cpu);
}

/// Each CPU's idle thread, as a new thread (the boot CPU's).
fn idle_main(cpu: usize) {
    idle_loop(cpu)
}

/// What a CPU does when nothing else is ready: pulls work from busier
/// CPUs, else waits for an interrupt or a waker's write.
fn idle_loop(cpu: usize) -> ! {
    let slot = CPUS.get(cpu);
    loop {
        Arch::disable_interrupts();
        rcu::quiescent(cpu);
        if slot.running.load(Ordering::Relaxed) == 0 {
            balance::idle(cpu, Arch::now_ns());
        }
        if slot.need_resched.load(Ordering::SeqCst) || slot.running.load(Ordering::Relaxed) > 0 {
            reschedule(Switch::Preempt);
            continue;
        }
        slot.wake.store(0, Ordering::Relaxed);
        slot.polling.store(true, Ordering::SeqCst);
        let started = Arch::now_ns();
        if !slot.need_resched.load(Ordering::SeqCst) {
            Arch::idle_until(&slot.wake);
        }
        slot.polling.store(false, Ordering::SeqCst);
        slot.idle_ns.fetch_add(Arch::now_ns().saturating_sub(started), Ordering::Relaxed);
    }
}

// ---- The reaper ----

fn reaper_key() -> usize {
    &REAPER as *const AtomicU64 as usize
}

/// Frees exited threads once their CPUs have switched away from them.
fn reaper_main(_: usize) {
    loop {
        wait::wait_until(reaper_key(), "reap", || ZOMBIE_COUNT.load(Ordering::Acquire) > 0);
        loop {
            let ready = {
                let mut zombies = ZOMBIES.lock_irq();
                let Some(list) = zombies.as_mut() else { break };
                let found = list.iter().position(|t| {
                    t.state.try_lock().is_some_and(|s| s.run == Run::Dead)
                        && !<Arch as Threads>::is_running(&t.context)
                });
                found.map(|index| list.swap_remove(index))
            };
            match ready {
                Some(thread) => reap(thread),
                None => break,
            }
        }
        if ZOMBIE_COUNT.load(Ordering::Acquire) > 0 {
            // One is still being switched away from.
            sleep_ns(1_000_000);
        }
    }
}

fn reap(thread: ThreadRef) {
    ZOMBIE_COUNT.fetch_sub(1, Ordering::AcqRel);
    let registered = {
        let mut registry = REGISTRY.lock_irq();
        registry.as_mut().and_then(|list| {
            let index = list.iter().position(|t| ThreadRef::ptr_eq(t, &thread))?;
            Some(list.swap_remove(index))
        })
    };
    THREADS.fetch_sub(1, Ordering::Relaxed);
    thread.reaped.store(true, Ordering::Release);
    wait::wake_all(thread.token());
    // The last references go here, in thread context, so the stack can be
    // unmapped (unless a joiner still holds one; it drops it likewise).
    drop(registered);
    drop(thread);
}

/// The thread whose token is `token`.
pub(super) fn registry_find(token: usize) -> Option<ThreadRef> {
    let registry = REGISTRY.lock_irq();
    registry.as_ref()?.iter().find(|t| t.token() == token).cloned()
}

/// Calls `f` on every thread. `f` runs with the registry locked and
/// interrupts off, so it must not block or allocate; it may wake threads.
pub fn for_each_thread(mut f: impl FnMut(&ThreadRef)) {
    let registry = REGISTRY.lock_irq();
    for thread in registry.iter().flatten() {
        f(thread);
    }
}

/// How many threads exist (not yet reaped).
pub fn thread_count() -> usize {
    THREADS.load(Ordering::Relaxed)
}

// ---- Balancing support ----

/// Moves a thread that `take_for` took off `from` to `rq`, the queue of
/// `cpu`.
fn adopt(rq: &mut Rq, cpu: usize, thread: ThreadRef, class: Class) {
    thread.cpu.store(cpu, Ordering::Relaxed);
    thread.stats.migrations.fetch_add(1, Ordering::Relaxed);
    rq.enqueue(thread, class);
}

// ---- Reports ----

/// Threads that balancing pulled to CPU `cpu`, by level (core, package,
/// node, machine), or `None` if it runs no threads.
pub fn pulled(cpu: usize) -> Option<[u64; 4]> {
    if cpu >= MAX_CPUS {
        return None;
    }
    CPUS.get(cpu).rq.lock_irq().as_ref().map(|rq| rq.pulled)
}

/// Checks the fair queue of every CPU, for the self-tests.
pub fn check_queues() -> Result<(), &'static str> {
    for cpu in online().iter() {
        if let Some(rq) = CPUS.get(cpu).rq.lock_irq().as_ref() {
            rq.check()?;
        }
    }
    Ok(())
}

/// Writes the thread table, for `kdb`.
pub fn write_threads(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
    let Some(registry) = REGISTRY.try_lock_irq() else {
        line(format_args!("Threads: another CPU holds the thread table"));
        return;
    };
    let list = registry.as_deref().unwrap_or(&[]);
    line(format_args!(
        "{} threads (id, name, CPU, state, policy, lent priority, CPUs allowed, run time, \
         switches, migrations, domain):",
        list.len()
    ));
    let everywhere = online();
    for thread in list {
        let Some(info) = thread.info() else {
            line(format_args!("  {:>4} {:<16} (its lock is held)", thread.id(), thread.name()));
            continue;
        };
        let (run, policy, boost) = (info.run, info.policy, info.boost);
        let mut allowed = radix_klog::TextBuf::<12>::new();
        let _ = match (info.affinity.len(), info.affinity.lowest()) {
            (1, Some(cpu)) => fmt::Write::write_fmt(&mut allowed, format_args!("cpu{cpu}")),
            _ if everywhere.is_subset(&info.affinity) => fmt::Write::write_str(&mut allowed, "all"),
            (n, _) => fmt::Write::write_fmt(&mut allowed, format_args!("{n} CPUs")),
        };
        let mut state = radix_klog::TextBuf::<40>::new();
        let _ = fmt::Write::write_fmt(&mut state, format_args!("{run}"));
        let mut how = radix_klog::TextBuf::<24>::new();
        let _ = fmt::Write::write_fmt(&mut how, format_args!("{policy}"));
        line(format_args!(
            "  {:>4} {:<16} cpu{:<3} {:<22} {:<16} {:>2} {:<8} {:>7}.{:03} s {:>8} {:>5} {}",
            thread.id(),
            thread.name(),
            thread.cpu(),
            state,
            how,
            boost,
            allowed,
            thread.runtime_ns() / 1_000_000_000,
            thread.runtime_ns() / 1_000_000 % 1000,
            thread.switches(),
            thread.migrations(),
            thread.domain().name()
        ));
    }
}

/// Writes every CPU's run queue, for `kdb`.
pub fn write_queues(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
    let (wakeups, handoffs, boosts) = wait::stats();
    let mut waiting = radix_klog::TextBuf::<16>::new();
    let _ = match wait::waiting() {
        Some(n) => fmt::Write::write_fmt(&mut waiting, format_args!("{n}")),
        None => fmt::Write::write_str(&mut waiting, "(locked)"),
    };
    line(format_args!(
        "Scheduler: {} threads, {} waiting in the wait table; {} wake-ups through it, {} mutex \
         handoffs, {} priority loans; {} grace periods",
        thread_count(),
        waiting,
        wakeups,
        handoffs,
        boosts,
        rcu::completed()
    ));
    for cpu in 0..Arch::cpu_count().min(MAX_CPUS) {
        let slot = CPUS.get(cpu);
        if !slot.online.load(Ordering::Relaxed) {
            continue;
        }
        let Some(guard) = slot.rq.try_lock_irq() else {
            line(format_args!("  cpu{cpu}: another CPU holds its run queue"));
            continue;
        };
        let Some(rq) = guard.as_ref() else { continue };
        line(format_args!(
            "  cpu{cpu}: {} queued, running `{}`; {} switches, {} preemptions, {} ms idle; pulled \
             {}/{}/{}/{} (core/package/node/machine){}",
            rq.len(),
            rq.current.name(),
            rq.switches,
            slot.preemptions.load(Ordering::Relaxed),
            slot.idle_ns.load(Ordering::Relaxed) / 1_000_000,
            rq.pulled[0],
            rq.pulled[1],
            rq.pulled[2],
            rq.pulled[3],
            if slot.need_resched.load(Ordering::Relaxed) { "; switch pending" } else { "" }
        ));
        rq.for_each(|thread, running, weighing| {
            let mut how = radix_klog::TextBuf::<24>::new();
            let _ = match weighing {
                Weighing::Weight(weight) => {
                    fmt::Write::write_fmt(&mut how, format_args!("weight {weight}"))
                }
                Weighing::Prio(prio) => {
                    fmt::Write::write_fmt(&mut how, format_args!("real-time {prio}"))
                }
            };
            line(format_args!(
                "    {} {:<16} {}",
                if running { "*" } else { " " },
                thread.name(),
                how
            ));
        });
    }
}
