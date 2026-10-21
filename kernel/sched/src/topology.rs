//! The CPU topology, and the decisions that depend on it: where a waking
//! thread should run, and which CPU an underloaded one should pull from.
//!
//! The kernel describes each CPU by its core (hardware threads on one core
//! share it), its package (whose cores share the last-level cache) and its
//! NUMA node, and gives the distances between nodes (ACPI's SLIT; 10 is
//! local). From that come the *levels* of each CPU: the CPUs sharing its
//! core, its package, its node, and the whole machine. Each level holds
//! the one below: a package that spans nodes (AMD's NPS modes, or a
//! virtual machine's made-up topology) counts as one package per node,
//! since its last-level caches do not span nodes either. Placement and
//! balancing walk the levels from the closest out, so a thread moves to a
//! CPU that shares its caches before one that does not, and to a near node
//! before a far one. Moving between nodes also waits longer between
//! attempts ([`interval_ticks`]) and prefers threads whose caches have gone
//! cold ([`is_cache_hot`]).
//!
//! Loads are counts of runnable threads; weights are left to the fair queues
//! on each CPU. Only threads waiting in a queue move: a thread alone on its
//! CPU stays where it is.

use crate::cpuset::{CpuSet, MAX_CPUS};

/// Most NUMA nodes (as `radix_mm::MAX_NODES`).
pub const MAX_NODES: usize = 8;

/// A topology level, closest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Hardware threads of one core.
    Core,
    /// Cores of one package, which share its last-level cache.
    Package,
    /// CPUs of one NUMA node.
    Node,
    /// Every CPU.
    Machine,
}

impl Level {
    /// Every level, closest first.
    pub const ALL: [Level; 4] = [Level::Core, Level::Package, Level::Node, Level::Machine];

    /// Lower-case name.
    pub const fn name(self) -> &'static str {
        match self {
            Level::Core => "core",
            Level::Package => "package",
            Level::Node => "node",
            Level::Machine => "machine",
        }
    }

    const fn lower(self) -> Option<Level> {
        match self {
            Level::Core => None,
            Level::Package => Some(Level::Core),
            Level::Node => Some(Level::Package),
            Level::Machine => Some(Level::Node),
        }
    }
}

/// Where one CPU sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Place {
    /// Its package.
    pub package: u32,
    /// Its core, unique within the package.
    pub core: u32,
    /// Its NUMA node.
    pub node: u8,
}

/// How busy one CPU is, as a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CpuLoad {
    /// Runnable threads, the running one included; 0 when idle.
    pub running: u32,
    /// The real-time priority of the running thread, 0 when it is fair or
    /// the CPU is idle.
    pub rt_prio: u8,
}

/// How many timer ticks pass between balancing attempts at `level` on a
/// busy CPU. An idle CPU tries every level at every tick.
pub const fn interval_ticks(level: Level) -> u64 {
    match level {
        Level::Core => 1,
        Level::Package => 2,
        Level::Node => 8,
        Level::Machine => 16,
    }
}

/// Whether a thread that last ran `since_ns` ago probably still has its
/// data in the caches that moving it across `level` would leave behind.
pub const fn is_cache_hot(since_ns: u64, level: Level) -> bool {
    let window = match level {
        Level::Core => 0,
        Level::Package => 500_000,
        Level::Node | Level::Machine => 5_000_000,
    };
    since_ns < window
}

/// Whether moving one thread from a group of `from_cpus` CPUs running
/// `from` threads to a group of `to_cpus` running `to` makes the load more
/// even. It never reverses the imbalance, so two groups cannot pass a
/// thread back and forth (proved in `proofs.rs`).
pub const fn should_pull(from: u32, from_cpus: u32, to: u32, to_cpus: u32) -> bool {
    if from == 0 || from_cpus == 0 || to_cpus == 0 {
        return false;
    }
    (from as u64 - 1) * to_cpus as u64 >= (to as u64 + 1) * from_cpus as u64
}

/// The machine's CPUs, packages and nodes.
#[derive(Clone)]
pub struct Topology {
    cpus: usize,
    places: [Place; MAX_CPUS],
    distance: [[u8; MAX_NODES]; MAX_NODES],
}

impl Topology {
    /// `cpus` CPUs, each its own core in one package on one node.
    pub const fn flat(cpus: usize) -> Topology {
        let cpus = if cpus > MAX_CPUS { MAX_CPUS } else { cpus };
        let mut places = [Place { package: 0, core: 0, node: 0 }; MAX_CPUS];
        let mut i = 0;
        while i < MAX_CPUS {
            places[i].core = i as u32;
            i += 1;
        }
        let mut distance = [[20u8; MAX_NODES]; MAX_NODES];
        let mut n = 0;
        while n < MAX_NODES {
            distance[n][n] = 10;
            n += 1;
        }
        Topology { cpus, places, distance }
    }

