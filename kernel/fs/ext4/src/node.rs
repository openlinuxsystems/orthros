// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ext4 files, directories and links as VFS nodes.
//!
//! A node keeps its inode in memory whole, with its extents. A change
//! edits them and writes the inode, and the tree if its extents changed,
//! into the running transaction. A file's data goes through its page
//! cache: its size lives there while the node is in memory and follows
//! into the inode with the change time at the next sync, the node kept
//! in memory until then. Blocks are taken for a file's pages only when
//! they are written back (delayed allocation): writing promises room
//! first, so a full volume fails the write, not the write-back.
//!
//! A file removed while open goes on the orphan list and keeps its blocks
//! until its last reference goes; then the next commit frees it. Hard
//! links, extended attributes and owners Linux made are kept as they are:
//! Radix makes none (files get mode 644, directories 755, owner root).

use alloc::sync::Weak;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_block::mem::CachePage;
use radix_block::{PAGE_SIZE, Page, SECTOR, Segment, SegmentMut};
use radix_sync::{Mutex, Once};
use radix_vfs::acl::{self, Acl, AclCell};
use radix_vfs::cache::{Backing, FileCache, page_len};
use radix_vfs::mem::{self, Alloc, KArc, KVec};
use radix_vfs::path::check_name;
use radix_vfs::{
    Access, AccessChange, Error, In, Kind, ListFn, Node, NodeRef, Out, Result, Setting, Stat, now,
};

use crate::dir;
use crate::extent::{self, Extent, Extents, Found, MAX_LEN, TreeIo};
use crate::fs::{Inner, Scratch, Txn, zeroed};
use crate::inode::{self, Inode, flags, mode};
use crate::sb::{self, incompat, ro_compat};
use crate::xattr;

const PAGE: u64 = PAGE_SIZE as u64;

/// The most pages one request to the disk moves.
const MAX_SEGMENTS: usize = 256;

/// The most links a directory counts, as on Linux (`EXT4_LINK_MAX`).
const LINK_MAX: u16 = 65000;

/// What a node knows about itself.
pub(crate) struct Meta {
    pub(crate) inode: Inode,
    /// The inode's bytes.
    raw: KVec<u8>,
    pub(crate) extents: Extents,
    /// The blocks its tree is in (or its block map's indirect blocks).
    tree: KVec<u64>,
    /// How many of those are leaves, in the order Radix writes a tree; 0
    /// for a tree Radix did not write.
    leaves: usize,
    /// Its blocks are in an old block map, given extents at its first
    /// change.
    mapped: bool,
    /// The first extent changed since its tree was written.
    from: Option<usize>,
    /// The blocks of data it has.
    data_blocks: u64,
    /// Its checksums' seed.
    seed: u32,
    /// Whether the inode on the disk is behind.
    dirty: bool,
    /// How many times it fell behind, so that a change made while it is
    /// stored is stored again.
    changes: u64,
}

impl Meta {
    fn behind(&mut self) {
        self.dirty = true;
        self.changes = self.changes.wrapping_add(1);
    }

    /// Notes that extents from `i` on changed.
    fn changed_from(&mut self, i: usize) {
        self.from = Some(self.from.map_or(i, |f| f.min(i)));
    }
}

/// An ext4 file, directory or link.
pub struct Ext4Node {
    pub(crate) fs: KArc<Inner>,
    ino: u32,
    kind: Kind,
    pub(crate) meta: Mutex<Meta>,
    /// A file's data.
    cache: Option<FileCache>,
    me: Once<Weak<Ext4Node, Alloc>>,
    pinned: AtomicBool,
    /// Its access list, once read.
    acl: AclCell,
    /// A directory's default access list, once read.
    default_acl: AclCell,
}

/// Where a directory entry is.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Place {
    l: u64,
    block: u64,
    offset: usize,
    ino: u32,
}

/// The tree's side of the volume, for [`extent::Writer`].
struct Io<'a> {
    fs: &'a Inner,
    txn: &'a mut Txn,
    goal: u64,
}

impl TreeIo for Io<'_> {
    fn take(&mut self) -> Result<u64> {
        let (block, _) = self.fs.take_blocks(self.txn, self.goal, 1)?;
        self.goal = block + 1;
        Ok(block)
    }

    fn give(&mut self, block: u64) -> Result<()> {
        self.fs.free_blocks(self.txn, block, 1)
    }

    fn write(&mut self, block: u64, data: &[u8]) -> Result<()> {
        self.fs.write(self.txn, self.fs.geo.offset(block), data)
    }
}

/// A directory's link count with one more subdirectory: past
/// [`LINK_MAX`] it is 1, "many", as with Linux's `dir_nlink`.
fn add_dir_link(links: u16, nlink: bool) -> Result<u16> {
    match links {
        1 if nlink => Ok(1),
        l if l >= LINK_MAX && nlink => Ok(1),
        l if l >= LINK_MAX => Err(Error::NoSpace),
        l => Ok(l + 1),
    }
}

/// A directory's link count with one subdirectory fewer.
fn drop_dir_link(links: u16) -> u16 {
    if links > 2 { links - 1 } else { links }
}

impl Ext4Node {
    fn new(
        fs: KArc<Inner>,
        ino: u32,
        kind: Kind,
        meta: Meta,
        made: bool,
    ) -> Result<KArc<Ext4Node>> {
        // One just made has no lists until its directory's default gives
        // them, which sets them as attributes and so forgets these.
        let list = || if made { AclCell::none() } else { AclCell::unread() };
        let cache = (kind == Kind::File).then(|| FileCache::new(meta.inode.size, meta.inode.mtime));
        let node = mem::arc(Ext4Node {
            fs,
            ino,
            kind,
            meta: Mutex::new(meta),
            cache,
            me: Once::new(),
            pinned: AtomicBool::new(false),
            acl: list(),
            default_acl: list(),
        })?;
        let _ = node.me.set(KArc::downgrade(&node));
        Ok(node)
    }

