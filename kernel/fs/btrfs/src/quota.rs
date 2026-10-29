// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Simple quotas (ADR 0170), as Linux 6.7 keeps them: each extent counted,
//! for as long as it lives, to the subvolume that wrote it, in that
//! subvolume's quota group, and limits on what a group may hold.
//!
//! The quota tree has a status item, and an info and a limit item for each
//! group. A data extent names its owner in an inline reference that comes
//! first; a tree block's owner is in its header. Only extents of a
//! transaction from the status item's `enable_gen` on count, as only they
//! can name their owner. Linux's full quota groups, which Radix does not
//! keep, are marked inconsistent by the first commit that changes the
//! filesystem, as Linux marks them when it cannot keep them, and `btrfs
//! quota rescan` counts them again.

use core::sync::atomic::Ordering;

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Qgroup, Result};

use crate::cow::Writer;
use crate::files::put_u64;
use crate::format::{Key, item, tree, u64_at};
use crate::refs::{EXTENT_HEADER, OWNER_REF};
use crate::superblock::incompat;
use crate::tree::{Bounds, Cursor, Root, read_block};
use crate::volume::Volume;

/// The quota tree's items, by their key's type: objectid 0, and the
/// group's ID as the offset.
mod kind {
    pub const STATUS: u8 = 240;
    pub const INFO: u8 = 242;
    pub const LIMIT: u8 = 244;
    /// A group's membership of a group above it: objectid the member,
    /// offset the group above, and the other way round.
    pub const RELATION: u8 = 246;
}

/// The status item's flags.
mod status {
    pub const ON: u64 = 1 << 0;
    pub const INCONSISTENT: u64 = 1 << 2;
    pub const SIMPLE_MODE: u64 = 1 << 3;
}

/// The limits a limit item's flags set.
mod limit {
    pub const MAX_RFER: u64 = 1 << 0;
    pub const MAX_EXCL: u64 = 1 << 1;
}

/// The size of every item of the quota tree: a status item of simple
/// quotas, with its `enable_gen`, an info item, a limit item.
const ITEM: usize = 40;

/// The status item's version.
const VERSION: u64 = 1;

/// Where a root item keeps the fields of a new tree's.
mod root_at {
    pub const GENERATION: usize = 160;
    pub const BYTENR: usize = 176;
    pub const REFS: usize = 216;
    pub const LEVEL: usize = 238;
    pub const GENERATION_V2: usize = 239;
    pub const SIZE: usize = 439;
}

/// A quota group: a subvolume's, at level 0, or one above them.
#[derive(Clone, Copy, Default)]
struct Group {
    id: u64,
    /// Its referenced and exclusive bytes; simple quotas keep them
    /// alike.
    referenced: u64,
    exclusive: u64,
    /// The bytes its subvolume's dirty pages will take when written.
    reserved: u64,
    /// Its limits: which of them it has, and the most it may hold.
    flags: u64,
    max_referenced: u64,
    max_exclusive: u64,
    /// Whether its items lag what the transaction counted.
    dirty: bool,
}

impl Group {
    /// Whether `bytes` more would take it past a limit.
    fn over(&self, bytes: u64) -> bool {
        let past =
            |held: u64, max: u64| held.saturating_add(self.reserved).saturating_add(bytes) > max;
        (self.flags & limit::MAX_RFER != 0 && past(self.referenced, self.max_referenced))
            || (self.flags & limit::MAX_EXCL != 0 && past(self.exclusive, self.max_exclusive))
    }
}

/// A filesystem's quotas, as its transactions keep them.
pub(crate) struct Quotas {
    /// Simple quotas, which Radix keeps; otherwise Linux's full quota
    /// groups, which it only marks inconsistent.
    simple: bool,
    /// The first transaction whose extents count.
    enable_gen: u64,
    /// Whether the status item says the counts are inconsistent.
    inconsistent: bool,
    groups: KVec<Group>,
    /// Each group's membership of a group above it: (member, above).
    relations: KVec<(u64, u64)>,
}