    /// Sets how many CPUs there are.
    pub fn set_cpus(&mut self, cpus: usize) {
        self.cpus = cpus.min(MAX_CPUS);
    }

    /// Records where CPU `cpu` sits.
    pub fn set_place(&mut self, cpu: usize, place: Place) {
        if let Some(slot) = self.places.get_mut(cpu) {
            *slot = Place { node: place.node.min(MAX_NODES as u8 - 1), ..place };
        }
    }

    /// Records the distance between two nodes (10 is local).
    pub fn set_distance(&mut self, from: usize, to: usize, distance: u8) {
        if from < MAX_NODES && to < MAX_NODES {
            self.distance[from][to] = distance;
        }
    }

    /// How many CPUs there are.
    pub fn cpus(&self) -> usize {
        self.cpus
    }

    /// Where CPU `cpu` sits.
    pub fn place(&self, cpu: usize) -> Place {
        self.places.get(cpu).copied().unwrap_or_default()
    }

    /// The distance from CPU `a`'s node to CPU `b`'s.
    pub fn cpu_distance(&self, a: usize, b: usize) -> u8 {
        let (a, b) = (self.place(a).node as usize, self.place(b).node as usize);
        self.distance[a][b]
    }

    fn shares(&self, a: usize, b: usize, level: Level) -> bool {
        let (pa, pb) = (self.place(a), self.place(b));
        let package = pa.package == pb.package && pa.node == pb.node;
        match level {
            Level::Core => package && pa.core == pb.core,
            Level::Package => package,
            Level::Node => pa.node == pb.node,
            Level::Machine => true,
        }
    }

    /// The CPUs that share `level` with `cpu`.
    pub fn span(&self, cpu: usize, level: Level) -> CpuSet {
        let mut set = CpuSet::EMPTY;
        for other in 0..self.cpus {
            if self.shares(cpu, other, level) {
                set.insert(other);
            }
        }
        set
    }