    /// Reads inode `ino` from the disk, with `buf` for room.
    pub(crate) fn load(fs: KArc<Inner>, ino: u32, buf: &mut [u8]) -> Result<KArc<Ext4Node>> {
        let mut raw = zeroed(fs.geo.inode_size as usize)?;
        fs.read(fs.inode_offset(ino)?, &mut raw)?;
        let inode = Inode::parse(&fs.geo, ino, &raw)?;
        // A name of a freed inode is damage.
        if inode.links == 0 && inode.mode == 0 {
            return Err(Error::Corrupt);
        }
        let kind = inode.kind()?;
        let mut extents = Extents::new();
        let mut tree = mem::vec();
        let has_blocks = matches!(kind, Kind::File | Kind::Directory)
            || kind == Kind::Symlink && !inode.fast_link(&fs.geo);
        let sealed = inode.flags & (flags::INLINE_DATA | flags::EA_INODE) != 0;
        if has_blocks && !sealed {
            fs.load_blocks(ino, &inode, buf, &mut extents, &mut tree)?;
        }
        let mapped = has_blocks && !inode.has_extents();
        let seed = inode::seed(&fs.geo, ino, inode.generation);
        let data_blocks = extents.blocks();
        let meta = Meta {
            inode,
            raw,
            extents,
            tree,
            leaves: 0,
            mapped,
            from: None,
            data_blocks,
            seed,
            dirty: false,
            changes: 0,
        };
        Ext4Node::new(fs, ino, kind, meta, false)
    }

    pub(crate) fn ino(&self) -> u32 {
        self.ino
    }

    fn me(&self) -> Result<KArc<Ext4Node>> {
        self.me.get().and_then(Weak::upgrade).ok_or(Error::NotFound)
    }

    fn file(&self) -> Result<&FileCache> {
        match &self.cache {
            Some(cache) => {
                if self.meta.lock().inode.flags
                    & (flags::ENCRYPT | flags::INLINE_DATA | flags::VERITY)
                    != 0
                {
                    return Err(Error::NotSupported);
                }
                Ok(cache)
            }
            None if self.kind == Kind::Directory => Err(Error::IsDirectory),
            None => Err(Error::NotSupported),
        }
    }

    /// Checks that it is a directory Radix can read: one whose entries are
    /// in the inode (`inline_data`) or encrypted it cannot.
    fn is_dir(&self) -> Result<()> {
        if self.kind != Kind::Directory {
            return Err(Error::NotDirectory);
        }
        if self.meta.lock().inode.flags & (flags::ENCRYPT | flags::INLINE_DATA) != 0 {
            return Err(Error::NotSupported);
        }
        Ok(())
    }

    /// Whether it has no name left.
    fn removed(&self) -> bool {
        self.meta.lock().inode.links == 0
    }

    fn block_size(&self) -> u64 {
        u64::from(self.fs.geo.block_size)
    }

    /// Where its blocks would best start: its inode's group.
    fn home(&self) -> u64 {
        let g = (self.ino - 1) / self.fs.geo.inodes_per_group;
        self.fs.geo.group_start(g)
    }

    /// A directory's entries' seed, for checking its blocks.
    fn dir_seed(&self, meta: &Meta) -> Option<u32> {
        self.fs.geo.metadata_csum().then_some(meta.seed)
    }

    /// Writes its tree, if its extents changed, and its inode into the
    /// transaction.
    fn save(&self, txn: &mut Txn, meta: &mut Meta) -> Result<()> {
        let fs = &*self.fs;
        if let Some(mut from) = meta.from.take() {
            if meta.mapped {
                // The old map's indirect blocks go: it gets a tree.
                for &block in &meta.tree {
                    fs.free_blocks(txn, block, 1)?;
                }
                meta.tree.clear();
                meta.leaves = 0;
                meta.mapped = false;
                meta.inode.flags |= flags::EXTENTS;
                from = 0;
            }
            if meta.leaves == 0 {
                from = 0;
            }
            let goal = meta.extents.as_slice().first().map_or_else(|| self.home(), |e| e.physical);
            let mut buf = core::mem::replace(&mut txn.buf, mem::vec());
            let Meta { extents, tree, inode, leaves, seed, .. } = &mut *meta;
            let stored = extent::Writer {
                geo: &fs.geo,
                seed: *seed,
                io: &mut Io { fs, txn, goal },
                buf: &mut buf,
            }
            .store(extents.as_slice(), from, *leaves, &mut inode.block, tree);
            txn.buf = buf;
            match stored {
                Ok(n) => meta.leaves = n,
                Err(err) => {
                    fs.fail();
                    return Err(err);
                }
            }
        }
        let per = self.block_size() / SECTOR;
        let acl = u64::from(meta.inode.file_acl != 0);
        if meta.inode.has_extents() || meta.mapped {
            meta.inode.sectors = (meta.data_blocks + meta.tree.len() as u64 + acl) * per;
        }
        let Meta { inode, raw, .. } = &mut *meta;
        inode.encode(&fs.geo, self.ino, raw);
        fs.write(txn, fs.inode_offset(self.ino)?, raw)
    }

    /// Sets its attribute of index `index` named `name` to `value`, or
    /// removes it given none ([`xattr::change`]), and its change time
    /// with it. The access list it keeps is read again after a change of
    /// the one that decides.
    fn change_attribute(
        &self,
        index: u8,
        name: &[u8],
        value: Option<(&[u8], Setting)>,
    ) -> Result<()> {
        self.fs.writable()?;
        let changed = (|| {
            let mut txn = self.fs.begin()?;
            let mut meta = self.meta.lock();
            let home = self.home();
            let Meta { raw, inode, .. } = &mut *meta;
            if !xattr::change(&self.fs, &mut txn, raw, inode, home, index, name, value)? {
                return Ok(());
            }
            meta.inode.ctime = now();
            self.save(&mut txn, &mut meta)
        })();
        if xattr::is_access(index) {
            self.acl.forget();
        } else if xattr::is_default(index) {
            self.default_acl.forget();
        }
        changed
    }

    /// Notes that its inode is behind, and keeps it in memory until the
    /// volume syncs. The caller holds no lock.
    fn changed(&self) {
        self.meta.lock().behind();
        if !self.pin() {
            // No memory to remember it: it is written now instead.
            let _ = self.flush();
        }
    }

    /// Keeps it in memory until the volume syncs; false if there was no
    /// memory to.
    pub(crate) fn pin(&self) -> bool {
        if self.pinned.swap(true, Ordering::AcqRel) {
            return true;
        }
        let pinned =
            self.me.get().and_then(Weak::upgrade).is_some_and(|me| self.fs.pin(me).is_ok());
        if !pinned {
            self.pinned.store(false, Ordering::Release);
        }
        pinned
    }

    /// The volume no longer holds it for syncing.
    pub(crate) fn unpinned(&self) {
        self.pinned.store(false, Ordering::Release);
    }

