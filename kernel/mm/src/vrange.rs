// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Virtual address ranges: the kernel's mapping window.
//!
//! Kernel stacks, device memory (MMIO) and other areas that need virtually
//! contiguous memory are mapped into one window of the kernel's address
//! space. [`Ranges`] hands out page-aligned pieces of it: first fit over a
//! sorted table of free extents, merged again on release. Every area has
//! an unmapped guard page below it (the caller asks for it as part of the
//! length), so a stack growing down past its end faults instead of
//! overwriting the area below.

#![forbid(unsafe_code)]

use crate::page::PAGE_SIZE;

/// Most free extents the table holds. Releasing a range that would need
/// more fails, and the range stays allocated.
pub const EXTENTS: usize = 256;

/// Why a range could not be released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeError {
    /// The range is not page-aligned, is empty, or lies outside the window.
    Invalid,
    /// Part of the range is already free.
    AlreadyFree,
    /// The table of free extents is full.
    TooFragmented,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extent {
    start: u64,
    end: u64,
}

/// Free page-aligned ranges in a window of addresses, in a table of at
/// most `N` extents (generic so that the proofs can use a small table).
pub struct RangeTable<const N: usize> {
    window: Extent,
    free: [Extent; N],
    len: usize,
}

/// The kernel's mapping-window allocator.
pub type Ranges = RangeTable<EXTENTS>;

impl<const N: usize> RangeTable<N> {
    /// An allocator with nothing to hand out.
    pub const fn new() -> Self {
        RangeTable {
            window: Extent { start: 0, end: 0 },
            free: [Extent { start: 0, end: 0 }; N],
            len: 0,
        }
    }

    /// Makes `start..end` (page-aligned) the window, all of it free.
    pub fn init(&mut self, start: u64, end: u64) {
        let page = PAGE_SIZE as u64;
        let (start, end) = (start.next_multiple_of(page), end - end % page);
        self.window = Extent { start, end: end.max(start) };
        self.free[0] = self.window;
        self.len = usize::from(end > start);
    }

    /// The window.
    pub fn window(&self) -> (u64, u64) {
        (self.window.start, self.window.end)
    }

    /// Bytes free.
    pub fn free_bytes(&self) -> u64 {
        self.free[..self.len].iter().map(|e| e.end - e.start).sum()
    }

    /// Takes `bytes` (rounded up to pages) aligned to `align` (a power of
    /// two, at least a page) and returns the start.
    pub fn alloc(&mut self, bytes: u64, align: u64) -> Option<u64> {
        let page = PAGE_SIZE as u64;
        let bytes = bytes.checked_next_multiple_of(page).filter(|&b| b > 0)?;
        let align = align.max(page);
        if !align.is_power_of_two() {
            return None;
        }
        let index = (0..self.len).find(|&i| {
            let e = self.free[i];
            e.start
                .checked_next_multiple_of(align)
                .and_then(|s| s.checked_add(bytes))
                .is_some_and(|end| end <= e.end)
        })?;
        let Extent { start, end } = self.free[index];
        let at = start.next_multiple_of(align);
        let (before, after) = (Extent { start, end: at }, Extent { start: at + bytes, end });
        match (before.start < before.end, after.start < after.end) {
            (false, false) => self.remove(index),
            (true, false) => self.free[index] = before,
            (false, true) => self.free[index] = after,
            (true, true) => {
                if self.len == N {
                    return None;
                }
                self.free[index] = before;
                self.insert(index + 1, after);
            }
        }
        Some(at)
    }

