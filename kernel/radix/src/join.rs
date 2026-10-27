// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Work split in two, its halves on two CPUs: [`radix_vfs::join`] for the
//! filesystem compartments (ADR 0133).
//!
//! Every CPU has a helper thread, `help/N`, pinned to it in the fair class
//! at nice 0, as Linux's kworkers are. A caller runs one half of its work
//! itself and hands the other to the helper of another CPU: it claims an
//! idle helper (idle to claimed), leaves it the address of the half, which
//! lives on the caller's stack, marks it posted and wakes it; with no
//! helper idle, it runs both halves itself. After its own half, it takes
//! the other back if the helper has not started it (posted to idle) and
//! runs it; otherwise it waits until the helper is done with it, then
//! marks the helper idle. So the half outlives every use the helper makes
//! of it, and a busy CPU costs the caller no more than the hand-off.
//! `kernel/radix/tla/Join.tla` checks that each half runs once, only
//! while its caller is in the join, and that every join returns.
//!
//! The helper runs the half for whoever asked, in the root domain, as a
//! kworker does. A caller may sleep, so it must hold no spin lock.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use radix_hal::{CpuLocal, MAX_CPUS, Smp};
use radix_klog::error;

use crate::Arch;
use crate::sched::{self, Options, Policy, wait};

/// A helper's states (`Helper::state`).
const IDLE: u8 = 0;
const CLAIMED: u8 = 1;
const POSTED: u8 = 2;
const RUNNING: u8 = 3;
const DONE: u8 = 4;

/// A CPU's helper.
struct Helper {
    /// Its thread waits for work.
    up: AtomicBool,
    state: AtomicU8,
    /// The address of the [`Half`] it has, while it is posted or running.
    half: AtomicUsize,
}

impl Helper {
    const fn new() -> Helper {
        Helper { up: AtomicBool::new(false), state: AtomicU8::new(IDLE), half: AtomicUsize::new(0) }
    }

    /// What the helper waits on for a half.
    fn posted_key(&self) -> usize {
        &raw const self.state as usize
    }

    /// What its caller waits on for the end of the half.
    fn done_key(&self) -> usize {
        &raw const self.half as usize
    }
}

static HELPERS: [Helper; MAX_CPUS] = [const { Helper::new() }; MAX_CPUS];

/// The half a caller hands over.
struct Half<'a> {
    run: &'a mut (dyn FnMut() + Send),
}

/// Starts a helper on every CPU that runs threads, and gives the
/// filesystems [`join`] when there are two or more.
pub fn start() {
    let mut started = 0;
    for cpu in sched::online().iter() {
        let options = Options::new().pinned(cpu).policy(Policy::Fair { nice: 0 });
        let mut name = radix_klog::TextBuf::<16>::new();
        let _ = core::fmt::Write::write_fmt(&mut name, format_args!("help/{cpu}"));
        match sched::spawn(name.as_str(), helper_main, cpu, options) {
            Ok(_) => started += 1,
            Err(err) => error!("Join: no helper for CPU {cpu}: {err}"),
        }
    }
    if started > 1 {
        radix_vfs::set_join(join);
    }
}

fn helper_main(cpu: usize) {
    let Some(me) = HELPERS.get(cpu) else { return };
    me.up.store(true, Ordering::Release);
    loop {
        wait::wait_until(me.posted_key(), "help", || me.state.load(Ordering::Acquire) == POSTED);
        // The caller may have taken the half back meanwhile.
        if me.state.compare_exchange(POSTED, RUNNING, Ordering::Acquire, Ordering::Relaxed).is_err()
        {
            continue;
        }
        let half = me.half.load(Ordering::Relaxed) as *mut Half<'static>;
        #[allow(unsafe_code)]
        // SAFETY: the caller stored the address of its `Half` before it
        // marked this helper posted (release), which the exchange above
        // saw (acquire). Once running, the half is this helper's alone:
        // the caller can no longer take it back, and it stays in `join`,
        // the `Half` and what it borrows in place, until the store of
        // `DONE` below.
        unsafe {
            ((*half).run)();
        }
        me.state.store(DONE, Ordering::Release);
        wait::wake_all(me.done_key());
    }
}

/// Claims the idle helper of another CPU, if there is one: the next CPUs'
/// after this one's first.
fn claim() -> Option<&'static Helper> {
    sched::running()?;
    let count = Arch::cpu_count().min(MAX_CPUS);
    let here = Arch::current_cpu();
    (1..count).map(|k| &HELPERS[(here + k) % count]).find(|helper| {
        helper.up.load(Ordering::Acquire)
            && helper.state.load(Ordering::Relaxed) == IDLE
            && helper
                .state
                .compare_exchange(IDLE, CLAIMED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
    })
}

/// Runs `own` here and `other` on another CPU when a helper is idle, and
/// returns when both are done; otherwise runs one, then the other.
fn join(own: &mut dyn FnMut(), other: &mut (dyn FnMut() + Send)) {
    let Some(helper) = claim() else {
        own();
        other();
        return;
    };
    let mut half = Half { run: other };
    helper.half.store(&raw mut half as usize, Ordering::Relaxed);
    helper.state.store(POSTED, Ordering::Release);
    wait::wake_all(helper.posted_key());
    own();
    if helper.state.compare_exchange(POSTED, IDLE, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        // Not started: it is the caller's again.
        (half.run)();
        return;
    }
    wait::wait_until(helper.done_key(), "join", || helper.state.load(Ordering::Acquire) == DONE);
    helper.state.store(IDLE, Ordering::Release);
}
