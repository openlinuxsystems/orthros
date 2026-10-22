// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Resource domains: groups of threads that share a CPU weight and a
//! memory limit, after Linux's cgroups (v2), and their class for the
//! out-of-memory policy (spec 4.1).
//!
//! Domains form a tree under the root domain, `system`, which holds the
//! kernel's own threads. Each domain has:
//!
//! - a CPU weight, 1 to 10,000 (default 100): on each CPU, the fair class
//!   splits a domain's time between its own threads and its child domains
//!   in proportion to their weights, its own threads together counting as
//!   one child of the default weight (`sched::queue`);
//! - a memory limit, in pages, that its charges and its children's
//!   together may not pass: a charge that would is refused and the
//!   allocation fails with an error for the caller, never a kill;
//! - an out-of-memory class, [`OomClass`]: when the machine as a whole runs
//!   out of memory, `oom` stops whole domains, apps first and services
//!   next, and never `system` ones, which hold the trusted computing base;
//! - optionally a supervisor, which restarts the domain after an
//!   out-of-memory stop: Spark's job once it supervises services (Phase
//!   9); until then a kernel stand-in does it (`oom`).
//!
//! Domains are kernel objects that capabilities name
//! ([`radix_cap::ObjectKind::ResourceDomain`]); `caps::create_domain` makes
//! one with its first capability. Threads are charged for their stacks,
//! and the objects a program makes with `process.create` are charged to
//! its process's domain (`user::account`); Phase 22 builds containers on
//! domains.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use radix_cap::ObjectId;
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sched::{DOMAIN_WEIGHT_DEFAULT, DOMAIN_WEIGHT_MAX};
use radix_sync::{Once, SpinLock};

use crate::sched::Name;

/// A domain, shared.
pub type DomainRef = Arc<Domain, Kmalloc>;

/// Deepest domain below the root.
pub const MAX_DEPTH: u8 = 4;

/// Most domains at once.
pub const MAX_DOMAINS: usize = 256;

/// What the out-of-memory policy may do to a domain, least protected
/// first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OomClass {
    /// A user app: stopped first.
    App,
    /// A system service: stopped once no app is left to stop, and
    /// restarted by its supervisor.
    Service,
    /// The kernel and the trusted computing base: never stopped. An
    /// allocation fails instead (spec 3.4 and 16.1).
    System,
}

impl OomClass {
    /// Lower-case name.
    pub const fn name(self) -> &'static str {
        match self {
            OomClass::App => "app",
            OomClass::Service => "service",
            OomClass::System => "system",
        }
    }
}

/// What a new domain should be.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// CPU weight, 1 to 10,000.
    pub weight: u32,
    /// Memory limit in pages; `None` for none beyond the parent's.
    pub limit_pages: Option<u64>,
    /// Out-of-memory class; no more protected than the parent's.
    pub class: OomClass,
    /// What restarts it after an out-of-memory stop, if anything.
    pub supervisor: Option<fn(&DomainRef)>,
}

impl Settings {
    /// Default weight, no limit, in `class`, unsupervised.
    pub const fn new(class: OomClass) -> Settings {
        Settings { weight: DOMAIN_WEIGHT_DEFAULT, limit_pages: None, class, supervisor: None }
    }
}

/// Why a domain operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DomainError {
    /// The charge would pass the limit of this domain or one above it.
    OverLimit,
    /// A weight out of range, a class more protected than the parent's,
    /// or a tree deeper than [`MAX_DEPTH`].
    InvalidArgument,
    /// No memory, or [`MAX_DOMAINS`] domains exist already.
    NoMemory,
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DomainError::OverLimit => "the domain's memory limit is reached",
            DomainError::InvalidArgument => "invalid domain settings",
            DomainError::NoMemory => "out of memory for domains",
        })
    }
}

/// A resource domain.
pub struct Domain {
    id: u32,
    name: Name,
    parent: Option<DomainRef>,
    depth: u8,
    class: OomClass,
    supervisor: Option<fn(&DomainRef)>,
    /// The capability object naming it, if it was made through one.
    object: Option<ObjectId>,
    weight: AtomicU32,
    limit: AtomicU64,
    /// Pages charged to it and its children.
    usage: AtomicU64,
    peak: AtomicU64,
    refused: AtomicU64,
    threads: AtomicU32,
    stopping: AtomicBool,
    oom_stops: AtomicU32,
}

impl Domain {
    /// Its number, unique for as long as the kernel runs.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Its name.
    pub fn name(&self) -> &Name {
        &self.name
    }

    /// The domain above it; `None` for the root.
    pub fn parent(&self) -> Option<&DomainRef> {
        self.parent.as_ref()
    }

    /// How far below the root it is.
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// Its out-of-memory class.
    pub fn class(&self) -> OomClass {
        self.class
    }

    /// What restarts it after an out-of-memory stop.
    pub fn supervisor(&self) -> Option<fn(&DomainRef)> {
        self.supervisor
    }

