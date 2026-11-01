// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Changing a directory's index: names put in and taken out of its B+
//! tree ([`crate::index`] reads it). `$Extend\$Reparse`'s index of
//! reparse points is kept the same way, its keys in their own
//! [`Order`].
//!
//! A name goes into the leaf where a lookup for it ends, in order. A node
//! with no room left splits at its middle entry, which goes up into its
//! parent between the two halves, as in any B-tree; the root lives in the
//! directory's record, so when the record has no room for it, all its
//! entries move down into a new block and the root keeps only its last
//! entry, pointing there. A name taken out of a leaf just goes; one in a
//! node above gives its place to the greatest name of the subtree before
//! it. A leaf left empty is freed: the entry above it comes down into its
//! neighbour, and a node above left with no names but a child is replaced
//! by that child. Every node stays in order and every entry of a node
//! above has a subtree, as Windows' own `chkdsk` checks.

use core::cmp::Ordering;

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::edit::{BLOCK_USA, put_u16, put_u32, put_u64};
use crate::index::{BLOCK_HEADER, FileName, ROOT_HEADER};
use crate::record::{STRIDE, u16_at, u32_at, u64_at};
use crate::upcase::Upcase;

/// Bits of an index entry's flags.
const SUBNODE: u16 = 1;
const LAST: u16 = 2;

/// The bit of a node header's flags that says its entries have subtrees.
const HAS_CHILDREN: u8 = 1;

/// The deepest tree Radix changes.
const MAX_DEPTH: usize = 32;

/// What a directory's index blocks are kept in.
pub trait Store {
    /// Reads the block at VCN `vcn` into `buf`, its update sequence put
    /// back.
    fn read(&mut self, vcn: u64, buf: &mut [u8]) -> Result<()>;
    /// Writes `buf`, the block at VCN `vcn` with its update sequence put
    /// back; the store puts it on for the disk.
    fn write(&mut self, vcn: u64, buf: &[u8]) -> Result<()>;
    /// A free block's VCN, now in use.
    fn alloc(&mut self) -> Result<u64>;
    /// Frees the block at VCN `vcn`.
    fn free(&mut self, vcn: u64) -> Result<()>;
    /// Whether the record has room for a root of `len` bytes.
    fn root_fits(&self, len: usize) -> bool;
    /// The size of a block.
    fn block_size(&self) -> usize;
}

/// How an index orders its keys, which the tree's functions take as
/// UTF-16 units.
#[derive(Clone, Copy)]
pub enum Order<'a> {
    /// A directory's file names, through the volume's up-case table
    /// ([`collate`]).
    Names(&'a Upcase),
    /// Keys of 32-bit little-endian numbers compared in turn, as Windows'
    /// `COLLATION_NTOFS_ULONGS`: `$Reparse`'s, a reparse tag and a file
    /// reference. Their units are the key's bytes in pairs ([`ulongs`]).
    Ulongs,
}

impl Order<'_> {
    /// The order of `key` and the key of `entry`.
    fn compare(self, key: &[u16], entry: &[u8]) -> Result<Ordering> {
        match self {
            Order::Names(upcase) => Ok(collate(upcase, key, key_of(entry)?.units())),
            Order::Ulongs => {
                let theirs = raw_key(entry)?;
                let ours = key
                    .chunks(2)
                    .map(|p| u32::from(p[0]) | u32::from(p.get(1).copied().unwrap_or(0)) << 16);
                let theirs = theirs.chunks(4).map(|b| {
                    let mut n = [0u8; 4];
                    n[..b.len()].copy_from_slice(b);
                    u32::from_le_bytes(n)
                });
                Ok(ours.cmp(theirs))
            }
        }
    }

    /// The key of `entry`, as the tree's functions take it.
    fn key(self, entry: &[u8]) -> Result<KVec<u16>> {
        match self {
            Order::Names(_) => units_of(entry),
            Order::Ulongs => ulongs(raw_key(entry)?),
        }
    }

    /// Whether `entry` is the file `reference`'s: a directory's entries
    /// say which file they are for, and `$Reparse`'s keys do.
    fn is_of(self, entry: &[u8], reference: u64) -> bool {
        match self {
            Order::Names(_) => {
                crate::record::record_of(u64_at(entry, 0)) == crate::record::record_of(reference)
            }
            Order::Ulongs => true,
        }
    }
}