    /// Writes its data, then its inode with the size and change time its
    /// page cache has.
    pub(crate) fn flush(&self) -> Result<()> {
        let seen = self.meta.lock().changes;
        if let Some(cache) = &self.cache
            && !self.removed()
        {
            cache.sync(&FileBacking(self))?;
        }
        let modified = self.cache.as_ref().map(FileCache::modified);
        let mut txn = self.fs.begin()?;
        let mut meta = self.meta.lock();
        if !meta.dirty {
            return Ok(());
        }
        if let Some(modified) = modified
            && modified != meta.inode.mtime
        {
            meta.inode.mtime = modified;
            meta.inode.ctime = meta.inode.ctime.max(modified);
        }
        self.save(&mut txn, &mut meta)?;
        meta.dirty = meta.changes != seen;
        Ok(())
    }

    /// Drops up to `max` clean pages of a file.
    pub(crate) fn shrink(&self, max: usize) -> usize {
        self.cache.as_ref().map_or(0, |cache| cache.shrink(max, &FileBacking(self)))
    }

    /// Sets the next orphan its `dtime` names.
    pub(crate) fn set_next_orphan(&self, txn: &mut Txn, next: u32) -> Result<()> {
        let mut meta = self.meta.lock();
        meta.inode.dtime = next;
        self.save(txn, &mut meta)
    }

    /// Unmaps its blocks from `keep` on, freeing them.
    fn cut(&self, txn: &mut Txn, meta: &mut Meta, keep: u64) -> Result<()> {
        self.punch(txn, meta, keep, (1 << 32) - keep.min(1 << 32))
    }

    /// Unmaps `n` of its blocks from `l`, freeing them.
    fn punch(&self, txn: &mut Txn, meta: &mut Meta, l: u64, n: u64) -> Result<()> {
        if l >= 1 << 32 || n == 0 {
            return Ok(());
        }
        let mut freed: KVec<(u64, u64)> = mem::vec();
        let mut lost = Ok(());
        let i = meta.extents.punch(l, n, &mut |p, n| {
            if lost.is_ok() {
                lost = mem::push(&mut freed, (p, n)).map_err(Error::from);
            }
        })?;
        if lost.is_err() {
            self.fs.fail();
            return lost;
        }
        if freed.is_empty() {
            return Ok(());
        }
        for &(p, n) in &freed {
            meta.data_blocks = meta.data_blocks.saturating_sub(n);
            self.fs.free_blocks(txn, p, n)?;
        }
        meta.changed_from(i);
        Ok(())
    }

    /// Cuts an orphan Linux left with links to its size, at mount.
    pub(crate) fn cut_orphan(&self, txn: &mut Txn) -> Result<()> {
        let mut meta = self.meta.lock();
        let keep = meta.inode.size.div_ceil(self.block_size());
        self.cut(txn, &mut meta, keep)?;
        self.save(txn, &mut meta)
    }

    /// Its last link went: onto the orphan list, until its last reference
    /// goes.
    fn unlinked(&self, txn: &mut Txn) -> Result<()> {
        let mut meta = self.meta.lock();
        meta.inode.links =
            if self.kind == Kind::Directory { 0 } else { meta.inode.links.saturating_sub(1) };
        meta.inode.ctime = now();
        if meta.inode.links == 0 {
            meta.inode.dtime = txn.add_orphan(self.ino)?;
        }
        self.save(txn, &mut meta)
    }
}

/// A directory's blocks.
impl Ext4Node {
    /// The disk block of its block `l`, if it has one.
    fn dir_block(&self, l: u64) -> Option<u64> {
        let meta = self.meta.lock();
        if l >= 1 << 32 {
            return None;
        }
        match meta.extents.find(l as u32) {
            Found::Mapped { physical, unwritten: false, .. } => Some(physical),
            _ => None,
        }
    }

    fn dir_blocks(&self) -> u64 {
        self.meta.lock().inode.size / self.block_size()
    }

    /// Reads its block `l`, at disk block `block`, into `buf`, checked.
    /// An indexed directory's index blocks hold no entries but `.` and
    /// `..`, and are checked as index blocks: Radix lists and finds
    /// entries without the index.
    fn read_dir_block(&self, l: u64, block: u64, buf: &mut [u8]) -> Result<()> {
        self.fs.read(self.fs.geo.offset(block), buf)?;
        let meta = self.meta.lock();
        let seed = self.dir_seed(&meta);
        if meta.inode.flags & flags::INDEX != 0 {
            if l == 0 {
                return dir::check_root(buf, seed);
            }
            if dir::is_index_node(buf) {
                return seed.map_or(Ok(()), |seed| dir::check_node(buf, seed));
            }
        }
        dir::check(buf, seed, self.fs.geo.inodes)
    }

    /// The entry named `name`. The caller holds `ns`.
    fn find_entry(&self, ns: &mut Scratch, name: &str) -> Result<Option<Place>> {
        let filetype = self.fs.geo.filetype();
        for l in 0..self.dir_blocks() {
            let Some(block) = self.dir_block(l) else { continue };
            self.read_dir_block(l, block, &mut ns.a)?;
            if let Some(e) = dir::find(&ns.a, filetype, name.as_bytes()) {
                return Ok(Some(Place { l, block, offset: e.offset, ino: e.ino }));
            }
        }
        Ok(None)
    }