impl Quotas {
    /// The quotas of `volume`, if its quota tree says they are on.
    pub(crate) fn load(volume: &Volume) -> Result<Option<Quotas>> {
        let Some(root) = volume.tree(tree::QUOTA)? else { return Ok(None) };
        let mut quotas = Quotas {
            simple: false,
            enable_gen: 0,
            inconsistent: false,
            groups: mem::vec(),
            relations: mem::vec(),
        };
        let mut on = false;
        let mut cursor = Cursor::seek(volume, &root, &Key::new(0, 0, 0))?;
        while let Some((key, data)) = cursor.get() {
            let at =
                |offset| data.get(offset..offset + 8).map(|b| u64_at(b, 0)).ok_or(Error::Corrupt);
            match key.kind {
                kind::STATUS => {
                    let flags = at(16)?;
                    on = flags & status::ON != 0;
                    quotas.simple = flags & status::SIMPLE_MODE != 0;
                    quotas.inconsistent = flags & status::INCONSISTENT != 0;
                    if quotas.simple {
                        quotas.enable_gen = at(32)?;
                    }
                }
                kind::INFO if key.objectid == 0 => {
                    let (referenced, exclusive) = (at(8)?, at(24)?);
                    let group = quotas.group(key.offset)?;
                    (group.referenced, group.exclusive) = (referenced, exclusive);
                }
                kind::LIMIT if key.objectid == 0 => {
                    let (flags, max_referenced, max_exclusive) = (at(0)?, at(8)?, at(16)?);
                    let group = quotas.group(key.offset)?;
                    group.flags = flags & (limit::MAX_RFER | limit::MAX_EXCL);
                    (group.max_referenced, group.max_exclusive) = (max_referenced, max_exclusive);
                }
                kind::RELATION if key.objectid < key.offset => {
                    mem::push(&mut quotas.relations, (key.objectid, key.offset))?;
                }
                _ => {}
            }
            cursor.advance()?;
        }
        for g in &mut quotas.groups {
            g.dirty = false;
        }
        Ok(on.then_some(quotas))
    }

    /// Group `id`, made at zero if it has none.
    fn group(&mut self, id: u64) -> Result<&mut Group> {
        let at = match self.groups.binary_search_by_key(&id, |g| g.id) {
            Ok(at) => at,
            Err(at) => {
                mem::insert(&mut self.groups, at, Group { id, dirty: true, ..Group::default() })?;
                at
            }
        };
        Ok(&mut self.groups[at])
    }

    /// Adds `delta` bytes to group `id` and to every group above it, as
    /// Linux's `btrfs_record_squota_delta` does: a count that would go
    /// below zero stops at zero.
    fn add(&mut self, id: u64, delta: i64) -> Result<()> {
        let mut todo: KVec<u64> = mem::vec();
        mem::push(&mut todo, id)?;
        let mut done = 0;
        while let Some(&next) = todo.get(done) {
            done += 1;
            let group = self.group(next)?;
            group.referenced = group.referenced.saturating_add_signed(delta);
            group.exclusive = group.exclusive.saturating_add_signed(delta);
            group.dirty = true;
            for &(member, above) in &self.relations {
                if member == next && !todo.contains(&above) {
                    mem::push(&mut todo, above)?;
                }
            }
        }
        Ok(())
    }

    /// Whether some group has a limit.
    fn limited(&self) -> bool {
        self.groups.iter().any(|g| g.flags != 0)
    }
}

/// The owner an extent item `data` names, if a data extent's first
/// inline reference names one.
pub(crate) fn owner_of(data: &[u8]) -> Option<u64> {
    (data.get(EXTENT_HEADER) == Some(&OWNER_REF))
        .then(|| data.get(EXTENT_HEADER + 1..EXTENT_HEADER + 9).map(|b| u64_at(b, 0)))
        .flatten()
}

/// Whether `volume` keeps simple quotas: whether its new data extents
/// name their owner.
pub(crate) fn simple(volume: &Volume) -> bool {
    volume.quotas().lock().as_ref().is_some_and(|q| q.simple)
}