/// The units [`Order::Ulongs`] takes for a key of `bytes`.
pub fn ulongs(bytes: &[u8]) -> Result<KVec<u16>> {
    let mut v = mem::vec();
    for pair in bytes.chunks(2) {
        mem::push(&mut v, u16::from(pair[0]) | u16::from(pair.get(1).copied().unwrap_or(0)) << 8)?;
    }
    Ok(v)
}

/// The bytes of `entry`'s key.
fn raw_key(entry: &[u8]) -> Result<&[u8]> {
    let len = usize::from(u16_at(entry, 0x0a));
    entry.get(0x10..0x10 + len).ok_or(Error::Corrupt)
}

/// The order of the names `a` and `b` in an index: as the up-case table
/// orders them, and names the same but for case by their units.
pub fn collate(upcase: &Upcase, a: &[u16], b: impl Iterator<Item = u16> + Clone) -> Ordering {
    match upcase.compare(a.iter().copied(), b.clone()) {
        Ordering::Equal => a.iter().copied().cmp(b),
        order => order,
    }
}

/// An index entry for the file `reference`, whose key is the `$FILE_NAME`
/// value `key`, pointing to the block at VCN `subnode` if any.
pub fn make_entry(reference: u64, key: &[u8], subnode: Option<u64>) -> Result<KVec<u8>> {
    let body = (0x10 + key.len()).next_multiple_of(8);
    let len = body + if subnode.is_some() { 8 } else { 0 };
    let mut e = crate::fs::zeroed(len)?;
    put_u64(&mut e, 0, reference);
    put_u16(&mut e, 8, len as u16);
    put_u16(&mut e, 0x0a, key.len() as u16);
    put_u16(&mut e, 0x0c, if subnode.is_some() { SUBNODE } else { 0 });
    e[0x10..0x10 + key.len()].copy_from_slice(key);
    if let Some(vcn) = subnode {
        put_u64(&mut e, len - 8, vcn);
    }
    Ok(e)
}

/// A node's last, keyless entry, pointing to the block at VCN `subnode`
/// if any.
fn last_entry(subnode: Option<u64>) -> Result<KVec<u8>> {
    let len = if subnode.is_some() { 0x18 } else { 0x10 };
    let mut e = crate::fs::zeroed(len)?;
    put_u16(&mut e, 8, len as u16);
    put_u16(&mut e, 0x0c, LAST | if subnode.is_some() { SUBNODE } else { 0 });
    if let Some(vcn) = subnode {
        put_u64(&mut e, len - 8, vcn);
    }
    Ok(e)
}

/// The entry `entry` pointing to `subnode` instead of what it pointed to.
fn repoint(entry: &[u8], subnode: Option<u64>) -> Result<KVec<u8>> {
    let flags = u16_at(entry, 0x0c);
    if flags & LAST != 0 {
        return last_entry(subnode);
    }
    let key_len = usize::from(u16_at(entry, 0x0a));
    make_entry(u64_at(entry, 0), &entry[0x10..0x10 + key_len], subnode)
}

fn flags_of(entry: &[u8]) -> u16 {
    u16_at(entry, 0x0c)
}

fn is_last(entry: &[u8]) -> bool {
    flags_of(entry) & LAST != 0
}

fn subnode_of(entry: &[u8]) -> Option<u64> {
    (flags_of(entry) & SUBNODE != 0).then(|| u64_at(entry, entry.len() - 8))
}

fn key_of(entry: &[u8]) -> Result<FileName<'_>> {
    FileName::new(raw_key(entry)?)
}