    /// Whether it holds no entry but `.` and `..`.
    fn is_empty_dir(&self, ns: &mut Scratch) -> Result<bool> {
        let filetype = self.fs.geo.filetype();
        for l in 0..self.dir_blocks() {
            let Some(block) = self.dir_block(l) else { continue };
            self.read_dir_block(l, block, &mut ns.a)?;
            let mut empty = true;
            dir::for_each(&ns.a, filetype, |e| {
                empty = e.name == b"." || e.name == b"..";
                empty
            });
            if !empty {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Writes `buf`, its block at disk block `block`, with its checksum.
    fn write_dir_block(&self, txn: &mut Txn, block: u64, buf: &mut [u8], seed: u32) -> Result<()> {
        dir::seal(buf, seed);
        self.fs.write(txn, self.fs.geo.offset(block), buf)
    }

    /// Drops its hash index, before it changes: its index blocks become
    /// empty ones (see [`crate::dir`]).
    fn unindex(&self, txn: &mut Txn, ns: &mut Scratch, meta: &mut Meta) -> Result<()> {
        if meta.inode.flags & flags::INDEX == 0 {
            return Ok(());
        }
        let geo = &self.fs.geo;
        let (filetype, tail) = (geo.filetype(), geo.metadata_csum());
        let bs = self.block_size();
        for l in 0..meta.inode.size / bs {
            let Found::Mapped { physical, unwritten: false, .. } = meta.extents.find(l as u32)
            else {
                continue;
            };
            self.fs.read(geo.offset(physical), &mut ns.a)?;
            if l == 0 {
                dir::unindex_root(&mut ns.a, filetype, tail)?;
            } else if dir::is_index_node(&ns.a) {
                dir::empty(&mut ns.a, tail);
            } else {
                continue;
            }
            self.write_dir_block(txn, physical, &mut ns.a, meta.seed)?;
        }
        meta.inode.flags &= !flags::INDEX;
        self.save(txn, meta)
    }

    /// Adds an entry naming inode `ino` `name`, of type `kind`, growing
    /// the directory by a block if no block has room.
    fn add_entry(
        &self,
        txn: &mut Txn,
        ns: &mut Scratch,
        name: &str,
        ino: u32,
        kind: u8,
    ) -> Result<()> {
        let fs = &*self.fs;
        let geo = &fs.geo;
        let (filetype, tail) = (geo.filetype(), geo.metadata_csum());
        let mut meta = self.meta.lock();
        self.unindex(txn, ns, &mut meta)?;
        let bs = self.block_size();
        let blocks = meta.inode.size / bs;
        for l in 0..blocks {
            let Found::Mapped { physical, unwritten: false, .. } = meta.extents.find(l as u32)
            else {
                continue;
            };
            fs.read(geo.offset(physical), &mut ns.a)?;
            dir::check(&ns.a, self.dir_seed(&meta), geo.inodes)?;
            if dir::insert(&mut ns.a, filetype, name.as_bytes(), ino, kind) {
                return self.write_dir_block(txn, physical, &mut ns.a, meta.seed);
            }
        }
        let limit =
            if geo.has_incompat(incompat::LARGEDIR) { 1 << 32 } else { u64::from(u32::MAX) / bs };
        if blocks + 1 >= limit {
            return Err(Error::NoSpace);
        }
        let goal = meta.extents.goal(blocks as u32).unwrap_or_else(|| self.home());
        let (block, _) = fs.take_blocks(txn, goal, 1)?;
        let i = meta.extents.insert(Extent {
            logical: blocks as u32,
            len: 1,
            physical: block,
            unwritten: false,
        })?;
        meta.changed_from(i);
        meta.data_blocks += 1;
        meta.inode.size += bs;
        dir::empty(&mut ns.a, tail);
        dir::insert(&mut ns.a, filetype, name.as_bytes(), ino, kind);
        self.write_dir_block(txn, block, &mut ns.a, meta.seed)?;
        self.save(txn, &mut meta)
    }

    /// Removes the entry at `place`.
    fn remove_entry(&self, txn: &mut Txn, ns: &mut Scratch, place: &Place) -> Result<()> {
        let filetype = self.fs.geo.filetype();
        self.read_dir_block(place.l, place.block, &mut ns.a)?;
        dir::remove(&mut ns.a, filetype, place.offset);
        let seed = self.meta.lock().seed;
        self.write_dir_block(txn, place.block, &mut ns.a, seed)
    }

    /// Points the entry at `place` at inode `ino`, of type `kind`.
    fn retarget_entry(
        &self,
        txn: &mut Txn,
        ns: &mut Scratch,
        place: &Place,
        ino: u32,
        kind: u8,
    ) -> Result<()> {
        let filetype = self.fs.geo.filetype();
        self.read_dir_block(place.l, place.block, &mut ns.a)?;
        dir::retarget(&mut ns.a, filetype, place.offset, ino, kind);
        let seed = self.meta.lock().seed;
        self.write_dir_block(txn, place.block, &mut ns.a, seed)
    }

    /// The directory its `..` names.
    fn parent_ino(&self, ns: &mut Scratch) -> Result<u32> {
        let block = self.dir_block(0).ok_or(Error::Corrupt)?;
        self.read_dir_block(0, block, &mut ns.a)?;
        dir::parent_of(&ns.a, self.fs.geo.filetype()).ok_or(Error::Corrupt)
    }

    /// Points its `..` at `parent`.
    fn set_parent(&self, txn: &mut Txn, ns: &mut Scratch, parent: u32) -> Result<()> {
        {
            let mut meta = self.meta.lock();
            self.unindex(txn, ns, &mut meta)?;
        }
        let block = self.dir_block(0).ok_or(Error::Corrupt)?;
        self.read_dir_block(0, block, &mut ns.a)?;
        dir::set_parent(&mut ns.a, self.fs.geo.filetype(), parent)?;
        let seed = self.meta.lock().seed;
        self.write_dir_block(txn, block, &mut ns.a, seed)
    }

    /// Whether it is directory `ino` or under it.
    fn under(&self, ns: &mut Scratch, ino: u32) -> Result<bool> {
        let mut at = self.ino;
        for _ in 0..self.fs.geo.inodes {
            if at == ino {
                return Ok(true);
            }
            if at == sb::ROOT {
                return Ok(false);
            }
            let node = Inner::node_for(&self.fs, at, &mut ns.b)?;
            at = node.parent_ino(ns)?;
        }
        Err(Error::Corrupt)
    }

    /// Changes the time and, by `links`, the link count of a directory
    /// whose entries changed.
    fn touch_dir(&self, txn: &mut Txn, links: i8) -> Result<()> {
        let nlink = self.fs.geo.has_ro_compat(ro_compat::DIR_NLINK);
        let mut meta = self.meta.lock();
        match links {
            1 => meta.inode.links = add_dir_link(meta.inode.links, nlink)?,
            -1 => meta.inode.links = drop_dir_link(meta.inode.links),
            _ => {}
        }
        let time = now();
        meta.inode.mtime = time;
        meta.inode.ctime = time;
        self.save(txn, &mut meta)
    }

    /// Makes a file, directory or link `name` in it, with `access`'s owner,
    /// group and permissions; `target` is a link's.
    fn make(
        &self,
        ns: &mut Scratch,
        name: &str,
        kind: Kind,
        access: Access,
        target: Option<&[u8]>,
    ) -> Result<KArc<Ext4Node>> {
        let fs = &self.fs;
        let geo = &fs.geo;
        let dir = kind == Kind::Directory;
        let slow = target.is_some_and(|t| t.len() >= inode::BLOCK_LEN);
        let mut txn = fs.begin()?;
        // A block for the new entry, one for the directory's or the
        // link's, and room for a tree to grow.
        fs.room(&mut txn, 2 + u64::from(dir || slow), 1)?;
        if dir {
            add_dir_link(self.meta.lock().inode.links, geo.has_ro_compat(ro_compat::DIR_NLINK))?;
        }
        let made = (|| {
            let near = if dir && self.ino == sb::ROOT {
                let g = txn.dir_group;
                txn.dir_group = (g + 1) % geo.groups;
                g
            } else {
                (self.ino - 1) / geo.inodes_per_group
            };
            let ino = fs.take_inode(&mut txn, near, dir)?;
            let mut raw = zeroed(geo.inode_size as usize)?;
            inode::fresh(geo, &mut raw);
            let generation = fs.next_generation();
            let time = now();
            let (type_bits, links) = match kind {
                Kind::Directory => (mode::DIR, 2),
                Kind::Symlink => (mode::LINK, 1),
                _ => (mode::FILE, 1),
            };
            let fast = target.is_some() && !slow;
            let mut block = extent::empty_root();
            if let (Some(t), true) = (target, fast) {
                block = [0; inode::BLOCK_LEN];
                block[..t.len()].copy_from_slice(t);
            }
            let inode = Inode {
                mode: type_bits | (access.mode & radix_vfs::mode::ALL),
                uid: access.owner,
                gid: access.group,
                links,
                size: target.map_or(0, |t| t.len() as u64),
                sectors: 0,
                flags: if fast { 0 } else { flags::EXTENTS },
                generation,
                atime: time,
                ctime: time,
                mtime: time,
                crtime: time,
                dtime: 0,
                file_acl: 0,
                block,
            };
            let meta = Meta {
                inode,
                raw,
                extents: Extents::new(),
                tree: mem::vec(),
                leaves: 0,
                mapped: false,
                from: None,
                data_blocks: 0,
                seed: inode::seed(geo, ino, generation),
                dirty: false,
                changes: 0,
            };
            let node = Ext4Node::new(fs.clone(), ino, kind, meta, true)?;
            {
                let mut meta = node.meta.lock();
                if dir || slow {
                    let home = node.home();
                    let (b, _) = fs.take_blocks(&mut txn, home, 1)?;
                    if dir {
                        dir::first(&mut ns.a, geo.filetype(), ino, self.ino, geo.metadata_csum());
                        dir::seal(&mut ns.a, meta.seed);
                        meta.inode.size = self.block_size();
                    } else if let Some(t) = target {
                        ns.a.fill(0);
                        ns.a[..t.len()].copy_from_slice(t);
                    }
                    fs.write(&mut txn, geo.offset(b), &ns.a)?;
                    meta.extents.insert(Extent {
                        logical: 0,
                        len: 1,
                        physical: b,
                        unwritten: false,
                    })?;
                    meta.data_blocks = 1;
                    meta.changed_from(0);
                }
                node.save(&mut txn, &mut meta)?;
            }
            self.add_entry(&mut txn, ns, name, ino, inode::dirent_type(kind))?;
            self.touch_dir(&mut txn, if dir { 1 } else { 0 })?;
            fs.add_node(&node)?;
            Ok(node)
        })();
        if made.is_err() {
            fs.fail();
        }
        made
    }
}

/// Whether `moving` may replace `old`: a file a file, a directory an
/// empty directory.
fn check_replace(moving: Kind, old: &Ext4Node, ns: &mut Scratch) -> Result<()> {
    match (moving, old.kind) {
        (_, Kind::Directory) if !old.is_empty_dir(ns)? => Err(Error::NotEmpty),
        (Kind::Directory, Kind::Directory) => Ok(()),
        (Kind::Directory, _) => Err(Error::NotDirectory),
        (_, Kind::Directory) => Err(Error::IsDirectory),
        _ => Ok(()),
    }
}

/// A file's page cache backing: its blocks.
struct FileBacking<'a>(&'a Ext4Node);

impl FileBacking<'_> {
    /// Blocks of the file in a page.
    fn per_page(&self) -> u64 {
        PAGE / self.0.block_size()
    }

    fn max_segments(&self) -> usize {
        self.0.fs.cache.device().info().max_segments.clamp(1, MAX_SEGMENTS)
    }

    /// Reads page `index`, block by block, `len` bytes of it.
    fn read_blocks(&self, index: u64, page: &mut Page, len: usize) -> Result<()> {
        let node = self.0;
        let bs = node.block_size();
        let device = node.fs.cache.device();
        let mut k = 0u64;
        while k * bs < len as u64 {
            let l = index * self.per_page() + k;
            let found = if l < 1 << 32 {
                node.meta.lock().extents.find(l as u32)
            } else {
                Found::Hole { len: 1 }
            };
            let offset = (k * bs) as usize;
            match found {
                Found::Mapped { physical, unwritten: false, .. } => {
                    let segment = SegmentMut { page: &mut *page, offset, len: bs as usize };
                    device.read(physical * (bs / SECTOR), &mut [segment])?;
                    radix_hw::device_wrote(&mut page.bytes_mut()[offset..offset + bs as usize]);
                }
                _ => page.bytes_mut()[offset..offset + bs as usize].fill(0),
            }
            k += 1;
        }
        page.bytes_mut()[len..].fill(0);
        Ok(())
    }

    /// Writes the file's blocks `l..l + n`, of `pages` (whose first block
    /// is `first`), to the disk's from `physical`.
    fn write_blocks(
        &self,
        pages: &[&Page],
        first: u64,
        l: u64,
        n: u64,
        physical: u64,
    ) -> Result<()> {
        let bs = self.0.block_size();
        let per = self.per_page();
        let device = self.0.fs.cache.device();
        let most = self.max_segments();
        let mut segments: KVec<Segment<'_>> = mem::vec_with_capacity(most.min(n as usize))?;
        let mut start = physical;
        let mut k = 0;
        while k < n {
            let o = l - first + k;
            let page = pages[(o / per) as usize];
            let offset = ((o % per) * bs) as usize;
            let joins = segments
                .last()
                .is_some_and(|s| core::ptr::eq(s.page, page) && s.offset + s.len == offset);
            if joins {
                if let Some(s) = segments.last_mut() {
                    s.len += bs as usize;
                }
            } else {
                if segments.len() == most {
                    device.write(start * (bs / SECTOR), &segments)?;
                    start = physical + k;
                    segments.clear();
                }
                let _ = segments.push_within_capacity(Segment { page, offset, len: bs as usize });
            }
            k += 1;
        }
        if !segments.is_empty() {
            device.write(start * (bs / SECTOR), &segments)?;
        }
        Ok(())
    }
}

impl Backing for FileBacking<'_> {
    fn persistent(&self) -> bool {
        true
    }