/// Reserves room in subvolume `subvol`'s group for `bytes` of dirty
/// pages; fails with [`Error::QuotaExceeded`] if that takes it past a
/// limit. Nothing is kept while no group has a limit.
pub(crate) fn reserve(volume: &Volume, subvol: u64, bytes: u64) -> Result<()> {
    if !volume.quota_limited().load(Ordering::Acquire) {
        return Ok(());
    }
    let mut quotas = volume.quotas().lock();
    let Some(q) = quotas.as_mut().filter(|q| q.simple) else { return Ok(()) };
    let Ok(at) = q.groups.binary_search_by_key(&subvol, |g| g.id) else { return Ok(()) };
    let group = &mut q.groups[at];
    if group.over(bytes) {
        return Err(Error::QuotaExceeded);
    }
    group.reserved += bytes;
    Ok(())
}

/// Gives back the room [`reserve`] took for `bytes` in subvolume
/// `subvol`'s group.
pub(crate) fn release(volume: &Volume, subvol: u64, bytes: u64) {
    if !volume.quota_limited().load(Ordering::Acquire) {
        return;
    }
    let mut quotas = volume.quotas().lock();
    let Some(q) = quotas.as_mut() else { return };
    if let Ok(at) = q.groups.binary_search_by_key(&subvol, |g| g.id) {
        q.groups[at].reserved = q.groups[at].reserved.saturating_sub(bytes);
    }
}

/// The quota groups of `volume`, by ID; fails with
/// [`Error::NotSupported`] when quotas are off.
pub(crate) fn qgroups(volume: &Volume, writable: bool) -> Result<KVec<Qgroup>> {
    if let Some(q) = volume.quotas().lock().as_ref() {
        return list(q);
    }
    // A read-only mount keeps none: they are as the disk has them.
    match Quotas::load(volume)? {
        Some(q) if !writable => list(&q),
        _ => Err(Error::NotSupported),
    }
}

/// The groups of `q`, as [`qgroups`] gives them.
fn list(q: &Quotas) -> Result<KVec<Qgroup>> {
    let mut out = mem::vec();
    for g in &q.groups {
        let max = |flag, max| if g.flags & flag != 0 { max } else { u64::MAX };
        mem::push(
            &mut out,
            Qgroup {
                id: g.id,
                referenced: g.referenced,
                exclusive: g.exclusive,
                max_referenced: max(limit::MAX_RFER, g.max_referenced),
                max_exclusive: max(limit::MAX_EXCL, g.max_exclusive),
            },
        )?;
    }
    Ok(out)
}

/// Loads the quotas of `volume`, which a writable mount keeps.
pub(crate) fn start(volume: &Volume) -> Result<()> {
    let quotas = Quotas::load(volume)?;
    let limited = quotas.as_ref().is_some_and(|q| q.simple && q.limited());
    *volume.quotas().lock() = quotas;
    volume.quota_limited().store(limited, Ordering::Release);
    Ok(())
}

impl Writer {
    /// Counts `delta` bytes of an extent of transaction `generation` that
    /// tree `owner` wrote, made or freed, to its group, if simple quotas
    /// count it: an extent of a subvolume, from `enable_gen` on.
    pub(crate) fn account(volume: &Volume, owner: u64, generation: u64, delta: i64) -> Result<()> {
        if !Writer::is_subvolume(owner) {
            return Ok(());
        }
        let mut quotas = volume.quotas().lock();
        match quotas.as_mut() {
            Some(q) if q.simple && generation >= q.enable_gen => q.add(owner, delta),
            _ => Ok(()),
        }
    }