/// Which node of the tree: the root, or the block at a VCN.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum At {
    Root,
    Block(u64),
}

/// A node, read: its bytes (the root's value, or a block) and where its
/// header is.
struct Node {
    at: At,
    bytes: KVec<u8>,
}

impl Node {
    fn header(&self) -> usize {
        match self.at {
            At::Root => ROOT_HEADER,
            At::Block(_) => BLOCK_HEADER,
        }
    }

    /// Where its entries start and end.
    fn span(&self) -> Result<(usize, usize)> {
        let h = self.header();
        let h_bytes = self.bytes.get(h..h + 16).ok_or(Error::Corrupt)?;
        let first = h + u32_at(h_bytes, 0) as usize;
        let end = h + u32_at(h_bytes, 4) as usize;
        if first > end || end > self.bytes.len() {
            return Err(Error::Corrupt);
        }
        Ok((first, end))
    }

    /// Its entries' offsets, the last one's too.
    fn offsets(&self) -> Result<KVec<usize>> {
        let (mut at, end) = self.span()?;
        let mut out = mem::vec();
        loop {
            let rest = self.bytes.get(at..end).ok_or(Error::Corrupt)?;
            if rest.len() < 0x10 {
                return Err(Error::Corrupt);
            }
            let len = usize::from(u16_at(rest, 8));
            if len < 0x10 || len % 8 != 0 || len > rest.len() {
                return Err(Error::Corrupt);
            }
            mem::push(&mut out, at)?;
            if is_last(rest) {
                return Ok(out);
            }
            at += len;
        }
    }

    fn entry(&self, at: usize) -> &[u8] {
        let len = usize::from(u16_at(&self.bytes, at + 8));
        &self.bytes[at..at + len]
    }

    /// Replaces its entries with `entries`, the last one's too, all with
    /// subtrees or none. Fails with [`Error::NoSpace`] if a block has no
    /// room for them; a root grows.
    fn set_entries(&mut self, entries: &[KVec<u8>]) -> Result<()> {
        let h = self.header();
        let (first, _) = self.span()?;
        let total: usize = entries.iter().map(KVec::len).sum();
        let end = first + total;
        match self.at {
            At::Root => {
                self.bytes.truncate(first);
                for e in entries {
                    mem::extend(&mut self.bytes, e)?;
                }
                // A root's allocated size is its length.
                put_u32(&mut self.bytes, h + 4, (end - h) as u32);
                put_u32(&mut self.bytes, h + 8, (end - h) as u32);
            }
            At::Block(_) => {
                let allocated = u32_at(&self.bytes, h + 8) as usize;
                if end - h > allocated || end > self.bytes.len() {
                    return Err(Error::NoSpace);
                }
                let mut at = first;
                for e in entries {
                    self.bytes[at..at + e.len()].copy_from_slice(e);
                    at += e.len();
                }
                self.bytes[end..].fill(0);
                put_u32(&mut self.bytes, h + 4, (end - h) as u32);
            }
        }
        let children = entries.last().and_then(|e| subnode_of(e)).is_some();
        let flags = &mut self.bytes[h + 12];
        *flags = (*flags & !HAS_CHILDREN) | if children { HAS_CHILDREN } else { 0 };
        Ok(())
    }

    /// Its entries, copied.
    fn entries(&self) -> Result<KVec<KVec<u8>>> {
        let mut out = mem::vec();
        for at in self.offsets()? {
            mem::push(&mut out, mem::copy(self.entry(at))?)?;
        }
        Ok(out)
    }

    /// Whether entries of `total` bytes fit it.
    fn fits(&self, store: &dyn Store, total: usize) -> Result<bool> {
        let h = self.header();
        let (first, _) = self.span()?;
        Ok(match self.at {
            At::Root => store.root_fits(first + total),
            At::Block(_) => first + total - h <= u32_at(&self.bytes, h + 8) as usize,
        })
    }
}

