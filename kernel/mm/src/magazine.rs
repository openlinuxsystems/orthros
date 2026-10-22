// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Magazines: the per-CPU object caches of the heap (Bonwick and Adams,
//! "Magazines and Vmem", USENIX 2001).
//!
//! A magazine is a small stack of free objects of one size class. Each CPU
//! keeps two per class, *loaded* and *previous*. Allocating pops from the
//! loaded magazine; freeing pushes onto it. When it runs empty (or full),
//! the CPU swaps it with the previous one, and only when both are empty (or
//! both full) does it exchange a magazine with the class's *depot*, a
//! locked list of full and empty magazines shared by all CPUs. So most
//! allocations and frees touch only the CPU's own cache, and a CPU that
//! alternates between allocating and freeing never touches the depot at
//! all.
//!
//! Magazines are 256-byte objects of the heap's own 256-byte class,
//! allocated from its slab layer directly. They are linked by `&'static
//! mut` references: whoever holds one owns it, so everything here is safe
//! code.

#![forbid(unsafe_code)]

/// Most objects one magazine holds (classes of large objects use fewer).
pub const ROUNDS: usize = 30;

/// A stack of free objects of one class.
#[repr(C)]
pub struct Magazine {
    /// The next magazine in a depot list.
    next: Option<&'static mut Magazine>,
    len: usize,
    rounds: [usize; ROUNDS],
}

/// Bytes a magazine takes: the 256-byte size class.
pub const MAGAZINE_BYTES: usize = 256;
// In statement position: Kani's `assert!` ends with a semicolon.
const _: () = {
    assert!(size_of::<Magazine>() == MAGAZINE_BYTES);
};

impl Magazine {
    /// An empty magazine.
    pub const fn new() -> Magazine {
        Magazine { next: None, len: 0, rounds: [0; ROUNDS] }
    }

    /// Objects it holds.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds no objects.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Pushes `object`, unless the magazine already holds `capacity` (at
    /// most [`ROUNDS`]) objects.
    pub fn push(&mut self, object: usize, capacity: usize) -> Result<(), usize> {
        if self.len >= capacity.min(ROUNDS) {
            return Err(object);
        }
        self.rounds[self.len] = object;
        self.len += 1;
        Ok(())
    }

    /// Pops the object pushed last.
    pub fn pop(&mut self) -> Option<usize> {
        self.len = self.len.checked_sub(1)?;
        Some(self.rounds[self.len])
    }
}

impl Default for Magazine {
    fn default() -> Self {
        Self::new()
    }
}

/// A list of magazines linked through their `next` fields.
#[derive(Default)]
pub struct Stack {
    head: Option<&'static mut Magazine>,
    len: usize,
}

impl Stack {
    /// An empty list.
    pub const fn new() -> Stack {
        Stack { head: None, len: 0 }
    }

    /// Magazines on the list.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Adds `magazine` to the list.
    pub fn push(&mut self, magazine: &'static mut Magazine) {
        magazine.next = self.head.take();
        self.head = Some(magazine);
        self.len += 1;
    }

    /// The magazines on the list, last added first.
    pub fn iter(&self) -> impl Iterator<Item = &Magazine> {
        core::iter::successors(self.head.as_deref(), |m| m.next.as_deref())
    }

    /// Takes the magazine added last.
    pub fn pop(&mut self) -> Option<&'static mut Magazine> {
        let magazine = self.head.take()?;
        self.head = magazine.next.take();
        self.len -= 1;
        Some(magazine)
    }
}

/// The depot of one size class: full and empty magazines shared by all
/// CPUs.
#[derive(Default)]
pub struct Depot {
    /// Magazines holding a full load.
    pub full: Stack,
    /// Magazines holding nothing.
    pub empty: Stack,
}

impl Depot {
    /// An empty depot.
    pub const fn new() -> Depot {
        Depot { full: Stack::new(), empty: Stack::new() }
    }
}