    /// The levels of `cpu` that add CPUs to the one below, closest first.
    pub fn levels(&self, cpu: usize) -> impl Iterator<Item = Level> + '_ {
        let mut below = CpuSet::single(cpu);
        Level::ALL.into_iter().filter(move |&level| {
            let span = self.span(cpu, level).or(&below);
            let grows = span.len() > below.len();
            below = span;
            grows
        })
    }

    /// The CPU an underloaded `this` should pull a thread from, comparing
    /// the groups that make up its span at `level`. `None` when the load
    /// there is even, or no CPU has a thread waiting.
    pub fn find_busiest(&self, this: usize, level: Level, loads: &[CpuLoad]) -> Option<usize> {
        let span = self.span(this, level);
        let group_of = |cpu: usize| match level.lower() {
            Some(lower) => self.span(cpu, lower).and(&span),
            None => CpuSet::single(cpu),
        };
        let sum = |group: &CpuSet| -> (u32, u32) {
            let running = group.iter().map(|c| loads.get(c).map_or(0, |l| l.running)).sum();
            (running, group.len() as u32)
        };
        let local = group_of(this);
        let (to, to_cpus) = sum(&local);
        let mut seen = local;
        let mut best: Option<(CpuSet, u32, u32, u8)> = None;
        for cpu in span.iter() {
            if seen.contains(cpu) {
                continue;
            }
            let group = group_of(cpu);
            seen = seen.or(&group);
            let (from, from_cpus) = sum(&group);
            let waiting = group.iter().any(|c| loads.get(c).is_some_and(|l| l.running >= 2));
            if !waiting || !should_pull(from, from_cpus, to, to_cpus) {
                continue;
            }
            let distance = self.cpu_distance(this, cpu);
            let better = match best {
                None => true,
                // Busier per CPU first, then nearer.
                Some((_, b, b_cpus, b_distance)) => {
                    let (mine, theirs) =
                        (u64::from(from) * u64::from(b_cpus), u64::from(b) * u64::from(from_cpus));
                    mine > theirs || (mine == theirs && distance < b_distance)
                }
            };
            if better {
                best = Some((group, from, from_cpus, distance));
            }
        }
        let (group, ..) = best?;
        group
            .iter()
            .filter(|&c| loads.get(c).is_some_and(|l| l.running >= 2))
            .max_by_key(|&c| (loads[c].running, core::cmp::Reverse(c)))
    }

    /// Where a waking fair thread that last ran on `prev` should run: an
    /// idle CPU as close to `prev` as possible, else `prev`, else the least
    /// busy CPU it may use. `allowed` must not be empty.
    pub fn select_cpu(&self, prev: usize, allowed: &CpuSet, loads: &[CpuLoad]) -> usize {
        let idle =
            |cpu: usize| allowed.contains(cpu) && loads.get(cpu).is_some_and(|l| l.running == 0);
        if idle(prev) {
            return prev;
        }
        if let Some(cpu) = self.nearest(prev, allowed, idle) {
            return cpu;
        }
        if allowed.contains(prev) && prev < self.cpus {
            return prev;
        }
        self.least_busy(prev, allowed, loads)
    }

    /// Where a waking real-time thread of priority `prio` should run: an
    /// idle CPU near `prev`, else `prev` if it runs something less urgent,
    /// else the CPU running the least urgent thing below `prio`, else
    /// `prev`. `allowed` must not be empty.
    pub fn select_rt_cpu(
        &self,
        prev: usize,
        prio: u8,
        allowed: &CpuSet,
        loads: &[CpuLoad],
    ) -> usize {
        let idle =
            |cpu: usize| allowed.contains(cpu) && loads.get(cpu).is_some_and(|l| l.running == 0);
        if idle(prev) {
            return prev;
        }
        if let Some(cpu) = self.nearest(prev, allowed, idle) {
            return cpu;
        }
        let below = |cpu: usize| loads.get(cpu).map_or(u8::MAX, |l| l.rt_prio);
        if allowed.contains(prev) && below(prev) < prio {
            return prev;
        }
        let lowest = allowed
            .iter()
            .filter(|&c| c < self.cpus && below(c) < prio)
            .min_by_key(|&c| (below(c), self.cpu_distance(prev, c), c));
        match lowest {
            Some(cpu) => cpu,
            None if allowed.contains(prev) && prev < self.cpus => prev,
            None => self.least_busy(prev, allowed, loads),
        }
    }

    /// The CPU satisfying `want` that is closest to `cpu`, by level and
    /// then by node distance.
    fn nearest(&self, cpu: usize, allowed: &CpuSet, want: impl Fn(usize) -> bool) -> Option<usize> {
        for level in self.levels(cpu) {
            let found = self
                .span(cpu, level)
                .and(allowed)
                .iter()
                .filter(|&c| want(c))
                .min_by_key(|&c| (self.cpu_distance(cpu, c), c));
            if found.is_some() {
                return found;
            }
        }
        None
    }

    fn least_busy(&self, prev: usize, allowed: &CpuSet, loads: &[CpuLoad]) -> usize {
        allowed
            .iter()
            .filter(|&c| c < self.cpus)
            .min_by_key(|&c| {
                (loads.get(c).map_or(u32::MAX, |l| l.running), self.cpu_distance(prev, c), c)
            })
            .or_else(|| allowed.lowest())
            .unwrap_or(prev)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Two packages of two cores with two hardware threads each; one node
    /// per package.
    fn machine() -> Topology {
        let mut topology = Topology::flat(8);
        for cpu in 0..8 {
            topology.set_place(
                cpu,
                Place {
                    package: (cpu / 4) as u32,
                    core: ((cpu / 2) % 2) as u32,
                    node: (cpu / 4) as u8,
                },
            );
        }
        topology.set_distance(0, 1, 21);
        topology.set_distance(1, 0, 21);
        topology
    }

    fn loads(running: &[u32]) -> Vec<CpuLoad> {
        running.iter().map(|&r| CpuLoad { running: r, rt_prio: 0 }).collect()
    }

    #[test]
    fn levels_grow_outwards() {
        let topology = machine();
        assert_eq!(topology.span(0, Level::Core).to_string(), "0-1");
        assert_eq!(topology.span(5, Level::Package).to_string(), "4-7");
        assert_eq!(topology.span(5, Level::Node).to_string(), "4-7");
        assert_eq!(topology.span(5, Level::Machine).to_string(), "0-7");
        // The node adds nothing to the package here.
        assert_eq!(
            topology.levels(0).collect::<Vec<_>>(),
            [Level::Core, Level::Package, Level::Machine]
        );
        // One package over two nodes counts as one per node.
        let mut split = Topology::flat(4);
        for cpu in 0..4 {
            split.set_place(cpu, Place { package: 0, core: cpu as u32, node: (cpu / 2) as u8 });
        }
        assert_eq!(split.span(3, Level::Package).to_string(), "2-3");
        assert_eq!(split.levels(3).collect::<Vec<_>>(), [Level::Package, Level::Machine]);
        let flat = Topology::flat(4);
        assert_eq!(flat.levels(2).collect::<Vec<_>>(), [Level::Package]);
        assert_eq!(Topology::flat(1).levels(0).count(), 0);
    }

    #[test]
    fn pulling_never_reverses_the_imbalance() {
        assert!(should_pull(2, 1, 0, 1));
        assert!(!should_pull(2, 1, 1, 1));
        assert!(should_pull(3, 2, 1, 2));
        assert!(!should_pull(3, 2, 2, 2));
        assert!(!should_pull(0, 1, 0, 1));
        // Every small case. The general case is two lines of algebra
        // (docs/design/scheduler.md): a pull needs (from - 1) t >= (to + 1) f,
        // the pull back would need to f >= from t, and the first gives
        // from t >= to f + f + t > to f.
        for from in 0..=48u32 {
            for to in 0..=48u32 {
                for from_cpus in 1..=12u32 {
                    for to_cpus in 1..=12u32 {
                        if should_pull(from, from_cpus, to, to_cpus) {
                            assert!(!should_pull(to + 1, to_cpus, from - 1, from_cpus));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn busiest_prefers_threads_that_are_waiting() {
        let topology = machine();
        // CPU 1, the core sibling, has a thread waiting.
        assert_eq!(
            topology.find_busiest(0, Level::Core, &loads(&[0, 2, 0, 0, 0, 0, 0, 0])),
            Some(1)
        );
        // One thread each on CPUs 2 and 3: nothing is waiting, so nothing
        // moves, although the package is uneven.
        assert_eq!(
            topology.find_busiest(0, Level::Package, &loads(&[0, 0, 1, 1, 0, 0, 0, 0])),
            None
        );
        assert_eq!(
            topology.find_busiest(0, Level::Package, &loads(&[0, 0, 3, 1, 0, 0, 0, 0])),
            Some(2)
        );
        // The other package is busier than this one.
        let far = loads(&[1, 1, 1, 1, 3, 2, 2, 2]);
        assert_eq!(topology.find_busiest(0, Level::Machine, &far), Some(4));
        assert_eq!(topology.find_busiest(0, Level::Package, &far), None);
    }

    #[test]
    fn waking_threads_stay_close() {
        let topology = machine();
        let all = CpuSet::first(8);
        assert_eq!(topology.select_cpu(0, &all, &loads(&[0; 8])), 0);
        assert_eq!(topology.select_cpu(0, &all, &loads(&[1, 1, 1, 0, 0, 0, 0, 0])), 3);
        assert_eq!(topology.select_cpu(0, &all, &loads(&[1, 1, 1, 1, 0, 1, 0, 1])), 4);
        assert_eq!(topology.select_cpu(0, &all, &loads(&[1; 8])), 0);
        let only = CpuSet::single(6);
        assert_eq!(topology.select_cpu(0, &only, &loads(&[0; 8])), 6);
    }

    #[test]
    fn real_time_threads_find_the_least_urgent_cpu() {
        let topology = machine();
        let all = CpuSet::first(4);
        let busy = |prios: [u8; 4]| {
            prios.iter().map(|&p| CpuLoad { running: 1, rt_prio: p }).collect::<Vec<_>>()
        };
        assert_eq!(topology.select_rt_cpu(0, 50, &all, &busy([0, 0, 0, 0])), 0);
        assert_eq!(topology.select_rt_cpu(0, 50, &all, &busy([60, 10, 0, 70])), 2);
        assert_eq!(topology.select_rt_cpu(0, 50, &all, &busy([60, 70, 80, 90])), 0);
        let mut mixed = busy([60, 70, 80, 90]);
        mixed[3].running = 0;
        assert_eq!(topology.select_rt_cpu(0, 50, &all, &mixed), 3);
    }

    proptest! {
        #[test]
        fn placement_respects_affinity(prev in 0usize..8, words in any::<u64>(), running in prop::collection::vec(0u32..4, 8)) {
            let topology = machine();
            let allowed = CpuSet::from_words([words & 0xff, 0, 0, 0]);
            prop_assume!(!allowed.is_empty());
            let loads = loads(&running);
            prop_assert!(allowed.contains(topology.select_cpu(prev, &allowed, &loads)));
            prop_assert!(allowed.contains(topology.select_rt_cpu(prev, 40, &allowed, &loads)));
        }

        #[test]
        fn pulling_evens_without_reversing(from in 0u32..64, from_cpus in 1u32..8, to in 0u32..64, to_cpus in 1u32..8) {
            if should_pull(from, from_cpus, to, to_cpus) {
                // After the move the source is still at least as busy per
                // CPU as the destination.
                prop_assert!(u64::from(from - 1) * u64::from(to_cpus) >= u64::from(to + 1) * u64::from(from_cpus));
                prop_assert!(!should_pull(to + 1, to_cpus, from - 1, from_cpus));
            }
        }
    }
}