/// A fresh block for VCN `vcn` of `size` bytes, with no entries.
fn fresh_block(vcn: u64, size: usize) -> Result<Node> {
    let mut b = crate::fs::zeroed(size)?;
    b[..4].copy_from_slice(b"INDX");
    put_u16(&mut b, 4, BLOCK_USA as u16);
    let count = 1 + size / STRIDE;
    put_u16(&mut b, 6, count as u16);
    put_u16(&mut b, BLOCK_USA, 1);
    put_u64(&mut b, 0x10, vcn);
    let first = (BLOCK_USA + 2 * count).next_multiple_of(8);
    let h = BLOCK_HEADER;
    put_u32(&mut b, h, (first - h) as u32);
    put_u32(&mut b, h + 4, (first - h) as u32);
    put_u32(&mut b, h + 8, (size - h) as u32);
    Ok(Node { at: At::Block(vcn), bytes: b })
}

fn read_node(root: &[u8], store: &mut dyn Store, at: At) -> Result<Node> {
    match at {
        At::Root => Ok(Node { at, bytes: mem::copy(root)? }),
        At::Block(vcn) => {
            let mut bytes = crate::fs::zeroed(store.block_size())?;
            store.read(vcn, &mut bytes)?;
            if u64_at(&bytes, 0x10) != vcn {
                return Err(Error::Corrupt);
            }
            Ok(Node { at, bytes })
        }
    }
}

fn write_node(root: &mut KVec<u8>, store: &mut dyn Store, node: &Node) -> Result<()> {
    match node.at {
        At::Root => {
            root.clear();
            mem::extend(root, &node.bytes)?;
            Ok(())
        }
        At::Block(vcn) => store.write(vcn, &node.bytes),
    }
}

/// A step down the tree: the node, and which of its entries the way went
/// on through (its index among them).
struct Step {
    node: Node,
    index: usize,
}

/// Goes down from the root to where `name` is or would be: each node on
/// the way, and the entry the way went through, the first whose key is
/// not less than `name` (or the last). Stops at a node whose entry is
/// `name` itself, or at a leaf.
fn descend(
    root: &[u8],
    store: &mut dyn Store,
    order: Order<'_>,
    name: &[u16],
) -> Result<(KVec<Step>, bool)> {
    let mut path = mem::vec();
    let mut at = At::Root;
    for _ in 0..MAX_DEPTH {
        let node = read_node(root, store, at)?;
        let offsets = node.offsets()?;
        let mut index = offsets.len() - 1;
        let mut found = false;
        for (i, &off) in offsets.iter().enumerate() {
            let e = node.entry(off);
            if is_last(e) {
                index = i;
                break;
            }
            match order.compare(name, e)? {
                Ordering::Greater => {}
                order => {
                    index = i;
                    found = order == Ordering::Equal;
                    break;
                }
            }
        }
        let next = subnode_of(node.entry(offsets[index]));
        mem::push(&mut path, Step { node, index })?;
        if found {
            return Ok((path, true));
        }
        match next {
            Some(vcn) => at = At::Block(vcn),
            None => return Ok((path, false)),
        }
    }
    Err(Error::Corrupt)
}

/// The UTF-16 units of the name an entry's key holds.
fn units_of(entry: &[u8]) -> Result<KVec<u16>> {
    let mut v = mem::vec();
    for u in key_of(entry)?.units() {
        mem::push(&mut v, u)?;
    }
    Ok(v)
}

/// Puts the leaf entry `entry` (no subtree) for a name not in the index
/// into the index whose root's value is `root`, splitting nodes as they
/// fill. Fails with [`Error::Exists`] if its name is there.
pub fn insert(
    root: &mut KVec<u8>,
    store: &mut dyn Store,
    order: Order<'_>,
    entry: &[u8],
) -> Result<()> {
    let units = order.key(entry)?;
    let (mut path, found) = descend(root, store, order, &units)?;
    if found {
        return Err(Error::Exists);
    }
    let Step { node, index } = path.pop().ok_or(Error::Corrupt)?;
    let mut entries = node.entries()?;
    mem::insert(&mut entries, index, mem::copy(entry)?)?;
    propagate(root, store, path, node, entries)
}