/// One CPU's two magazines for one class.
#[derive(Default)]
pub struct Loaded {
    /// The magazine allocations pop from and frees push onto.
    pub loaded: Option<&'static mut Magazine>,
    /// The one before it, kept full or empty.
    pub previous: Option<&'static mut Magazine>,
}

impl Loaded {
    /// No magazines.
    pub const fn new() -> Loaded {
        Loaded { loaded: None, previous: None }
    }

    /// Takes an object from the loaded magazine, swapping in the previous
    /// one if that one has objects.
    pub fn pop(&mut self) -> Option<usize> {
        if let Some(object) = self.loaded.as_mut().and_then(|m| m.pop()) {
            return Some(object);
        }
        if self.previous.as_ref().is_some_and(|m| !m.is_empty()) {
            core::mem::swap(&mut self.loaded, &mut self.previous);
            return self.loaded.as_mut().and_then(|m| m.pop());
        }
        None
    }

    /// Puts an object in the loaded magazine, swapping in the previous one
    /// if that one has room. Gives the object back when both are full (or
    /// missing).
    pub fn push(&mut self, object: usize, capacity: usize) -> Result<(), usize> {
        let object = match self.loaded.as_mut() {
            Some(magazine) => match magazine.push(object, capacity) {
                Ok(()) => return Ok(()),
                Err(object) => object,
            },
            None => object,
        };
        if self.previous.as_ref().is_some_and(|m| m.len() < capacity.min(ROUNDS)) {
            core::mem::swap(&mut self.loaded, &mut self.previous);
            if let Some(magazine) = self.loaded.as_mut() {
                return magazine.push(object, capacity);
            }
        }
        Err(object)
    }

    /// Objects held in both magazines.
    pub fn len(&self) -> usize {
        self.loaded.as_ref().map_or(0, |m| m.len()) + self.previous.as_ref().map_or(0, |m| m.len())
    }

    /// Whether both magazines are empty or missing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn magazine() -> &'static mut Magazine {
        Box::leak(Box::new(Magazine::new()))
    }

    #[test]
    fn a_magazine_is_a_bounded_stack() {
        let m = magazine();
        assert_eq!(m.pop(), None);
        for object in 1..=4 {
            m.push(object, 4).unwrap();
        }
        assert_eq!(m.push(5, 4), Err(5));
        assert_eq!(m.pop(), Some(4));
        assert_eq!(m.len(), 3);
        assert_eq!(m.push(9, 100), Ok(()));
        let mut stack = Stack::new();
        stack.push(m);
        stack.push(magazine());
        assert_eq!(stack.len(), 2);
        assert!(stack.pop().unwrap().is_empty());
        assert_eq!(stack.pop().unwrap().len(), 4);
        assert!(stack.pop().is_none());
    }

    proptest! {
        /// Two magazines behave like one stack of twice the capacity, as
        /// long as nothing goes in when both are full.
        #[test]
        fn loaded_and_previous_hold_what_was_pushed(
            ops in proptest::collection::vec(any::<bool>(), 0..200),
            capacity in 1usize..=ROUNDS,
        ) {
            let mut cpu = Loaded { loaded: Some(magazine()), previous: Some(magazine()) };
            let mut model: Vec<usize> = Vec::new();
            let mut next = 1;
            for push in ops {
                if push {
                    match cpu.push(next, capacity) {
                        Ok(()) => model.push(next),
                        Err(object) => {
                            prop_assert_eq!(object, next);
                            prop_assert_eq!(model.len(), 2 * capacity);
                        }
                    }
                    next += 1;
                } else {
                    match cpu.pop() {
                        Some(object) => {
                            let at = model.iter().position(|&o| o == object);
                            prop_assert!(at.is_some());
                            model.remove(at.unwrap());
                        }
                        None => prop_assert!(model.is_empty()),
                    }
                }
                prop_assert_eq!(cpu.len(), model.len());
            }
        }
    }
}