    /// Writes the items of the groups whose counts changed, and the
    /// status item, before the commit: a commit of a filesystem with
    /// Linux's full quota groups marks them inconsistent. The commit
    /// changes no subvolume, so no count changes after.
    pub(crate) fn write_quotas(&mut self, volume: &Volume) -> Result<()> {
        let (changed, mark) = {
            let mut quotas = volume.quotas().lock();
            let Some(q) = quotas.as_mut() else { return Ok(()) };
            let mut changed = mem::vec();
            if q.simple {
                for g in q.groups.iter_mut().filter(|g| g.dirty) {
                    mem::push(&mut changed, *g)?;
                    g.dirty = false;
                }
            }
            let mark = !q.simple && !q.inconsistent;
            q.inconsistent |= mark;
            (changed, mark)
        };
        if changed.is_empty() && !mark {
            return Ok(());
        }
        let generation = self.generation;
        for g in &changed {
            let info = Key::new(0, kind::INFO, g.id);
            let fill = |d: &mut [u8]| {
                put_u64(d, 0, generation);
                put_u64(d, 8, g.referenced);
                put_u64(d, 16, g.referenced);
                put_u64(d, 24, g.exclusive);
                put_u64(d, 32, g.exclusive);
            };
            match self.update(volume, tree::QUOTA, &info, fill) {
                Err(Error::NotFound) => {
                    self.insert_with(volume, tree::QUOTA, &info, ITEM, |d| {
                        d.fill(0);
                        fill(d);
                    })?;
                    self.write_limit(volume, g)?;
                }
                done => done?,
            }
        }
        let mut bad = false;
        self.update(volume, tree::QUOTA, &Key::new(0, kind::STATUS, 0), |d| {
            if d.len() < 32 {
                bad = true;
                return;
            }
            put_u64(d, 8, generation);
            if mark {
                put_u64(d, 16, u64_at(d, 16) | status::INCONSISTENT);
            }
        })?;
        if bad { Err(Error::Corrupt) } else { Ok(()) }
    }

    /// Writes group `g`'s limit item.
    fn write_limit(&mut self, volume: &Volume, g: &Group) -> Result<()> {
        let key = Key::new(0, kind::LIMIT, g.id);
        let fill = |d: &mut [u8]| {
            put_u64(d, 0, g.flags);
            put_u64(d, 8, g.max_referenced);
            put_u64(d, 16, g.max_exclusive);
        };
        match self.update(volume, tree::QUOTA, &key, fill) {
            Err(Error::NotFound) => self.insert_with(volume, tree::QUOTA, &key, ITEM, |d| {
                d.fill(0);
                fill(d);
            }),
            done => done,
        }
    }

    /// Turns simple quotas on, as Linux's `btrfs quota enable --simple`
    /// does: a quota tree with its status item and a group at zero for
    /// each subvolume. Extents count from the next transaction on, as on
    /// Linux. Fails with [`Error::NotSupported`] if Linux's full quota
    /// groups are on.
    pub(crate) fn enable_quotas(&mut self, volume: &Volume) -> Result<()> {
        if let Some(q) = volume.quotas().lock().as_ref() {
            return if q.simple { Ok(()) } else { Err(Error::NotSupported) };
        }
        if volume.tree(tree::QUOTA)?.is_some() {
            // A quota tree whose status says they are off.
            return Err(Error::NotSupported);
        }
        let generation = self.generation;
        let leaf = self.new_root_leaf(volume, tree::QUOTA, &volume.root_tree())?;
        // As Linux's `btrfs_create_tree`: its bytes used are the commit's
        // to count.
        let key = Key::new(tree::QUOTA, item::ROOT_ITEM, 0);
        self.insert_with(volume, tree::ROOT, &key, root_at::SIZE, |d| {
            d.fill(0);
            put_u64(d, root_at::GENERATION, generation);
            put_u64(d, root_at::BYTENR, leaf.bytenr);
            d[root_at::REFS..root_at::REFS + 4].copy_from_slice(&1u32.to_le_bytes());
            d[root_at::LEVEL] = leaf.level;
            put_u64(d, root_at::GENERATION_V2, generation);
        })?;
        let enable_gen = generation + 1;
        self.insert_with(volume, tree::QUOTA, &Key::new(0, kind::STATUS, 0), ITEM, |d| {
            put_u64(d, 0, VERSION);
            put_u64(d, 8, generation);
            put_u64(d, 16, status::ON | status::SIMPLE_MODE);
            put_u64(d, 24, 0);
            put_u64(d, 32, enable_gen);
        })?;
        let mut quotas = Quotas {
            simple: true,
            enable_gen,
            inconsistent: false,
            groups: mem::vec(),
            relations: mem::vec(),
        };
        for id in subvolumes(volume)? {
            quotas.group(id)?;
        }
        *volume.quotas().lock() = Some(quotas);
        self.incompat |= incompat::SIMPLE_QUOTA;
        self.incompat_off &= !incompat::SIMPLE_QUOTA;
        Ok(())
    }

