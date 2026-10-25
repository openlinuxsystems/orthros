// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The heap's size classes.
//!
//! Requests up to [`MAX_SMALL`] bytes are rounded up to one of
//! [`CLASSES`] sizes, a quarter of a power of two apart above 64 bytes, so
//! less than a fifth of an object is wasted on rounding there. The
//! smallest classes are 16 bytes apart, so up to 15 bytes are: nearly half
//! of a 32-byte object. Each class has its own slab cache. An object's
//! alignment is the largest power of two dividing its size (capped at a
//! page), because slabs start on a page boundary and objects follow each
//! other without gaps; a request with a stricter alignment moves up to the
//! first class that provides it.
//!
//! A slab is a block of 2^order frames: the smallest that holds at least
//! eight objects, up to 32 KiB.

#![forbid(unsafe_code)]

use crate::page::PAGE_SIZE;

/// Number of size classes.
pub const CLASSES: usize = 32;
/// Largest request served from a slab cache; larger ones take whole blocks
/// of frames.
pub const MAX_SMALL: usize = 8192;
/// Largest slab order.
pub const MAX_SLAB_ORDER: u32 = 3;

/// The object size of each class, in bytes.
pub const SIZES: [u16; CLASSES] = [
    16, 32, 48, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 640, 768, 896, 1024,
    1280, 1536, 1792, 2048, 2560, 3072, 3584, 4096, 5120, 6144, 7168, 8192,
];

/// For each multiple of 16 bytes up to [`MAX_SMALL`], the smallest class
/// that holds it.
const BY_SIZE: [u8; MAX_SMALL / 16 + 1] = {
    let mut table = [0u8; MAX_SMALL / 16 + 1];
    let mut class = 0;
    let mut slot = 0;
    while slot < table.len() {
        while (SIZES[class] as usize) < slot * 16 {
            class += 1;
        }
        table[slot] = class as u8;
        slot += 1;
    }
    table
};

/// The order of each class's slabs: the smallest that holds at least
/// eight objects, up to [`MAX_SLAB_ORDER`].
const SLAB_ORDERS: [u8; CLASSES] = {
    let mut table = [0u8; CLASSES];
    let mut class = 0;
    while class < CLASSES {
        let mut order = 0;
        while order < MAX_SLAB_ORDER && (PAGE_SIZE << order) / (SIZES[class] as usize) < 8 {
            order += 1;
        }
        table[class] = order as u8;
        class += 1;
    }
    table
};

/// The objects in each class's slab.
const OBJECTS: [u16; CLASSES] = {
    let mut table = [0u16; CLASSES];
    let mut class = 0;
    while class < CLASSES {
        table[class] = ((PAGE_SIZE << SLAB_ORDERS[class]) / SIZES[class] as usize) as u16;
        class += 1;
    }
    table
};

/// For each class, 2^32 / size rounded down, plus one: `(offset * it) >>
/// 32` is `offset / size` for every offset within a slab, so finding an
/// object's index on every free takes a multiplication, not a division.
/// The product exceeds `offset / size` by less than `offset / 2^32`, under
/// 2^-17 for the at most 2^15 bytes of a slab, while the fraction of
/// `offset / size` is at most `1 - 1 / size`, and `1 / size` is at least
/// 2^-13: the integer part stays the same.
const RECIPROCALS: [u64; CLASSES] = {
    let mut table = [0u64; CLASSES];
    let mut class = 0;
    while class < CLASSES {
        table[class] = (1u64 << 32) / SIZES[class] as u64 + 1;
        class += 1;
    }
    table
};

/// A size class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Class(u8);

impl Class {
    /// The class that serves `size` bytes aligned to `align` (a power of
    /// two), or `None` when the request needs whole frames.
    pub fn for_layout(size: usize, align: usize) -> Option<Class> {
        if size > MAX_SMALL || align > PAGE_SIZE {
            return None;
        }
        let mut class = usize::from(BY_SIZE[size.div_ceil(16)]);
        while class < CLASSES {
            if Class(class as u8).align() >= align {
                return Some(Class(class as u8));
            }
            class += 1;
        }
        None
    }

    /// The class with index `index`, below [`CLASSES`].
    pub fn from_index(index: usize) -> Option<Class> {
        (index < CLASSES).then_some(Class(index as u8))
    }

    /// All classes, smallest first.
    pub fn all() -> impl Iterator<Item = Class> {
        (0..CLASSES as u8).map(Class)
    }

    /// Its index, below [`CLASSES`].
    pub fn index(self) -> usize {
        usize::from(self.0)
    }

    /// Object size in bytes.
    pub fn size(self) -> usize {
        usize::from(SIZES[self.index()])
    }

