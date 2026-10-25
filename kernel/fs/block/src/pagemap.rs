// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A sparse map from page numbers to values: the index of a page cache.
//!
//! It is a radix tree of nodes of [`FAN`] slots, as Linux's page cache
//! index (the XArray) is: page number `n` is found by taking six bits of
//! `n` at a time, from the top level down, so a lookup touches at most
//! `ceil(64 / 6)` nodes and never compares keys. The tree is only as tall
//! as the largest page number needs; a node whose slots all empty goes.
//! A file of `p` pages takes about `p / 64` leaf nodes.
//!
//! A node's slots are a vector on the heap, filled there one slot at a
//! time: a node built whole would be kilobytes on the stack first, and
//! kernel stacks are 16 KiB.

use crate::mem::{self, KVec, NoMemory};

/// Bits of a page number each level takes.
const BITS: u32 = 6;
/// Slots of a node.
pub const FAN: usize = 1 << BITS;

/// A node: [`FAN`] slots of values (a leaf) or of nodes.
enum Node<T> {
    Leaf(KVec<Option<T>>),
    Inner(KVec<Option<Node<T>>>),
}

/// [`FAN`] empty slots.
fn empty<S>() -> Result<KVec<Option<S>>, NoMemory> {
    let mut slots = mem::vec_with_capacity(FAN)?;
    for _ in 0..FAN {
        // Room for all of them was made above.
        let _ = slots.push_within_capacity(None);
    }
    Ok(slots)
}

impl<T> Node<T> {
    /// An empty leaf, for level 0, or an empty inner node.
    fn new(level: u32) -> Result<Node<T>, NoMemory> {
        Ok(if level == 0 { Node::Leaf(empty()?) } else { Node::Inner(empty()?) })
    }

    fn is_empty(&self) -> bool {
        match self {
            Node::Leaf(slots) => slots.iter().all(Option::is_none),
            Node::Inner(slots) => slots.iter().all(Option::is_none),
        }
    }
}

/// A map from page numbers to values.
pub struct PageMap<T> {
    root: Option<Node<T>>,
    /// Levels below and including the root: page numbers below
    /// `FAN^height` fit.
    height: u32,
    len: usize,
}

impl<T> Default for PageMap<T> {
    fn default() -> Self {
        PageMap::new()
    }
}

/// The slot of page `index` at `level` (0 for the leaves).
fn slot(index: u64, level: u32) -> usize {
    ((index >> (BITS * level)) as usize) & (FAN - 1)
}

/// Whether page `index` fits under a tree of `height` levels.
fn fits(index: u64, height: u32) -> bool {
    height > 0 && (BITS * height >= 64 || index >> (BITS * height) == 0)
}

impl<T> PageMap<T> {
    /// An empty map.
    pub const fn new() -> PageMap<T> {
        PageMap { root: None, height: 0, len: 0 }
    }

    /// How many pages it holds.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The value of page `index`.
    pub fn get(&self, index: u64) -> Option<&T> {
        if !fits(index, self.height) {
            return None;
        }
        let mut node = self.root.as_ref()?;
        let mut level = self.height - 1;
        loop {
            match node {
                Node::Leaf(slots) => return slots[slot(index, 0)].as_ref(),
                Node::Inner(slots) => {
                    node = slots[slot(index, level)].as_ref()?;
                    level -= 1;
                }
            }
        }
    }

    /// The value of page `index`, to change.
    pub fn get_mut(&mut self, index: u64) -> Option<&mut T> {
        if !fits(index, self.height) {
            return None;
        }
        let mut node = self.root.as_mut()?;
        let mut level = self.height - 1;
        loop {
            match node {
                Node::Leaf(slots) => return slots[slot(index, 0)].as_mut(),
                Node::Inner(slots) => {
                    node = slots[slot(index, level)].as_mut()?;
                    level -= 1;
                }
            }
        }
    }

    /// Makes the tree tall enough for page `index`.
    fn grow(&mut self, index: u64) -> Result<(), NoMemory> {
        if self.root.is_none() {
            self.height = 1;
            while !fits(index, self.height) {
                self.height += 1;
            }
            return Ok(());
        }
        while !fits(index, self.height) {
            // The new root is allocated before the old one moves into it,
            // so a failure leaves the tree as it was.
            let mut node = Node::new(self.height)?;
            if let Node::Inner(slots) = &mut node {
                slots[0] = self.root.take();
            }
            self.root = Some(node);
            self.height += 1;
        }
        Ok(())
    }