/// Writes `entries` into `node`, whose parents are `path`, each with the
/// index of the entry the way to it went through: a node they do not fit
/// splits, its middle entry going up into its parent, as far up as that
/// goes. A root with no room in its record moves its entries down into a
/// block (two, if one cannot take them all).
fn propagate(
    root: &mut KVec<u8>,
    store: &mut dyn Store,
    mut path: KVec<Step>,
    mut node: Node,
    mut entries: KVec<KVec<u8>>,
) -> Result<()> {
    loop {
        let total: usize = entries.iter().map(KVec::len).sum();
        if node.fits(store, total)? {
            node.set_entries(&entries)?;
            return write_node(root, store, &node);
        }
        if node.at == At::Root {
            return root_down(root, store, node, entries);
        }
        // A block splits: the entries before the middle one go into a new
        // block, the middle one goes up pointing to it, and those after
        // stay.
        let (left, middle, right) = split(entries)?;
        let vcn = store.alloc()?;
        let mut new = fresh_block(vcn, store.block_size())?;
        new.set_entries(&left)?;
        node.set_entries(&right)?;
        store.write(vcn, &new.bytes)?;
        write_node(root, store, &node)?;
        let carry = repoint(&middle, Some(vcn))?;
        let Step { node: parent, index } = path.pop().ok_or(Error::Corrupt)?;
        entries = parent.entries()?;
        mem::insert(&mut entries, index, carry)?;
        node = parent;
    }
}

/// Moves the root's `entries`, which its record has no room for, down into
/// a new block, the root keeping only its last entry, pointing there; or
/// into two, the middle entry staying in the root between them, if one
/// block cannot take them all.
fn root_down(
    root: &mut KVec<u8>,
    store: &mut dyn Store,
    mut node: Node,
    entries: KVec<KVec<u8>>,
) -> Result<()> {
    let size = store.block_size();
    let vcn = store.alloc()?;
    let mut child = fresh_block(vcn, size)?;
    match child.set_entries(&entries) {
        Ok(()) => {
            store.write(vcn, &child.bytes)?;
            let last = last_entry(Some(vcn))?;
            node.set_entries(core::slice::from_ref(&last))?;
            write_node(root, store, &node)
        }
        Err(Error::NoSpace) => {
            let (left, middle, right) = split(entries)?;
            child.set_entries(&left)?;
            store.write(vcn, &child.bytes)?;
            let other = store.alloc()?;
            let mut second = fresh_block(other, size)?;
            second.set_entries(&right)?;
            store.write(other, &second.bytes)?;
            let mut up = mem::vec();
            mem::push(&mut up, repoint(&middle, Some(vcn))?)?;
            mem::push(&mut up, last_entry(Some(other))?)?;
            node.set_entries(&up)?;
            write_node(root, store, &node)
        }
        Err(err) => Err(err),
    }
}

/// A node's entries split at their middle one ([`split`]): those before
/// it, it, and those after.
type Halves = (KVec<KVec<u8>>, KVec<u8>, KVec<KVec<u8>>);