    fn read_page(&self, index: u64, page: &mut Page, len: usize) -> Result<()> {
        self.read_blocks(index, page, len)
    }

    fn read_run(
        &self,
        index: u64,
        pages: &mut [CachePage],
        size: u64,
        _more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<usize> {
        let node = self.0;
        if self.per_page() > 1 || index >= 1 << 32 {
            let page = pages.first_mut().ok_or(Error::Io)?;
            self.read_blocks(index, page, page_len(index, size))?;
            return Ok(1);
        }
        let in_file = size.div_ceil(PAGE).saturating_sub(index).max(1);
        let max = (pages.len() as u64).min(in_file).min(self.max_segments() as u64);
        let found = node.meta.lock().extents.find(index as u32);
        let n = match found {
            Found::Mapped { physical, len, unwritten: false } => {
                let n = u64::from(len).min(max) as usize;
                let mut segments = mem::vec_with_capacity(n)?;
                for page in &mut pages[..n] {
                    let _ = segments.push_within_capacity(SegmentMut {
                        page,
                        offset: 0,
                        len: PAGE_SIZE,
                    });
                }
                node.fs.cache.device().read(physical * (PAGE / SECTOR), &mut segments)?;
                for page in &mut pages[..n] {
                    radix_hw::device_wrote(page.bytes_mut());
                }
                n
            }
            Found::Mapped { len, .. } => {
                let n = u64::from(len).min(max) as usize;
                for page in &mut pages[..n] {
                    page.bytes_mut().fill(0);
                }
                n
            }
            Found::Hole { len } => {
                let n = len.min(max) as usize;
                for page in &mut pages[..n] {
                    page.bytes_mut().fill(0);
                }
                n
            }
        };
        // Past the file's end it is zeros.
        for (at, page) in (index..).zip(&mut pages[..n]) {
            let keep = page_len(at, size);
            page.bytes_mut()[keep..].fill(0);
        }
        Ok(n)
    }

    fn run_pages(&self) -> usize {
        radix_vfs::cache::READ_RUN
    }

    fn write_page(&self, index: u64, page: &Page, len: usize) -> Result<()> {
        self.write_pages(index, &[page], len)
    }

    fn write_pages(&self, index: u64, pages: &[&Page], len: usize) -> Result<()> {
        let node = self.0;
        let fs = &*node.fs;
        let Some(last) = pages.len().checked_sub(1) else { return Ok(()) };
        let bs = node.block_size();
        let per = self.per_page();
        let first = index * per;
        let end = first + last as u64 * per + (len as u64).div_ceil(bs);
        if end > 1 << 32 {
            return Err(Error::TooBig);
        }
        let mut txn = fs.begin()?;
        let mut meta = node.meta.lock();
        let mut l = first;
        let mut failed = Ok(());
        while l < end {
            let (physical, n) = match meta.extents.find(l as u32) {
                Found::Mapped { physical, len, unwritten } => {
                    let n = u64::from(len).min(end - l);
                    if unwritten {
                        match meta.extents.set_written(l as u32, n as u32) {
                            Ok(i) => meta.changed_from(i),
                            Err(err) => {
                                failed = Err(err);
                                break;
                            }
                        }
                    }
                    (physical, n)
                }
                Found::Hole { len: hole } => {
                    let want = hole.min(end - l).min(u64::from(MAX_LEN)) as u32;
                    let goal = meta.extents.goal(l as u32).unwrap_or_else(|| node.home());
                    let (start, got) = match fs.take_blocks(&mut txn, goal, want) {
                        Ok(run) => run,
                        Err(err) => {
                            failed = Err(err);
                            break;
                        }
                    };
                    let e =
                        Extent { logical: l as u32, len: got, physical: start, unwritten: false };
                    match meta.extents.insert(e) {
                        Ok(i) => meta.changed_from(i),
                        Err(err) => {
                            let _ = fs.free_blocks(&mut txn, start, u64::from(got));
                            failed = Err(err);
                            break;
                        }
                    }
                    meta.data_blocks += u64::from(got);
                    (start, u64::from(got))
                }
            };
            // The data reaches the disk before the transaction that maps
            // it commits; a failure here would leave the mapping over
            // what the disk held, so the volume is given up.
            if let Err(err) = self.write_blocks(pages, first, l, n, physical) {
                fs.fail();
                return Err(err);
            }
            l += n;
        }
        let saved = node.save(&mut txn, &mut meta);
        failed.and(saved)
    }

    fn reserve(&self, size: u64) -> Result<()> {
        let mut meta = self.0.meta.lock();
        if size > meta.inode.size {
            meta.inode.size = size;
            meta.behind();
        }
        Ok(())
    }

    fn reserve_pages(&self, pages: usize) -> Result<()> {
        self.0.fs.reserve(pages as u64 * self.per_page())
    }

    fn release_pages(&self, pages: usize) {
        self.0.fs.release(pages as u64 * self.per_page());
    }

    fn zero(&self, from: u64, to: u64) -> Result<()> {
        let node = self.0;
        let bs = node.block_size();
        let (l, n) = (from / bs, (to - from) / bs);
        let any = {
            let meta = node.meta.lock();
            l < 1 << 32 && !matches!(meta.extents.find(l as u32), Found::Hole { len } if len >= n)
        };
        if !any {
            return Ok(());
        }
        let mut txn = node.fs.begin()?;
        let mut meta = node.meta.lock();
        node.punch(&mut txn, &mut meta, l, n)?;
        node.save(&mut txn, &mut meta)
    }

    fn truncate(&self, size: u64) -> Result<()> {
        let node = self.0;
        let mut txn = node.fs.begin()?;
        let mut meta = node.meta.lock();
        let keep = size.div_ceil(node.block_size());
        node.cut(&mut txn, &mut meta, keep)?;
        meta.inode.size = size;
        let time = now();
        meta.inode.mtime = time;
        meta.inode.ctime = time;
        node.save(&mut txn, &mut meta)
    }

    fn max_size(&self) -> u64 {
        let geo = &self.0.fs.geo;
        let bs = u64::from(geo.block_size);
        let most = ((1u64 << 32) - 1) * bs;
        if geo.has_ro_compat(ro_compat::HUGE_FILE) {
            most
        } else {
            // Without `huge_file` the sectors it takes must fit 32 bits.
            most.min(((1u64 << 32) - 1) * SECTOR / bs * bs)
        }
    }
}

impl Drop for Ext4Node {
    fn drop(&mut self) {
        // Pages never written back give back the room promised them.
        if let Some(cache) = &self.cache {
            let dirty = cache.dirty_pages() as u64;
            if dirty > 0 {
                self.fs.release(dirty * (PAGE / u64::from(self.fs.geo.block_size)));
            }
        }
        if !self.fs.read_only && self.meta.get_mut().inode.links == 0 {
            let mut doomed = self.fs.doomed.lock();
            // Without memory to note it, the orphan list frees it at the
            // next mount.
            let _ = mem::push(&mut doomed, self.ino);
        }
    }
}

impl Node for Ext4Node {
    fn stat(&self) -> Stat {
        let status = self.cache.as_ref().map(FileCache::status);
        let meta = self.meta.lock();
        let (size, modified) = match status {
            Some((size, modified, _)) => (size, modified),
            None => (meta.inode.size, meta.inode.mtime),
        };
        // A write the cache holds changed the file when it was made.
        let changed = if modified > meta.inode.mtime {
            meta.inode.ctime.max(modified)
        } else {
            meta.inode.ctime
        };
        Stat {
            kind: self.kind,
            size,
            allocated: meta.inode.sectors * SECTOR,
            id: u64::from(self.ino),
            created: meta.inode.crtime,
            modified,
            accessed: meta.inode.atime,
            changed,
            links: u32::from(meta.inode.links),
            device: meta.inode.device(),
        }
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        self.is_dir()?;
        let mut ns = self.fs.ns.lock();
        let found = self.find_entry(&mut ns, name)?.ok_or(Error::NotFound)?;
        Ok(Inner::node_for(&self.fs, found.ino, &mut ns.b)?)
    }