    /// Sets page `index` to `value`, returning what it held. Fails, giving
    /// `value` back, when a node cannot be allocated.
    pub fn insert(&mut self, index: u64, value: T) -> Result<Option<T>, (NoMemory, T)> {
        if let Err(err) = self.grow(index) {
            return Err((err, value));
        }
        if self.root.is_none() {
            match Node::new(self.height - 1) {
                Ok(node) => self.root = Some(node),
                Err(err) => return Err((err, value)),
            }
        }
        let height = self.height;
        let Some(root) = self.root.as_mut() else { return Err((NoMemory, value)) };
        let mut node = root;
        let mut level = height - 1;
        loop {
            match node {
                Node::Leaf(slots) => {
                    let old = slots[slot(index, 0)].replace(value);
                    if old.is_none() {
                        self.len += 1;
                    }
                    return Ok(old);
                }
                Node::Inner(slots) => {
                    let at = &mut slots[slot(index, level)];
                    if at.is_none() {
                        match Node::new(level - 1) {
                            Ok(child) => *at = Some(child),
                            Err(err) => return Err((err, value)),
                        }
                    }
                    let Some(child) = at.as_mut() else { return Err((NoMemory, value)) };
                    node = child;
                    level -= 1;
                }
            }
        }
    }

    /// Takes page `index` out.
    pub fn remove(&mut self, index: u64) -> Option<T> {
        if !fits(index, self.height) {
            return None;
        }
        let root = self.root.as_mut()?;
        let taken = remove_in(root, index, self.height - 1);
        if taken.is_some() {
            self.len -= 1;
            if self.root.as_ref().is_some_and(Node::is_empty) {
                self.root = None;
                self.height = 0;
            }
        }
        taken
    }

    /// The first page at or after `index`, and its value.
    pub fn next(&self, index: u64) -> Option<(u64, &T)> {
        if self.height == 0 || !fits(index, self.height) {
            return None;
        }
        let root = self.root.as_ref()?;
        next_in(root, index, self.height - 1, 0)
    }

    /// Calls `f` with every page from `index` on, in order, until it
    /// returns false.
    pub fn for_each_from(&self, index: u64, mut f: impl FnMut(u64, &T) -> bool) {
        let mut at = index;
        while let Some((found, value)) = self.next(at) {
            if !f(found, value) {
                return;
            }
            match found.checked_add(1) {
                Some(next) => at = next,
                None => return,
            }
        }
    }

    /// Keeps only the pages for which `keep` returns true, and returns
    /// those it took out, through `taken`.
    pub fn retain(&mut self, mut keep: impl FnMut(u64, &mut T) -> bool, mut taken: impl FnMut(T)) {
        if self.height == 0 {
            return;
        }
        let Some(root) = self.root.as_mut() else { return };
        let removed = retain_in(root, self.height - 1, 0, &mut keep, &mut taken);
        self.len -= removed;
        if self.root.as_ref().is_some_and(Node::is_empty) {
            self.root = None;
            self.height = 0;
        }
    }
}

fn remove_in<T>(node: &mut Node<T>, index: u64, level: u32) -> Option<T> {
    match node {
        Node::Leaf(slots) => slots[slot(index, 0)].take(),
        Node::Inner(slots) => {
            let at = &mut slots[slot(index, level)];
            let child = at.as_mut()?;
            let taken = remove_in(child, index, level - 1);
            if taken.is_some() && child.is_empty() {
                *at = None;
            }
            taken
        }
    }
}

/// The first page at or after `index` under `node`, whose pages start at
/// `base`.
fn next_in<T>(node: &Node<T>, index: u64, level: u32, base: u64) -> Option<(u64, &T)> {
    let span = 1u64.checked_shl(BITS * level).unwrap_or(0);
    let first = if index > base { slot(index, level) } else { 0 };
    match node {
        Node::Leaf(slots) => {
            (first..FAN).find_map(|i| slots[i].as_ref().map(|value| (base + i as u64, value)))
        }
        Node::Inner(slots) => {
            for (i, child) in slots.iter().enumerate().skip(first) {
                let Some(child) = child.as_ref() else { continue };
                let child_base = base + i as u64 * span;
                if let Some(found) = next_in(child, index.max(child_base), level - 1, child_base) {
                    return Some(found);
                }
            }
            None
        }
    }
}

fn retain_in<T>(
    node: &mut Node<T>,
    level: u32,
    base: u64,
    keep: &mut impl FnMut(u64, &mut T) -> bool,
    taken: &mut impl FnMut(T),
) -> usize {
    let span = 1u64.checked_shl(BITS * level).unwrap_or(0);
    let mut removed = 0;
    match node {
        Node::Leaf(slots) => {
            for (i, at) in slots.iter_mut().enumerate() {
                if let Some(value) = at
                    && !keep(base + i as u64, value)
                {
                    if let Some(value) = at.take() {
                        taken(value);
                    }
                    removed += 1;
                }
            }
        }
        Node::Inner(slots) => {
            for (i, at) in slots.iter_mut().enumerate() {
                let Some(child) = at.as_mut() else { continue };
                removed += retain_in(child, level - 1, base + i as u64 * span, keep, taken);
                if child.is_empty() {
                    *at = None;
                }
            }
        }
    }
    removed
}