/// Splits a node's `entries` (its last one's too), too many for a block,
/// at its middle by bytes: the entries before the middle one, ended by a
/// last entry pointing where the middle one did; the middle one; and
/// those after it, with the node's last entry.
fn split(entries: KVec<KVec<u8>>) -> Result<Halves> {
    if entries.len() < 3 {
        return Err(Error::Corrupt);
    }
    let total: usize = entries.iter().map(KVec::len).sum();
    let mut before = 0;
    let mut middle = entries.len() - 2;
    for (i, e) in entries.iter().enumerate() {
        if i > 0 && before + e.len() / 2 >= total / 2 {
            middle = i;
            break;
        }
        before += e.len();
    }
    // Never the last entry, which has no key.
    let middle = middle.clamp(1, entries.len() - 2);
    let mut left = mem::vec();
    let mut right = mem::vec();
    let mut mid = None;
    for (i, e) in entries.into_iter().enumerate() {
        match i.cmp(&middle) {
            Ordering::Less => mem::push(&mut left, e)?,
            Ordering::Equal => mid = Some(e),
            Ordering::Greater => mem::push(&mut right, e)?,
        }
    }
    let mid = mid.ok_or(Error::Corrupt)?;
    mem::push(&mut left, last_entry(subnode_of(&mid))?)?;
    Ok((left, mid, right))
}

/// The steps from the block at VCN `vcn` down to the leaf at the end of
/// its subtree, by each node's last entry (`rightmost`) or first.
fn edge_path(root: &[u8], store: &mut dyn Store, vcn: u64, rightmost: bool) -> Result<KVec<Step>> {
    let mut steps = mem::vec();
    let mut at = vcn;
    for _ in 0..MAX_DEPTH {
        let node = read_node(root, store, At::Block(at))?;
        let offsets = node.offsets()?;
        let index = if rightmost { offsets.len() - 1 } else { 0 };
        let next = subnode_of(node.entry(offsets[index]));
        mem::push(&mut steps, Step { node, index })?;
        match next {
            Some(v) => at = v,
            None => return Ok(steps),
        }
    }
    Err(Error::Corrupt)
}

/// Takes the entry of `name`, for the file `reference`, out of the index
/// whose root's value is `root`. Fails with [`Error::NotFound`] if it is
/// not there.
pub fn remove(
    root: &mut KVec<u8>,
    store: &mut dyn Store,
    order: Order<'_>,
    name: &[u16],
    reference: u64,
) -> Result<()> {
    let (mut path, found) = descend(root, store, order, name)?;
    if !found {
        return Err(Error::NotFound);
    }
    let Step { mut node, index } = path.pop().ok_or(Error::Corrupt)?;
    let mut entries = node.entries()?;
    let e = &entries[index];
    if !order.is_of(e, reference) {
        return Err(Error::NotFound);
    }
    let Some(vcn) = subnode_of(e) else {
        // In a leaf: it just goes.
        entries.remove(index);
        node.set_entries(&entries)?;
        write_node(root, store, &node)?;
        let empty = entries.len() == 1 && node.at != At::Root;
        mem::push(&mut path, Step { node, index })?;
        return if empty { settle(root, store, order, path) } else { Ok(()) };
    };
    // In a node above: the greatest entry of the subtree before it takes
    // its place, keeping its subtree.
    let mut below = edge_path(root, store, vcn, true)?;
    let Step { node: mut leaf, .. } = below.pop().ok_or(Error::Corrupt)?;
    let mut leaf_entries = leaf.entries()?;
    if leaf_entries.len() < 2 {
        // A leaf with no names: not a tree as Radix keeps it.
        return Err(Error::Corrupt);
    }
    let greatest = leaf_entries.remove(leaf_entries.len() - 2);
    leaf.set_entries(&leaf_entries)?;
    write_node(root, store, &leaf)?;
    entries[index] = repoint(&greatest, Some(vcn))?;
    propagate(root, store, path, node, entries)?;
    // Where the greatest came from may be an empty leaf now: found again
    // from the top, as the nodes above may have split.
    let units = order.key(&greatest)?;
    let (mut path, found) = descend(root, store, order, &units)?;
    let step = path.last().ok_or(Error::Corrupt)?;
    let offsets = step.node.offsets()?;
    let sub = subnode_of(step.node.entry(offsets[step.index]));
    match (found, sub) {
        (true, Some(vcn)) => {
            for s in edge_path(root, store, vcn, true)? {
                mem::push(&mut path, s)?;
            }
            let leaf = &path.last().ok_or(Error::Corrupt)?.node;
            if leaf.offsets()?.len() == 1 { settle(root, store, order, path) } else { Ok(()) }
        }
        _ => Err(Error::Corrupt),
    }
}