    fn access(&self) -> Access {
        let meta = self.meta.lock();
        let inode = &meta.inode;
        Access { owner: inode.uid, group: inode.gid, mode: inode.mode & radix_vfs::mode::ALL }
    }

    fn set_access(&self, change: &AccessChange) -> Result<()> {
        self.fs.writable()?;
        if change.mode.is_some() && self.kind == Kind::Symlink {
            return Err(Error::NotSupported);
        }
        let mut txn = self.fs.begin()?;
        let mut meta = self.meta.lock();
        let inode = &mut meta.inode;
        let access = change.apply(Access {
            owner: inode.uid,
            group: inode.gid,
            mode: inode.mode & radix_vfs::mode::ALL,
        });
        inode.uid = access.owner;
        inode.gid = access.group;
        inode.mode = (inode.mode & !radix_vfs::mode::ALL) | access.mode;
        inode.ctime = now();
        self.save(&mut txn, &mut meta)
    }

    fn get_attribute(&self, name: &[u8]) -> Result<KVec<u8>> {
        let (index, rest) = xattr::split(name)?;
        let value = {
            let meta = self.meta.lock();
            xattr::get(&self.fs, &meta.raw, meta.inode.file_acl, index, rest)?
        };
        if xattr::is_list(index) { xattr::acl_from_disk(&value) } else { Ok(value) }
    }