    /// Its CPU weight.
    pub fn weight(&self) -> u32 {
        self.weight.load(Ordering::Relaxed)
    }

    /// Changes its CPU weight; it applies when the run queues next
    /// recompute their shares.
    pub fn set_weight(&self, weight: u32) -> Result<(), DomainError> {
        if !(1..=DOMAIN_WEIGHT_MAX).contains(&weight) {
            return Err(DomainError::InvalidArgument);
        }
        self.weight.store(weight, Ordering::Relaxed);
        Ok(())
    }

    /// Its memory limit in pages (`u64::MAX` for none).
    pub fn limit_pages(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    /// Changes its memory limit. A limit below what is charged already
    /// refuses new charges until enough is given back.
    pub fn set_limit_pages(&self, pages: Option<u64>) {
        self.limit.store(pages.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Pages charged to it and its children.
    pub fn usage_pages(&self) -> u64 {
        self.usage.load(Ordering::Relaxed)
    }

    /// The most pages it has had charged at once.
    pub fn peak_pages(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    /// Charges it refused for its limit (or a limit above it).
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// Threads in it now.
    pub fn threads(&self) -> u32 {
        self.threads.load(Ordering::Relaxed)
    }

    /// How many times the out-of-memory policy has stopped it.
    pub fn oom_stops(&self) -> u32 {
        self.oom_stops.load(Ordering::Relaxed)
    }

    /// Whether it is being stopped by the out-of-memory policy.
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    /// Whether it is `other` or below it.
    pub fn is_within(&self, other: &Domain) -> bool {
        let mut at = Some(self);
        while let Some(domain) = at {
            if core::ptr::eq(domain, other) {
                return true;
            }
            at = domain.parent.as_deref();
        }
        false
    }

    /// Charges `pages` to it and every domain above it, unless that would
    /// pass one of their limits; then nothing is charged.
    pub fn charge(&self, pages: u64) -> Result<(), DomainError> {
        let mut at = Some(self);
        while let Some(domain) = at {
            let before = domain.usage.fetch_add(pages, Ordering::AcqRel);
            if before.saturating_add(pages) > domain.limit.load(Ordering::Relaxed) {
                domain.usage.fetch_sub(pages, Ordering::AcqRel);
                // Undo the domains below this one.
                let mut undo = Some(self);
                while let Some(below) = undo {
                    if core::ptr::eq(below, domain) {
                        break;
                    }
                    below.usage.fetch_sub(pages, Ordering::AcqRel);
                    undo = below.parent.as_deref();
                }
                self.refused.fetch_add(1, Ordering::Relaxed);
                return Err(DomainError::OverLimit);
            }
            domain.peak.fetch_max(before + pages, Ordering::Relaxed);
            at = domain.parent.as_deref();
        }
        Ok(())
    }

    /// Gives back `pages` charged with [`Domain::charge`].
    pub fn uncharge(&self, pages: u64) {
        let mut at = Some(self);
        while let Some(domain) = at {
            domain.usage.fetch_sub(pages, Ordering::AcqRel);
            at = domain.parent.as_deref();
        }
    }

    /// A thread joined it.
    pub(crate) fn thread_joined(&self) {
        self.threads.fetch_add(1, Ordering::Relaxed);
    }

    /// A thread in it exited (or was freed without running). Returns
    /// whether it was the domain's last.
    pub(crate) fn thread_left(&self) -> bool {
        self.threads.fetch_sub(1, Ordering::AcqRel) == 1
    }

    /// What to wait on (`sched::wait`) for its last thread to exit.
    pub fn key(&self) -> usize {
        self as *const Domain as usize
    }

    /// Marks it stopping (or not): from the out-of-memory policy, and from
    /// its supervisor once it has restarted it.
    pub(crate) fn set_stopping(&self, stopping: bool) {
        if stopping {
            self.oom_stops.fetch_add(1, Ordering::Relaxed);
        }
        self.stopping.store(stopping, Ordering::Release);
    }
}

/// Every domain, for the out-of-memory policy and the debugger.
static DOMAINS: SpinLock<Option<Vec<DomainRef, Kmalloc>>> = SpinLock::new(None);
/// The root domain.
static ROOT: Once<DomainRef> = Once::new();
static NEXT_ID: AtomicU32 = AtomicU32::new(0);

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

/// Makes the root domain. Call once, before the scheduler starts.
pub fn init() {
    let mut list = Vec::new_in(kmalloc());
    let root = Arc::try_new_in(
        new_domain(Name::new("system"), None, 0, Settings::new(OomClass::System), None),
        kmalloc(),
    );
    match (list.try_reserve_exact(MAX_DOMAINS), root) {
        (Ok(()), Ok(root)) => {
            let _ = list.push_within_capacity(root.clone());
            *DOMAINS.lock_irq() = Some(list);
            let _ = ROOT.set(root);
        }
        _ => panic!("Domains: no memory for the domain table at boot"),
    }
}

fn new_domain(
    name: Name,
    parent: Option<DomainRef>,
    depth: u8,
    settings: Settings,
    object: Option<ObjectId>,
) -> Domain {
    Domain {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        name,
        parent,
        depth,
        class: settings.class,
        supervisor: settings.supervisor,
        object,
        weight: AtomicU32::new(settings.weight),
        limit: AtomicU64::new(settings.limit_pages.unwrap_or(u64::MAX)),
        usage: AtomicU64::new(0),
        peak: AtomicU64::new(0),
        refused: AtomicU64::new(0),
        threads: AtomicU32::new(0),
        stopping: AtomicBool::new(false),
        oom_stops: AtomicU32::new(0),
    }
}

/// The root domain, `system`.
pub fn root() -> DomainRef {
    ROOT.get().expect("domains::init runs before anything asks for the root").clone()
}

/// Makes a domain below `parent`, named by capability object `object` if
/// a capability is made for it.
pub(crate) fn create(
    name: &str,
    parent: &DomainRef,
    settings: Settings,
    object: Option<ObjectId>,
) -> Result<DomainRef, DomainError> {
    if !(1..=DOMAIN_WEIGHT_MAX).contains(&settings.weight)
        || settings.class > parent.class
        || parent.depth >= MAX_DEPTH
    {
        return Err(DomainError::InvalidArgument);
    }
    let domain = Arc::try_new_in(
        new_domain(Name::new(name), Some(parent.clone()), parent.depth + 1, settings, object),
        kmalloc(),
    )
    .map_err(|_| DomainError::NoMemory)?;
    let mut list = DOMAINS.lock_irq();
    let list = list.as_mut().ok_or(DomainError::NoMemory)?;
    list.push_within_capacity(domain.clone()).map_err(|_| DomainError::NoMemory)?;
    Ok(domain)
}

/// The domain named by capability object `object`.
pub(crate) fn by_object(object: ObjectId) -> Option<DomainRef> {
    let list = DOMAINS.lock_irq();
    list.as_ref()?.iter().find(|d| d.object == Some(object)).cloned()
}

/// The capability store let go of `object`: the domain leaves the table.
/// It lives on while threads in it or domains below it hold it.
pub(crate) fn released(object: ObjectId) {
    let removed = {
        let mut list = DOMAINS.lock_irq();
        let Some(list) = list.as_mut() else { return };
        list.iter().position(|d| d.object == Some(object)).map(|index| list.swap_remove(index))
    };
    drop(removed);
}

/// Takes `domain`, made without a capability, off the table, so it goes
/// once the last thread and reference let go of it.
pub(crate) fn remove(domain: &DomainRef) {
    let removed = {
        let mut list = DOMAINS.lock_irq();
        let Some(list) = list.as_mut() else { return };
        list.iter().position(|d| DomainRef::ptr_eq(d, domain)).map(|index| list.swap_remove(index))
    };
    drop(removed);
}

/// Runs `f` on every domain in the table, with its lock held: `f` must not
/// take locks or allocate.
pub fn for_each(mut f: impl FnMut(&DomainRef)) {
    let list = DOMAINS.lock_irq();
    for domain in list.iter().flatten() {
        f(domain);
    }
}

/// Like [`for_each`], but gives up if another CPU holds the table: for
/// `kdb`.
pub fn try_for_each(mut f: impl FnMut(&DomainRef)) -> bool {
    let Some(list) = DOMAINS.try_lock_irq() else { return false };
    for domain in list.iter().flatten() {
        f(domain);
    }
    true
}

/// Writes the domain table, for `kdb`.
pub fn write_table(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
    line(format_args!(
        "Domains (class, CPU weight, pages used / limit, peak, refused charges, threads, \
         out-of-memory stops):"
    ));
    let shown = try_for_each(|d| {
        let mut limit = radix_klog::TextBuf::<24>::new();
        let _ = match d.limit_pages() {
            u64::MAX => fmt::Write::write_str(&mut limit, "none"),
            pages => fmt::Write::write_fmt(&mut limit, format_args!("{pages}")),
        };
        let mut name = radix_klog::TextBuf::<32>::new();
        for _ in 0..d.depth() {
            name.push_str("  ");
        }
        name.push_str(d.name().as_str());
        line(format_args!(
            "  {:>3} {:<24} {:<8} {:>5} {:>8} / {:<8} {:>8} {:>6} {:>4} {:>3}{}",
            d.id(),
            name,
            d.class().name(),
            d.weight(),
            d.usage_pages(),
            limit,
            d.peak_pages(),
            d.refused(),
            d.threads(),
            d.oom_stops(),
            if d.is_stopping() { " stopping" } else { "" }
        ));
    });
    if !shown {
        line(format_args!("  another CPU holds the domain table"));
    }
}