    /// Gives back `bytes` (rounded up to pages) at `start`.
    pub fn release(&mut self, start: u64, bytes: u64) -> Result<(), RangeError> {
        let page = PAGE_SIZE as u64;
        let end = bytes
            .checked_next_multiple_of(page)
            .and_then(|b| start.checked_add(b))
            .filter(|&end| end > start)
            .ok_or(RangeError::Invalid)?;
        if !start.is_multiple_of(page) || start < self.window.start || end > self.window.end {
            return Err(RangeError::Invalid);
        }
        // The first extent that starts at or after `start`.
        let index = self.free[..self.len].partition_point(|e| e.start < start);
        let before = index.checked_sub(1).map(|i| self.free[i]);
        let after = (index < self.len).then(|| self.free[index]);
        if before.is_some_and(|b| b.end > start) || after.is_some_and(|a| a.start < end) {
            return Err(RangeError::AlreadyFree);
        }
        match (before.filter(|b| b.end == start), after.filter(|a| a.start == end)) {
            (Some(_), Some(a)) => {
                self.free[index - 1].end = a.end;
                self.remove(index);
            }
            (Some(_), None) => self.free[index - 1].end = end,
            (None, Some(_)) => self.free[index].start = start,
            (None, None) => {
                if self.len == N {
                    return Err(RangeError::TooFragmented);
                }
                self.insert(index, Extent { start, end });
            }
        }
        Ok(())
    }

    fn remove(&mut self, index: usize) {
        self.free.copy_within(index + 1..self.len, index);
        self.len -= 1;
    }

    fn insert(&mut self, index: usize, extent: Extent) {
        self.free.copy_within(index..self.len, index + 1);
        self.free[index] = extent;
        self.len += 1;
    }

    /// Checks that the free extents are sorted, disjoint, not touching and
    /// inside the window.
    pub fn check(&self) -> Result<(), &'static str> {
        let mut last = None;
        for e in &self.free[..self.len] {
            if e.start >= e.end || e.start < self.window.start || e.end > self.window.end {
                return Err("an extent is empty or outside the window");
            }
            if last.is_some_and(|end| end >= e.start) {
                return Err("extents overlap, touch or are out of order");
            }
            last = Some(e.end);
        }
        Ok(())
    }
}

impl<const N: usize> Default for RangeTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const P: u64 = PAGE_SIZE as u64;

    #[test]
    fn hands_out_and_merges() {
        let mut ranges = Ranges::new();
        ranges.init(0x1000_0000, 0x1010_0000);
        let a = ranges.alloc(P, P).unwrap();
        let b = ranges.alloc(3 * P, 4 * P).unwrap();
        assert_eq!(a, 0x1000_0000);
        assert_eq!(b, 0x1000_4000);
        assert_eq!(ranges.release(a, P), Ok(()));
        assert_eq!(ranges.release(a, P), Err(RangeError::AlreadyFree));
        assert_eq!(ranges.release(a + P, P), Err(RangeError::AlreadyFree));
        assert_eq!(ranges.release(b, 3 * P), Ok(()));
        assert_eq!(ranges.free_bytes(), 0x10_0000);
        assert_eq!(ranges.len, 1);
        assert_eq!(ranges.release(0x2000_0000, P), Err(RangeError::Invalid));
        assert_eq!(ranges.alloc(0x20_0000, P), None);
        ranges.check().unwrap();
    }

    proptest! {
        #[test]
        fn ranges_never_overlap(ops in proptest::collection::vec((any::<bool>(), 1u64..16, 0u32..4, any::<usize>()), 1..200)) {
            let mut ranges = Ranges::new();
            ranges.init(0x10_0000, 0x10_0000 + 256 * P);
            let mut taken: Vec<(u64, u64)> = Vec::new();
            for (alloc, pages, shift, pick) in ops {
                if alloc || taken.is_empty() {
                    let align = P << shift;
                    if let Some(start) = ranges.alloc(pages * P, align) {
                        prop_assert!(start.is_multiple_of(align));
                        let end = start + pages * P;
                        prop_assert!(taken.iter().all(|&(s, e)| end <= s || e <= start));
                        taken.push((start, end));
                    }
                } else {
                    let (start, end) = taken.swap_remove(pick % taken.len());
                    prop_assert_eq!(ranges.release(start, end - start), Ok(()));
                }
                ranges.check().unwrap();
                let used: u64 = taken.iter().map(|(s, e)| e - s).sum();
                prop_assert_eq!(ranges.free_bytes() + used, 256 * P);
            }
        }
    }
}