    fn set_attribute(&self, name: &[u8], value: &[u8], how: Setting) -> Result<()> {
        let (index, rest) = xattr::split(name)?;
        if xattr::is_list(index) {
            let disk = xattr::acl_to_disk(value)?;
            self.change_attribute(index, rest, Some((&disk, how)))
        } else {
            self.change_attribute(index, rest, Some((value, how)))
        }
    }

    fn list_attributes(&self) -> Result<KVec<u8>> {
        let meta = self.meta.lock();
        xattr::list(&self.fs, &meta.raw, meta.inode.file_acl)
    }

    fn remove_attribute(&self, name: &[u8]) -> Result<()> {
        let (index, rest) = xattr::split(name)?;
        self.change_attribute(index, rest, None)
    }

    fn access_list(&self) -> Result<Option<KArc<Acl>>> {
        self.acl.get(|| self.get_attribute(acl::ACCESS))
    }

    fn default_list(&self) -> Result<Option<KArc<Acl>>> {
        if self.kind != Kind::Directory {
            return Ok(None);
        }
        self.default_acl.get(|| self.get_attribute(acl::DEFAULT))
    }

    fn create(&self, name: &str, kind: Kind, access: Access) -> Result<NodeRef> {
        self.fs.writable()?;
        check_name(name)?;
        self.is_dir()?;
        if !matches!(kind, Kind::File | Kind::Directory) {
            return Err(Error::NotSupported);
        }
        let _me = self.me()?;
        let mut ns = self.fs.ns.lock();
        if self.removed() {
            return Err(Error::NotFound);
        }
        if self.find_entry(&mut ns, name)?.is_some() {
            return Err(Error::Exists);
        }
        Ok(self.make(&mut ns, name, kind, access, None)?)
    }

    fn make_link(&self, name: &str, target: &str, access: Access) -> Result<NodeRef> {
        self.fs.writable()?;
        check_name(name)?;
        self.is_dir()?;
        if target.is_empty() || target.len() >= self.fs.geo.block_size as usize {
            return Err(Error::NameTooLong);
        }
        let mut ns = self.fs.ns.lock();
        if self.removed() {
            return Err(Error::NotFound);
        }
        if self.find_entry(&mut ns, name)?.is_some() {
            return Err(Error::Exists);
        }
        Ok(self.make(&mut ns, name, Kind::Symlink, access, Some(target.as_bytes()))?)
    }

    fn read_link(&self, out: &mut dyn Out) -> Result<usize> {
        if self.kind != Kind::Symlink {
            return Err(Error::NotLink);
        }
        // Copied out first: `out` may be a program's memory, which may
        // fault.
        let path = {
            let meta = self.meta.lock();
            let size = usize::try_from(meta.inode.size).map_err(|_| Error::Corrupt)?;
            if size > self.fs.geo.block_size as usize {
                return Err(Error::Corrupt);
            }
            if meta.inode.flags & (flags::ENCRYPT | flags::INLINE_DATA) != 0 {
                return Err(Error::NotSupported);
            }
            let mut path = zeroed(size)?;
            if meta.inode.fast_link(&self.fs.geo) {
                path.copy_from_slice(meta.inode.block.get(..size).ok_or(Error::Corrupt)?);
            } else {
                let Found::Mapped { physical, unwritten: false, .. } = meta.extents.find(0) else {
                    return Err(Error::Corrupt);
                };
                drop(meta);
                self.fs.read(self.fs.geo.offset(physical), &mut path)?;
            }
            path
        };
        let n = path.len().min(out.len());
        out.put(0, &path[..n])?;
        Ok(path.len())
    }