/// Frees the empty leaf block at the bottom of `path`: the entry above it
/// that separates it from its neighbour comes down into that neighbour's
/// leaf, which takes its range. Nodes above left with no names but a
/// child give way to it.
fn settle(
    root: &mut KVec<u8>,
    store: &mut dyn Store,
    order: Order<'_>,
    mut path: KVec<Step>,
) -> Result<()> {
    let Step { node: leaf, .. } = path.pop().ok_or(Error::Corrupt)?;
    let At::Block(vcn) = leaf.at else { return Ok(()) };
    let Step { node: mut parent, index } = path.pop().ok_or(Error::Corrupt)?;
    let mut up = parent.entries()?;
    store.free(vcn)?;
    let (down, moved_to) = if index + 1 < up.len() {
        // The entry pointing here comes down to the front of the next
        // subtree, which takes this one's range.
        let down = repoint(&up.remove(index), None)?;
        parent.set_entries(&up)?;
        write_node(root, store, &parent)?;
        let next = subnode_of(&up[index]).ok_or(Error::Corrupt)?;
        (down, (index, next, false))
    } else if index == 0 {
        // The parent's only entry pointed here: it has no names and no
        // children now, an empty leaf in its turn.
        up[0] = last_entry(None)?;
        parent.set_entries(&up)?;
        write_node(root, store, &parent)?;
        if parent.at == At::Root {
            return Ok(());
        }
        mem::push(&mut path, Step { node: parent, index: 0 })?;
        return settle(root, store, order, path);
    } else {
        // The last entry pointed here: the entry before it comes down to
        // the end of the subtree before, which the last entry now points
        // to.
        let before = up.remove(index - 1);
        let prev = subnode_of(&before).ok_or(Error::Corrupt)?;
        let last = up.len() - 1;
        up[last] = last_entry(Some(prev))?;
        parent.set_entries(&up)?;
        write_node(root, store, &parent)?;
        (repoint(&before, None)?, (last, prev, true))
    };
    let (at, subtree, rightmost) = moved_to;
    mem::push(&mut path, Step { node: parent, index: at })?;
    for s in edge_path(root, store, subtree, rightmost)? {
        mem::push(&mut path, s)?;
    }
    let Step { node: target, .. } = path.pop().ok_or(Error::Corrupt)?;
    let mut entries = target.entries()?;
    let put = if rightmost { entries.len() - 1 } else { 0 };
    mem::insert(&mut entries, put, down.clone_in()?)?;
    propagate(root, store, path, target, entries)?;
    tidy(root, store, order, &order.key(&down)?)
}

/// Replaces each block on the way to `name` that has no names but a
/// child by that child, from the top down.
fn tidy(root: &mut KVec<u8>, store: &mut dyn Store, order: Order<'_>, name: &[u16]) -> Result<()> {
    for _ in 0..MAX_DEPTH {
        let (path, _) = descend(root, store, order, name)?;
        let mut changed = false;
        for i in 1..path.len() {
            let node = &path[i].node;
            let At::Block(vcn) = node.at else { continue };
            let offsets = node.offsets()?;
            if offsets.len() != 1 {
                continue;
            }
            let Some(child) = subnode_of(node.entry(offsets[0])) else { continue };
            let Step { node: parent, index } = &path[i - 1];
            let mut up = parent.entries()?;
            up[*index] = repoint(&up[*index], Some(child))?;
            let mut parent = Node { at: parent.at, bytes: mem::copy(&parent.bytes)? };
            parent.set_entries(&up)?;
            write_node(root, store, &parent)?;
            store.free(vcn)?;
            changed = true;
            break;
        }
        if !changed {
            return Ok(());
        }
    }
    Err(Error::Corrupt)
}

