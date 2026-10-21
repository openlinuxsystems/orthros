// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Sets of CPUs, for affinity masks and topology spans.

use core::fmt;

/// Most CPUs a set can hold (as `radix_hal::MAX_CPUS`).
pub const MAX_CPUS: usize = 256;

/// The 64-bit words in a set's representation ([`CpuSet::words`]).
pub const WORDS: usize = MAX_CPUS / 64;

/// A set of CPU indices below [`MAX_CPUS`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CpuSet([u64; WORDS]);

impl CpuSet {
    /// No CPUs.
    pub const EMPTY: CpuSet = CpuSet([0; WORDS]);

    /// CPUs 0 to `n - 1`.
    pub const fn first(n: usize) -> CpuSet {
        let mut words = [0u64; WORDS];
        let mut i = 0;
        while i < WORDS {
            let start = i * 64;
            words[i] = if n >= start + 64 {
                u64::MAX
            } else if n > start {
                (1u64 << (n - start)) - 1
            } else {
                0
            };
            i += 1;
        }
        CpuSet(words)
    }

    /// CPU `cpu` alone (no CPU if it is out of range).
    pub const fn single(cpu: usize) -> CpuSet {
        let mut words = [0u64; WORDS];
        if cpu < MAX_CPUS {
            words[cpu / 64] = 1 << (cpu % 64);
        }
        CpuSet(words)
    }

    /// The set with these bits, 64 CPUs per word.
    pub const fn from_words(words: [u64; WORDS]) -> CpuSet {
        CpuSet(words)
    }

    /// The bits, 64 CPUs per word.
    pub const fn words(&self) -> [u64; WORDS] {
        self.0
    }

    /// Whether `cpu` is in the set.
    pub const fn contains(&self, cpu: usize) -> bool {
        cpu < MAX_CPUS && self.0[cpu / 64] & (1 << (cpu % 64)) != 0
    }

    /// Adds `cpu` (ignored if out of range).
    pub fn insert(&mut self, cpu: usize) {
        if cpu < MAX_CPUS {
            self.0[cpu / 64] |= 1 << (cpu % 64);
        }
    }

    /// Removes `cpu`.
    pub fn remove(&mut self, cpu: usize) {
        if cpu < MAX_CPUS {
            self.0[cpu / 64] &= !(1 << (cpu % 64));
        }
    }

    /// The CPUs in both sets.
    #[must_use]
    pub const fn and(&self, other: &CpuSet) -> CpuSet {
        let mut words = [0u64; WORDS];
        let mut i = 0;
        while i < WORDS {
            words[i] = self.0[i] & other.0[i];
            i += 1;
        }
        CpuSet(words)
    }

    /// The CPUs in either set.
    #[must_use]
    pub const fn or(&self, other: &CpuSet) -> CpuSet {
        let mut words = [0u64; WORDS];
        let mut i = 0;
        while i < WORDS {
            words[i] = self.0[i] | other.0[i];
            i += 1;
        }
        CpuSet(words)
    }

    /// The CPUs in this set but not in `other`.
    #[must_use]
    pub const fn minus(&self, other: &CpuSet) -> CpuSet {
        let mut words = [0u64; WORDS];
        let mut i = 0;
        while i < WORDS {
            words[i] = self.0[i] & !other.0[i];
            i += 1;
        }
        CpuSet(words)
    }

    /// Whether the set is empty.
    pub const fn is_empty(&self) -> bool {
        let mut i = 0;
        while i < WORDS {
            if self.0[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Whether every CPU in this set is in `other`.
    pub const fn is_subset(&self, other: &CpuSet) -> bool {
        self.minus(other).is_empty()
    }

    /// How many CPUs are in the set.
    pub const fn len(&self) -> usize {
        let mut count = 0;
        let mut i = 0;
        while i < WORDS {
            count += self.0[i].count_ones() as usize;
            i += 1;
        }
        count
    }

    /// The lowest CPU in the set.
    pub fn lowest(&self) -> Option<usize> {
        self.iter().next()
    }

    /// The CPUs in the set, lowest first.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(word, &bits)| {
            let mut bits = bits;
            core::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(word * 64 + bit)
            })
        })
    }
}

impl fmt::Display for CpuSet {
    /// Ranges, as in `0-3,6`; `none` for the empty set.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let mut first = true;
        let mut cpus = self.iter().peekable();
        while let Some(start) = cpus.next() {
            let mut end = start;
            while cpus.peek() == Some(&(end + 1)) {
                end += 1;
                cpus.next();
            }
            if !first {
                f.write_str(",")?;
            }
            first = false;
            if end == start {
                write!(f, "{start}")?;
            } else {
                write!(f, "{start}-{end}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for CpuSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CpuSet({self})")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn sets_print_as_ranges() {
        assert_eq!(CpuSet::EMPTY.to_string(), "none");
        assert_eq!(CpuSet::first(4).to_string(), "0-3");
        let mut set = CpuSet::first(2);
        set.insert(5);
        set.insert(6);
        set.insert(255);
        assert_eq!(set.to_string(), "0-1,5-6,255");
        assert_eq!(CpuSet::first(MAX_CPUS).len(), MAX_CPUS);
        assert_eq!(CpuSet::first(65).len(), 65);
        assert_eq!(CpuSet::single(300), CpuSet::EMPTY);
    }

    proptest! {
        #[test]
        fn sets_behave_like_a_model(ops in prop::collection::vec((any::<bool>(), 0usize..300), 0..64),
                                    other in prop::collection::btree_set(0usize..256, 0..32)) {
            let mut set = CpuSet::EMPTY;
            let mut model = BTreeSet::new();
            for (insert, cpu) in ops {
                if insert {
                    set.insert(cpu);
                    if cpu < MAX_CPUS {
                        model.insert(cpu);
                    }
                } else {
                    set.remove(cpu);
                    model.remove(&cpu);
                }
            }
            prop_assert_eq!(set.iter().collect::<Vec<_>>(), model.iter().copied().collect::<Vec<_>>());
            prop_assert_eq!(set.len(), model.len());
            prop_assert_eq!(set.lowest(), model.first().copied());
            let mut other_set = CpuSet::EMPTY;
            for &cpu in &other {
                other_set.insert(cpu);
            }
            let both: Vec<_> = model.intersection(&other).copied().collect();
            prop_assert_eq!(set.and(&other_set).iter().collect::<Vec<_>>(), both);
            let either: Vec<_> = model.union(&other).copied().collect();
            prop_assert_eq!(set.or(&other_set).iter().collect::<Vec<_>>(), either);
            let only: Vec<_> = model.difference(&other).copied().collect();
            prop_assert_eq!(set.minus(&other_set).iter().collect::<Vec<_>>(), only);
            prop_assert!(set.and(&other_set).is_subset(&set));
        }
    }
}