    /// Alignment every object of the class has.
    pub fn align(self) -> usize {
        1 << self.size().trailing_zeros().min(PAGE_SIZE.trailing_zeros())
    }

    /// Order of the class's slabs.
    pub fn slab_order(self) -> u32 {
        u32::from(SLAB_ORDERS[self.index()])
    }

    /// Bytes in one slab.
    pub fn slab_bytes(self) -> usize {
        PAGE_SIZE << self.slab_order()
    }

    /// Objects in one slab.
    pub fn objects(self) -> usize {
        usize::from(OBJECTS[self.index()])
    }

    /// The index of the object that starts `offset` bytes into one of the
    /// class's slabs, if one does.
    pub fn object_at(self, offset: usize) -> Option<usize> {
        if offset >= self.slab_bytes() {
            return None;
        }
        // Without a division: see RECIPROCALS.
        let index = ((offset as u64 * RECIPROCALS[self.index()]) >> 32) as usize;
        (index * self.size() == offset && index < self.objects()).then_some(index)
    }

    /// Objects a per-CPU magazine holds: fewer for large objects, so idle
    /// caches keep less memory.
    pub fn magazine_rounds(self) -> usize {
        match self.size() {
            0..=256 => crate::magazine::ROUNDS,
            257..=1024 => crate::magazine::ROUNDS / 2,
            _ => crate::magazine::ROUNDS / 4,
        }
    }
}

/// The order of the block of frames that serves `size` bytes aligned to
/// `align` without a slab, or `None` when it is larger than the frame
/// allocator's biggest block. Blocks are aligned to their size.
pub fn large_order(size: usize, align: usize) -> Option<u32> {
    let bytes = size.max(align).max(PAGE_SIZE).checked_next_power_of_two()?;
    let order = bytes.trailing_zeros() - PAGE_SIZE.trailing_zeros();
    (order <= crate::buddy::MAX_ORDER).then_some(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn classes_are_sorted_and_slabs_are_full_enough() {
        for class in Class::all() {
            assert_eq!(class.size() % 16, 0);
            if class.index() > 0 {
                let smaller = Class(class.0 - 1).size();
                assert!(smaller < class.size());
                // Rounding up wastes less than a fifth above 64 bytes, and
                // less than 16 bytes below.
                let gap = class.size() - smaller;
                assert!(if class.size() > 64 { gap * 5 <= class.size() } else { gap <= 16 });
            }
            let waste = class.slab_bytes() - class.objects() * class.size();
            assert!(class.objects() >= 4 && waste * 8 <= class.slab_bytes(), "{class:?}");
            assert!(class.objects() <= usize::from(u16::MAX));
        }
        assert_eq!(Class::for_layout(1, 1), Some(Class(0)));
        assert_eq!(Class::for_layout(0, 1), Some(Class(0)));
        assert_eq!(Class::for_layout(17, 8).map(Class::size), Some(32));
        assert_eq!(Class::for_layout(48, 32).map(Class::size), Some(64));
        assert_eq!(Class::for_layout(100, 4096).map(Class::size), Some(4096));
        assert_eq!(Class::for_layout(8193, 8), None);
        assert_eq!(Class::for_layout(8, 8192), None);
        assert_eq!(large_order(8193, 8), Some(2));
        assert_eq!(large_order(1, 8192), Some(1));
        assert_eq!(large_order(4 << 20, 8), Some(10));
        assert_eq!(large_order((4 << 20) + 1, 8), None);
        assert_eq!(large_order(usize::MAX, 8), None);
    }

    #[test]
    fn objects_are_found_without_dividing() {
        for class in Class::all() {
            let mut order = 0;
            while order < MAX_SLAB_ORDER && (PAGE_SIZE << order) / class.size() < 8 {
                order += 1;
            }
            assert_eq!(class.slab_order(), order);
            assert_eq!(class.objects(), class.slab_bytes() / class.size());
            for offset in 0..class.slab_bytes() + 64 {
                let index = offset / class.size();
                let expected =
                    (offset % class.size() == 0 && index < class.objects()).then_some(index);
                assert_eq!(class.object_at(offset), expected, "{class:?} at {offset}");
            }
        }
    }

    proptest! {
        #[test]
        fn a_class_fits_its_requests(size in 0usize..=MAX_SMALL + 64, shift in 0u32..14) {
            let align = 1usize << shift;
            match Class::for_layout(size, align) {
                Some(class) => {
                    prop_assert!(class.size() >= size && class.align() >= align);
                    // No smaller class would do.
                    if let Some(smaller) = class.index().checked_sub(1) {
                        let smaller = Class(smaller as u8);
                        prop_assert!(smaller.size() < size || smaller.align() < align);
                    }
                }
                None => prop_assert!(size > MAX_SMALL || align > PAGE_SIZE),
            }
        }
    }
}