/// How many levels of blocks the index whose root's value is `root` has
/// below its root: 0 for a root alone.
pub fn depth(root: &[u8], store: &mut dyn Store) -> Result<usize> {
    let mut node = read_node(root, store, At::Root)?;
    for depth in 0..MAX_DEPTH {
        let offsets = node.offsets()?;
        let Some(vcn) = subnode_of(node.entry(offsets[0])) else { return Ok(depth) };
        node = read_node(root, store, At::Block(vcn))?;
    }
    Err(Error::Corrupt)
}

/// A node being walked: its entries' offsets, the one to visit next, and
/// whether that one's subtree was visited.
struct Visit {
    node: Node,
    offsets: KVec<usize>,
    next: usize,
    below: bool,
}

/// Calls `f` with each entry of the index whose root's value is `root`
/// in order, those whose names come after `after` (all, for none), but
/// no node's last; stops once `f` returns false.
pub fn walk(
    root: &[u8],
    store: &mut dyn Store,
    order: Order<'_>,
    after: Option<&[u16]>,
    f: &mut dyn FnMut(&[u8]) -> Result<bool>,
) -> Result<()> {
    let mut stack: KVec<Visit> = mem::vec();
    match after {
        Some(name) => {
            let (path, found) = descend(root, store, order, name)?;
            let last = path.len() - 1;
            for (i, Step { node, index }) in path.into_iter().enumerate() {
                let offsets = node.offsets()?;
                // The nodes above have the way down's entry next, its
                // subtree being walked; the node `name` is in has the
                // entry after it next, with its subtree.
                let (next, below) =
                    if i == last && found { (index + 1, false) } else { (index, true) };
                mem::push(&mut stack, Visit { node, offsets, next, below })?;
            }
        }
        None => {
            let node = read_node(root, store, At::Root)?;
            let offsets = node.offsets()?;
            mem::push(&mut stack, Visit { node, offsets, next: 0, below: false })?;
        }
    }
    while let Some(top) = stack.last_mut() {
        let Some(&at) = top.offsets.get(top.next) else {
            stack.pop();
            continue;
        };
        let e = top.node.entry(at);
        if !top.below
            && let Some(vcn) = subnode_of(e)
        {
            top.below = true;
            if stack.len() >= MAX_DEPTH {
                return Err(Error::Corrupt);
            }
            let node = read_node(root, store, At::Block(vcn))?;
            let offsets = node.offsets()?;
            mem::push(&mut stack, Visit { node, offsets, next: 0, below: false })?;
            continue;
        }
        if is_last(e) {
            stack.pop();
            continue;
        }
        if !f(e)? {
            return Ok(());
        }
        top.next += 1;
        top.below = false;
    }
    Ok(())
}

/// Changes the key of the entry of `name`, for the file `reference`, in
/// place: `patch` gets the `$FILE_NAME` it holds, whose length stays.
/// Fails with [`Error::NotFound`] if it is not there.
pub fn update(
    root: &mut KVec<u8>,
    store: &mut dyn Store,
    order: Order<'_>,
    name: &[u16],
    reference: u64,
    patch: &mut dyn FnMut(&mut [u8]),
) -> Result<()> {
    let (mut path, found) = descend(root, store, order, name)?;
    let Some(Step { mut node, index }) = path.pop().filter(|_| found) else {
        return Err(Error::NotFound);
    };
    let at = node.offsets()?[index];
    let e = node.entry(at);
    if !order.is_of(e, reference) {
        return Err(Error::NotFound);
    }
    let key_len = usize::from(u16_at(e, 0x0a));
    patch(&mut node.bytes[at + 0x10..at + 0x10 + key_len]);
    write_node(root, store, &node)
}

/// A fallible copy, for entries.
trait CloneIn: Sized {
    fn clone_in(&self) -> Result<Self>;
}

impl CloneIn for KVec<u8> {
    fn clone_in(&self) -> Result<KVec<u8>> {
        Ok(mem::copy(self)?)
    }
}