    /// Turns simple quotas off, as Linux's `btrfs quota disable` does:
    /// the quota tree goes, its blocks as a dropped tree's do; data
    /// extents keep naming their owner. Does nothing if they are off;
    /// fails with [`Error::NotSupported`] if Linux's full quota groups
    /// are on.
    pub(crate) fn disable_quotas(&mut self, volume: &Volume) -> Result<()> {
        {
            let mut quotas = volume.quotas().lock();
            if quotas.as_ref().is_some_and(|q| !q.simple) {
                return Err(Error::NotSupported);
            }
            *quotas = None;
        }
        volume.quota_limited().store(false, Ordering::Release);
        self.incompat &= !incompat::SIMPLE_QUOTA;
        self.incompat_off |= incompat::SIMPLE_QUOTA;
        let Some(top) = volume.tree(tree::QUOTA)? else { return Ok(()) };
        // Every block, each the quota tree's alone: the caller committed
        // first, so all are on the disk.
        let mut todo: KVec<Root> = mem::vec();
        mem::push(&mut todo, top)?;
        while let Some(root) = todo.pop() {
            let block = read_block(volume, &root, &Bounds::default())?;
            if !block.is_leaf() {
                for slot in 0..block.nritems() {
                    mem::push(&mut todo, block.child(slot))?;
                }
            }
            let key = Writer::block_key(root.bytenr, root.level);
            if !crate::refs::Backref::Tree(tree::QUOTA)
                .only(&Writer::extent_item(volume, &key)?.ok_or(Error::Corrupt)?)?
            {
                return Err(Error::Corrupt);
            }
            self.delete(volume, tree::EXTENT, &key)?;
            self.free_dropped(volume, root.bytenr, tree::QUOTA, root.generation)?;
        }
        let key = Writer::root_item_key(volume, tree::QUOTA)?;
        self.delete(volume, tree::ROOT, &key)?;
        self.forget_tree(tree::QUOTA);
        Ok(())
    }

    /// Sets the limits of group `id`: all ones is none. Fails with
    /// [`Error::NotFound`] if there is no such group, and with
    /// [`Error::NotSupported`] unless simple quotas are on.
    pub(crate) fn limit_qgroup(
        &mut self,
        volume: &Volume,
        id: u64,
        max_referenced: u64,
        max_exclusive: u64,
    ) -> Result<()> {
        let group = {
            let mut quotas = volume.quotas().lock();
            let q = quotas.as_mut().filter(|q| q.simple).ok_or(Error::NotSupported)?;
            let at = q.groups.binary_search_by_key(&id, |g| g.id).map_err(|_| Error::NotFound)?;
            let g = &mut q.groups[at];
            g.flags = 0;
            if max_referenced != u64::MAX {
                g.flags |= limit::MAX_RFER;
            }
            if max_exclusive != u64::MAX {
                g.flags |= limit::MAX_EXCL;
            }
            g.max_referenced = if max_referenced == u64::MAX { 0 } else { max_referenced };
            g.max_exclusive = if max_exclusive == u64::MAX { 0 } else { max_exclusive };
            let group = *g;
            volume.quota_limited().store(q.limited(), Ordering::Release);
            group
        };
        self.write_limit(volume, &group)
    }
}

/// Every subvolume of `volume`: the top one, and each root item of a tree
/// numbered as a subvolume is.
fn subvolumes(volume: &Volume) -> Result<KVec<u64>> {
    let mut ids = mem::vec();
    mem::push(&mut ids, tree::FS)?;
    let mut cursor =
        Cursor::seek(volume, &volume.root_tree(), &Key::first(tree::FIRST_FREE, item::ROOT_ITEM))?;
    while let Some((key, _)) = cursor.get() {
        if !Writer::is_subvolume(key.objectid) {
            break;
        }
        if key.kind == item::ROOT_ITEM && ids.last() != Some(&key.objectid) {
            mem::push(&mut ids, key.objectid)?;
        }
        cursor.advance()?;
    }
    Ok(ids)
}