    fn remove(&self, name: &str) -> Result<()> {
        self.fs.writable()?;
        check_name(name)?;
        self.is_dir()?;
        let fs = &self.fs;
        let mut ns = fs.ns.lock();
        let place = self.find_entry(&mut ns, name)?.ok_or(Error::NotFound)?;
        let child = Inner::node_for(fs, place.ino, &mut ns.b)?;
        if child.kind == Kind::Directory && !child.is_empty_dir(&mut ns)? {
            return Err(Error::NotEmpty);
        }
        let mut txn = fs.begin()?;
        let done = (|| {
            self.remove_entry(&mut txn, &mut ns, &place)?;
            self.touch_dir(&mut txn, if child.kind == Kind::Directory { -1 } else { 0 })?;
            child.unlinked(&mut txn)
        })();
        if done.is_err() {
            fs.fail();
        }
        drop(txn);
        // Its blocks go when the last reference to it does: at the next
        // commit, unless a handle has it.
        drop(child);
        done
    }

    fn rename(&self, from: &str, to_dir: &dyn Node, to: &str) -> Result<()> {
        self.fs.writable()?;
        check_name(from)?;
        check_name(to)?;
        self.is_dir()?;
        let target = radix_vfs::downcast::<Ext4Node>(to_dir).ok_or(Error::CrossMount)?;
        if !KArc::ptr_eq(&target.fs, &self.fs) {
            return Err(Error::CrossMount);
        }
        target.is_dir()?;
        let fs = &self.fs;
        let mut ns = fs.ns.lock();
        if self.removed() || target.removed() {
            return Err(Error::NotFound);
        }
        let same = core::ptr::eq(self, target);
        let source = self.find_entry(&mut ns, from)?.ok_or(Error::NotFound)?;
        if same && from == to {
            return Ok(());
        }
        let moving = Inner::node_for(fs, source.ino, &mut ns.b)?;
        let moving_dir = moving.kind == Kind::Directory;
        if moving_dir && !same && target.under(&mut ns, moving.ino)? {
            return Err(Error::BadName);
        }
        let replaced = match target.find_entry(&mut ns, to)? {
            // Another name of the same file: nothing to do, as POSIX says.
            Some(old) if old.ino == source.ino => return Ok(()),
            Some(old) => {
                let node = Inner::node_for(fs, old.ino, &mut ns.b)?;
                check_replace(moving.kind, &node, &mut ns)?;
                Some((old, node))
            }
            None => None,
        };
        let mut txn = fs.begin()?;
        fs.room(&mut txn, 2, 0)?;
        let done = (|| {
            let kind = inode::dirent_type(moving.kind);
            match &replaced {
                Some((old, _)) => {
                    target.retarget_entry(&mut txn, &mut ns, old, source.ino, kind)?;
                }
                None => target.add_entry(&mut txn, &mut ns, to, source.ino, kind)?,
            }
            // Found again: adding may have dropped the index.
            let source = self.find_entry(&mut ns, from)?.ok_or(Error::Corrupt)?;
            // An entry leaves an indexed directory's leaf as on Linux,
            // the index kept.
            self.remove_entry(&mut txn, &mut ns, &source)?;
            let old_dir = replaced.as_ref().is_some_and(|(_, n)| n.kind == Kind::Directory);
            if moving_dir && !same {
                moving.set_parent(&mut txn, &mut ns, target.ino)?;
            }
            let mut to_links: i8 = 0;
            if moving_dir && !same {
                to_links += 1;
            }
            if old_dir {
                to_links -= 1;
            }
            if same {
                self.touch_dir(&mut txn, to_links)?;
            } else {
                self.touch_dir(&mut txn, if moving_dir { -1 } else { 0 })?;
                target.touch_dir(&mut txn, to_links)?;
            }
            {
                let mut meta = moving.meta.lock();
                meta.inode.ctime = now();
                moving.save(&mut txn, &mut meta)?;
            }
            if let Some((_, old)) = &replaced {
                old.unlinked(&mut txn)?;
            }
            Ok(())
        })();
        if done.is_err() {
            fs.fail();
        }
        drop(txn);
        drop(replaced);
        done
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        self.is_dir()?;
        let fs = &self.fs;
        let filetype = fs.geo.filetype();
        let mut ns = fs.ns.lock();
        let bs = self.block_size();
        let mut l = cursor / bs;
        let mut skip = (cursor % bs) as usize;
        while l < self.dir_blocks() {
            let Some(block) = self.dir_block(l) else {
                l += 1;
                skip = 0;
                continue;
            };
            self.read_dir_block(l, block, &mut ns.a)?;
            let mut stop = false;
            let mut failed = Ok(());
            dir::for_each(&ns.a, filetype, |e| {
                if e.offset < skip || e.name == b"." || e.name == b".." {
                    return true;
                }
                let Ok(name) = core::str::from_utf8(e.name) else { return true };
                let kind = match inode::kind_of_dirent(e.kind) {
                    Some(kind) => kind,
                    None => {
                        let mut mode = [0u8; 2];
                        match fs.inode_offset(e.ino).and_then(|at| fs.read(at, &mut mode)) {
                            Ok(()) => {
                                inode::kind_of(u16::from_le_bytes(mode)).unwrap_or(Kind::File)
                            }
                            Err(err) => {
                                failed = Err(err);
                                return false;
                            }
                        }
                    }
                };
                let next = l * bs + (e.offset + e.rec_len) as u64;
                stop = !f(next, name, kind);
                !stop
            });
            failed?;
            if stop {
                return Ok(());
            }
            l += 1;
            skip = 0;
        }
        Ok(())
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        self.file()?.read(offset, out, &FileBacking(self))
    }

    fn write(&self, offset: u64, data: &dyn In) -> Result<usize> {
        self.fs.writable()?;
        let result = self.file()?.write(offset, data, &FileBacking(self));
        self.changed();
        result
    }

    fn set_size(&self, size: u64) -> Result<()> {
        self.fs.writable()?;
        let result = self.file()?.set_size(size, &FileBacking(self));
        self.changed();
        result
    }

    fn set_times(&self, accessed: Option<u64>, modified: Option<u64>) -> Result<()> {
        self.fs.writable()?;
        // A file's change time is its cache's; the cache's lock comes
        // before the node's (write-back takes it), so it is set first.
        if let (Some(modified), Some(cache)) = (modified, &self.cache) {
            cache.set_modified(inode::kept_time(&self.fs.geo, modified));
        }
        let mut txn = self.fs.begin()?;
        let mut meta = self.meta.lock();
        if let Some(accessed) = accessed {
            meta.inode.atime = inode::kept_time(&self.fs.geo, accessed);
        }
        if let Some(modified) = modified {
            meta.inode.mtime = inode::kept_time(&self.fs.geo, modified);
        }
        meta.inode.ctime = now();
        self.save(&mut txn, &mut meta)
    }

    fn sync(&self) -> Result<()> {
        // The whole volume: the bitmaps a file's blocks are in are shared,
        // and the volume is then consistent on the disk.
        self.fs.sync_all()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
