// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A mounted Btrfs filesystem: its volume, its checksum tree and its
//! nodes.
//!
//! It mounts the default subvolume, as Linux does when no `subvol=` is
//! given: the one the root tree's `default` entry names, the top one
//! (5) unless `btrfs subvolume set-default` changed it. The subvolumes
//! inside it are directories of the mount, as on Linux.
//!
//! A writable mount (ADR 0130) keeps the running transaction, and changes
//! files through it ([`crate::files`]); a commit makes them durable at the
//! next sync, every [`COMMIT_NS`], or sooner when the pages waiting to be
//! written pass [`DIRTY_PAGES_MAX`]. A node whose data or inode waits for
//! the sync is kept until then, unless its file loses its last name with
//! no one holding it open: then its items go at once and its pages are
//! never written, as Linux evicts the inode. A file removed while open
//! keeps its items until its last node goes, with an orphan item that
//! cleans it up at the next writable mount if the power goes first.
//!
//! # Locks
//!
//! A file's page cache is taken first, then, by a write that compresses,
//! the packers (ADR 0134). On a writable mount the transaction's lock
//! comes next, held by every change and also by every
//! read of the trees, since a change edits the transaction's blocks in
//! place; it is never held while a program's memory is copied, which may
//! fault and read a file. Then come a node's inode, the node table, the
//! nodes kept for the sync and the list of nodes to remove, then the
//! workspaces of compressed reads, the volume's block cache and the
//! disk's. `nodes` is held only to find or add a node, never while
//! reading the disk: a node is read first, and when another was added
//! for the same inode meanwhile, that one is used and the new one
//! dropped, which only frees memory. Shrinking only tries the
//! workspaces' and the packers' locks, as a reader or a writer that holds
//! one may be making memory.

use alloc::sync::Weak;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_block::{DeviceRef, PAGE_SIZE};
use radix_sync::{Mutex, MutexGuard};
use radix_vfs::mem::{self, Alloc, KArc, KBox, KVec};
use radix_vfs::{Error, FileSystem, FsInfo, FsKind, Kind, Node, NodeRef, Result, SocketBinding};

use crate::cow::Writer;
use crate::dir::Entries;
use crate::files::Files;
use crate::format::{Key, item, name_hash, tree};
use crate::node::BtrfsNode;
use crate::pack::Packer;
use crate::superblock::incompat;
use crate::tree::{Cursor, Root};
use crate::unpack::Workspace;
use crate::volume::Volume;

/// How long a change waits in memory, at most, before a commit makes it
/// durable: Linux's default `commit=30`.
pub const COMMIT_NS: u64 = 30_000_000_000;

/// How many pages may wait to be written before the writeback thread
/// commits early: 64 MiB.
pub const DIRTY_PAGES_MAX: usize = 16_384;

/// The running transaction of a writable mount, and what changing files
/// keeps beside it.
pub(crate) struct Txn {
    pub(crate) writer: Writer,
    /// The next inode number of each subvolume written so far.
    next_ino: KVec<(u64, u64)>,
}

impl Txn {
    /// Subvolume `subvol`, to change.
    pub(crate) fn files<'a>(&'a mut self, volume: &'a Volume, subvol: u64) -> Files<'a> {
        Files { w: &mut self.writer, volume, subvol, changed: [None; 2] }
    }

    /// A number for a new inode of subvolume `subvol`.
    pub(crate) fn take_ino(&mut self, volume: &Volume, subvol: u64) -> Result<u64> {
        let at = match self.next_ino.iter().position(|e| e.0 == subvol) {
            Some(at) => at,
            None => {
                let next = self.files(volume, subvol).next_ino()?;
                mem::push(&mut self.next_ino, (subvol, next))?;
                self.next_ino.len() - 1
            }
        };
        let ino = self.next_ino[at].1;
        if ino >= crate::files::object::LAST_FREE {
            return Err(Error::NoSpace);
        }
        self.next_ino[at].1 += 1;
        Ok(ino)
    }

    /// Forgets subvolume `subvol`, removed.
    pub(crate) fn forget(&mut self, subvol: u64) {
        self.next_ino.retain(|e| e.0 != subvol);
        self.writer.forget_tree(subvol);
    }
}

/// An entry of the node table: a node's subvolume and inode number, and
/// the node while it is in use.
type Entry = ((u64, u64), Weak<BtrfsNode, Alloc>);

/// What a filesystem's nodes share.
pub(crate) struct Inner {
    pub(crate) volume: KBox<Volume>,
    /// The checksum tree's root, if it has one, on a read-only mount.
    csums: Option<Root>,
    /// The nodes in use, by subvolume and inode number, sorted.
    nodes: Mutex<KVec<Entry>>,
    /// What reading compressed extents uses, once one is read: a
    /// workspace for the reader, and one for the half of a run another
    /// CPU uncompresses (ADR 0133).
    workspaces: Mutex<[Option<KBox<Workspace>>; 2]>,
    /// Whether data written is compressed with zstd, as Linux's
    /// `compress=zstd:1` mount option has it (ADR 0134).
    compress: AtomicBool,
    /// What compressing extents uses, once one is written: a packer for
    /// the writer, and one for the extent another CPU compresses.
    packers: Mutex<[Option<KBox<Packer>>; 2]>,
    /// The running transaction, on a writable mount.
    tx: Option<Mutex<Txn>>,
    /// The nodes whose data or inode waits for the next sync.
    dirty: Mutex<KVec<KArc<BtrfsNode>>>,
    /// Inodes removed while open and since closed, by subvolume and
    /// number: their items go at the next sync.
    reap: Mutex<KVec<(u64, u64)>>,
    /// What the sockets named on it hold, by subvolume and inode number:
    /// the kernel's, kept while it is mounted, as on Linux (ADR 0135).
    sockets: Mutex<KVec<((u64, u64), SocketBinding)>>,
    /// When the oldest change the disk lacks was made, or 0 if none.
    since: AtomicU64,
    /// The bytes of the dirty pages of files, which write-back takes
    /// data space for.
    data_reserved: AtomicU64,
    /// The bytes data could take, as last counted under the transaction.
    data_room: AtomicU64,
}

/// A mounted Btrfs filesystem.
pub struct BtrfsFs {
    inner: KArc<Inner>,
    root: KArc<BtrfsNode>,
}

impl BtrfsFs {
    /// Mounts the Btrfs filesystem on `device`, read-only, at its default
    /// subvolume.
    ///
    /// Fails as [`Volume::open`] does, and with [`Error::NotSupported`]
    /// for a sector size other than the page size (4 KiB, what
    /// `mkfs.btrfs` makes on x86-64), or a disk whose blocks are larger
    /// than the sectors.
    pub fn mount(device: DeviceRef) -> Result<KArc<BtrfsFs>> {
        BtrfsFs::mount_with(device, false)
    }

    /// Mounts the Btrfs filesystem on `device` to read and write, at its
    /// default subvolume, and cleans up the files a crash left removed
    /// but not yet freed.
    ///
    /// Fails as [`BtrfsFs::mount`] and [`Writer::new`] do, and with
    /// [`Error::NotSupported`] for a filesystem without `no-holes`, which
    /// every `mkfs.btrfs` since 5.15 makes: one that names its holes
    /// is not written yet.
    pub fn mount_writable(device: DeviceRef) -> Result<KArc<BtrfsFs>> {
        BtrfsFs::mount_with(device, true)
    }

    /// Mounts the Btrfs filesystem on `devices[0]` and those of the others
    /// that hold the rest of it ([`Volume::open_disks`]), to read and
    /// write if `writable`, as [`BtrfsFs::mount_writable`] does.
    ///
    /// Fails as [`BtrfsFs::mount`] does, and with [`Error::NoDevice`] when
    /// a disk of it is missing and the others lack a copy of a chunk, or
    /// `degraded` is not set: Linux's `degraded` mount option, which
    /// allows a mount with a disk missing. The kernel always sets it, and
    /// warns (ADR 0153).
    pub fn mount_disks(
        devices: &[DeviceRef],
        writable: bool,
        degraded: bool,
    ) -> Result<KArc<BtrfsFs>> {
        let logical_block =
            devices.iter().map(|d| d.info().logical_block).max().ok_or(Error::NotFound)?;
        let inner = Inner::open(devices, writable, degraded)?;
        BtrfsFs::with_root(inner, logical_block, writable)
    }

    /// The ID of the Btrfs filesystem on `device`, which each of its disks
    /// has in its superblock (its `fsid`).
    ///
    /// Fails with [`Error::UnknownFilesystem`] when `device` holds no
    /// Btrfs, and as reading it fails.
    pub fn id_of(device: &DeviceRef) -> Result<[u8; 16]> {
        Ok(crate::volume::read_superblock(device)?.0.fsid)
    }

    /// How many disks the filesystem has, and how many of them are not
    /// there: missing at the mount, or failed since (ADR 0153). Disks
    /// added and removed since the mount count as they are now.
    pub fn disks(&self) -> (usize, usize) {
        let volume = &self.inner.volume;
        (volume.disks().iter().filter(|d| !d.gone()).count(), volume.missing())
    }

    /// How many of its disks there missed changes, and are read last
    /// until a scrub has caught them up (ADR 0153).
    pub fn behind(&self) -> usize {
        self.inner.volume.disks().iter().filter(|d| d.present() && d.behind()).count()
    }

    /// Whether `device` is one of its disks, there now.
    pub fn has_disk(&self, device: &DeviceRef) -> bool {
        let disks = self.inner.volume.disks().iter();
        disks.filter_map(|d| d.device()).any(|d| DeviceRef::ptr_eq(d, device))
    }

    /// Whether it was mounted to write.
    pub fn writable(&self) -> bool {
        self.inner.tx.is_some()
    }

    /// Has the data written from now on compressed with zstd, or not:
    /// Linux's `compress=zstd:1` mount option (ADR 0134). Files without
    /// checksums, or marked not to be compressed, are written as they
    /// are; a file marked to be compressed is, either way.
    pub fn set_compress(&self, on: bool) {
        self.inner.compress.store(on, Ordering::Relaxed);
    }

    fn mount_with(device: DeviceRef, writable: bool) -> Result<KArc<BtrfsFs>> {
        BtrfsFs::mount_disks(&[device], writable, false)
    }

    /// The filesystem of `inner`, at its default subvolume. A frame of its
    /// own, apart from [`Inner::open`]'s deep reads: a mount runs in a
    /// system call, on a 16 KiB stack.
    #[inline(never)]
    fn with_root(inner: KArc<Inner>, logical_block: u32, writable: bool) -> Result<KArc<BtrfsFs>> {
        let volume = &inner.volume;
        let sectorsize = volume.superblock().sectorsize;
        if sectorsize as usize != PAGE_SIZE || logical_block > sectorsize {
            return Err(Error::NotSupported);
        }
        let subvol = default_subvolume(volume)?;
        let (top, dir, read_only) = volume.subvolume(subvol)?.ok_or(Error::Corrupt)?;
        let root = Inner::node_for(&inner, subvol, top, dir, read_only)?;
        if root.kind() != Kind::Directory {
            return Err(Error::Corrupt);
        }
        if writable {
            inner.clean_orphans()?;
        }
        Ok(mem::arc(BtrfsFs { inner, root })?)
    }

    /// The volume.
    pub fn volume(&self) -> &Volume {
        &self.inner.volume
    }
}

/// The subvolume mounted when none is asked for: the one the root tree's
/// `default` entry names, the top one when there is no such entry.
pub(crate) fn default_subvolume(volume: &Volume) -> Result<u64> {
    const NAME: &[u8] = b"default";
    let key = Key::new(tree::ROOT_TREE_DIR, item::DIR_ITEM, name_hash(NAME));
    let cursor = Cursor::seek(volume, &volume.root_tree(), &key)?;
    let Some((found, data)) = cursor.get() else { return Ok(tree::FS) };
    if found != key {
        return Ok(tree::FS);
    }
    for entry in Entries::of(data) {
        let entry = entry?;
        if entry.name == NAME {
            return Ok(entry.location.objectid);
        }
    }
    Ok(tree::FS)
}

impl Inner {
    /// Opens the volume on `device`, in a frame of its own. The writer
    /// reads every block group, the deepest calls of a mount: they come
    /// before the frame that builds the rest.
    /// A log of fsyncs Linux left is replayed and committed first, on a
    /// read-only mount too, as Linux's mount does (ADR 0151).
    #[inline(never)]
    fn open(devices: &[DeviceRef], writable: bool, degraded: bool) -> Result<KArc<Inner>> {
        let volume = Volume::open_disks(devices)?;
        if volume.missing() > 0 && !(degraded && volume.readable()) {
            return Err(Error::NoDevice);
        }
        Inner::start(volume, writable)
    }

    /// What [`Inner::open`] makes of `volume`, once open: in a frame of
    /// its own, with nothing but the transaction's box on it, so that the
    /// volume's reads beneath it stay shallow. The checksum tree is found
    /// after the replay, which changes it.
    #[inline(never)]
    fn start(volume: KBox<Volume>, writable: bool) -> Result<KArc<Inner>> {
        let log = volume.superblock().log_root != 0;
        let mut txn = if writable || log { Some(Inner::txn(&volume)?) } else { None };
        if log && let Some(txn) = txn.as_mut() {
            Inner::replay(&volume, txn)?;
        }
        let csums = Inner::checksums(&volume)?;
        Inner::of(volume, csums, if writable { txn } else { None })
    }

    /// A transaction of `volume`, in a frame of its own, on the heap.
    #[inline(never)]
    fn txn(volume: &Volume) -> Result<KBox<Txn>> {
        if !volume.superblock().has(incompat::NO_HOLES) {
            return Err(Error::NotSupported);
        }
        Ok(mem::boxed(Txn { writer: Writer::new(volume)?, next_ino: mem::vec() })?)
    }

    /// Replays the log of fsyncs `volume` holds in `txn`, and commits it,
    /// in a frame of its own.
    #[inline(never)]
    fn replay(volume: &Volume, txn: &mut Txn) -> Result<()> {
        crate::replay::replay(volume, txn)?;
        txn.writer.commit(volume)?;
        // The mount's changes look their inode numbers up afresh.
        txn.next_ino = mem::vec();
        Ok(())
    }

    /// The root of `volume`'s checksum tree, in a frame of its own.
    #[inline(never)]
    fn checksums(volume: &Volume) -> Result<Option<Root>> {
        volume.tree(tree::CSUM)
    }

    /// What the nodes of `volume` share, on the heap, in a frame of its
    /// own, with its checksum tree `csums`; `txn` writes it on a writable
    /// mount.
    #[inline(never)]
    fn of(
        volume: KBox<Volume>,
        csums: Option<Root>,
        txn: Option<KBox<Txn>>,
    ) -> Result<KArc<Inner>> {
        let tx = txn.map(|txn| Mutex::new(*txn));
        Ok(mem::arc(Inner {
            volume,
            csums,
            nodes: Mutex::new(mem::vec()),
            workspaces: Mutex::new([None, None]),
            compress: AtomicBool::new(false),
            packers: Mutex::new([None, None]),
            tx,
            dirty: Mutex::new(mem::vec()),
            reap: Mutex::new(mem::vec()),
            sockets: Mutex::new(mem::vec()),
            since: AtomicU64::new(0),
            data_reserved: AtomicU64::new(0),
            data_room: AtomicU64::new(0),
        })?)
    }

    /// Whether the mount writes.
    pub(crate) fn writable(&self) -> bool {
        self.tx.is_some()
    }

    /// Holds the transaction, on a writable mount, while the trees are
    /// read: a change edits its blocks in place.
    pub(crate) fn reading(&self) -> Option<MutexGuard<'_, Txn>> {
        self.tx.as_ref().map(Mutex::lock)
    }

    /// Runs the change `f` in the running transaction, once metadata has
    /// room for it (ADR 0131); fails with [`Error::ReadOnly`] on a
    /// read-only mount, and with [`Error::NoSpace`] before it starts when
    /// there is no room.
    pub(crate) fn change<R>(&self, f: impl FnOnce(&mut Txn) -> Result<R>) -> Result<R> {
        self.change_with(false, f)
    }

    /// Runs the change `f`, which frees more than it takes (removing,
    /// cutting): it may use the room kept for commits, so a full
    /// filesystem can be made less full.
    pub(crate) fn freeing<R>(&self, f: impl FnOnce(&mut Txn) -> Result<R>) -> Result<R> {
        self.change_with(true, f)
    }

    fn change_with<R>(&self, freeing: bool, f: impl FnOnce(&mut Txn) -> Result<R>) -> Result<R> {
        let tx = self.tx.as_ref().ok_or(Error::ReadOnly)?;
        let mut txn = tx.lock();
        txn.writer.make_room(&self.volume, freeing)?;
        self.touch();
        let result = f(&mut txn);
        self.count_data_room(&mut txn);
        result
    }

    /// Runs the change `f` in a transaction of its own, committed before
    /// and after it, as making a snapshot or removing a subvolume is: the
    /// trees it shares or drops are as the disk has them, and it is on
    /// the disk when it returns.
    pub(crate) fn alone<R>(
        &self,
        freeing: bool,
        f: impl FnOnce(&mut Txn) -> Result<R>,
    ) -> Result<R> {
        let tx = self.tx.as_ref().ok_or(Error::ReadOnly)?;
        let mut txn = tx.lock();
        self.commit_locked(&mut txn)?;
        txn.writer.make_room(&self.volume, freeing)?;
        self.touch();
        let result = f(&mut txn);
        self.count_data_room(&mut txn);
        let result = result?;
        self.commit_locked(&mut txn)?;
        Ok(result)
    }

    /// Whether a node of subvolume `subvol` is in use.
    pub(crate) fn in_use(&self, subvol: u64) -> bool {
        self.nodes.lock().iter().any(|((s, _), weak)| *s == subvol && weak.strong_count() > 0)
    }

    /// Counts again the bytes data could take.
    fn count_data_room(&self, txn: &mut Txn) {
        if let Ok(room) = txn.writer.room(&self.volume, crate::chunk::kind::DATA) {
            self.data_room.store(room, Ordering::Release);
        }
    }

    /// Takes room for `bytes` of data that dirty pages will need when they
    /// are written back; fails with [`Error::NoSpace`] when data has no
    /// room for them, as Linux's `write` does. Read-only mounts write
    /// nothing back.
    pub(crate) fn reserve_data(&self, bytes: u64) -> Result<()> {
        let Some(tx) = &self.tx else { return Ok(()) };
        let total = self.data_reserved.fetch_add(bytes, Ordering::AcqRel) + bytes;
        if total <= self.data_room.load(Ordering::Acquire) {
            return Ok(());
        }
        // The room counted may be old: count it again.
        self.count_data_room(&mut tx.lock());
        if total <= self.data_room.load(Ordering::Acquire) {
            return Ok(());
        }
        self.data_reserved.fetch_sub(bytes, Ordering::AcqRel);
        Err(Error::NoSpace)
    }

    /// Gives back the room [`Inner::reserve_data`] took for `bytes`.
    pub(crate) fn release_data(&self, bytes: u64) {
        let _ = self
            .data_reserved
            .try_update(Ordering::AcqRel, Ordering::Acquire, |r| Some(r.saturating_sub(bytes)));
    }

    /// Notes that the disk lacks a change made now.
    fn touch(&self) {
        let now = radix_vfs::now().max(1);
        let _ = self.since.compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire);
    }

    /// The checksum tree's root, as the running transaction has it.
    pub(crate) fn csums(&self) -> Result<Option<Root>> {
        if self.writable() { self.volume.tree(tree::CSUM) } else { Ok(self.csums) }
    }

    /// Keeps `node` until the next sync: its data or inode waits for it.
    pub(crate) fn pin(&self, node: KArc<BtrfsNode>) -> core::result::Result<(), KArc<BtrfsNode>> {
        self.touch();
        let mut dirty = self.dirty.lock();
        if dirty.try_reserve(1).is_err() {
            return Err(node);
        }
        let _ = dirty.push_within_capacity(node);
        Ok(())
    }

    /// Takes `node`, of a file whose last name is going, out of the nodes
    /// kept for the next sync when nothing else holds it: no one has it
    /// open (the caller's is the other reference) and the sync has not
    /// taken it, so its pages are never written. The caller holds the
    /// transaction, so no lookup finds the node meanwhile; the node table's
    /// lock keeps a shrink from taking it.
    pub(crate) fn take_idle(&self, node: &KArc<BtrfsNode>) -> bool {
        let _nodes = self.nodes.lock();
        let mut dirty = self.dirty.lock();
        if KArc::strong_count(node) != 2 {
            return false;
        }
        let Some(i) = dirty.iter().rposition(|kept| KArc::ptr_eq(kept, node)) else {
            return false;
        };
        // The caller's reference stays, so this drops nothing.
        drop(dirty.remove(i));
        node.unpinned();
        true
    }

    /// Notes that the last node of inode `ino` of subvolume `subvol`, which
    /// has no names left, went: its items go at the next sync. Without
    /// the memory to note it, its orphan item has them go at the next
    /// writable mount.
    pub(crate) fn reaped(&self, subvol: u64, ino: u64) {
        let mut reap = self.reap.lock();
        if reap.try_reserve(1).is_ok() {
            let _ = reap.push_within_capacity((subvol, ino));
        }
    }

    /// The node of inode `ino` of subvolume `subvol`, if one is in use.
    /// What the socket named by inode `ino` of subvolume `subvol` holds,
    /// if one was named there since the mount.
    pub(crate) fn socket(&self, subvol: u64, ino: u64) -> Option<SocketBinding> {
        let sockets = self.sockets.lock();
        let i = sockets.binary_search_by_key(&(subvol, ino), |(k, _)| *k).ok()?;
        Some(sockets[i].1.clone())
    }

    /// Notes that inode `ino` of subvolume `subvol` names a socket that
    /// holds `binding`, or that it no longer does.
    pub(crate) fn set_socket(
        &self,
        subvol: u64,
        ino: u64,
        binding: Option<SocketBinding>,
    ) -> Result<()> {
        let mut sockets = self.sockets.lock();
        match (sockets.binary_search_by_key(&(subvol, ino), |(k, _)| *k), binding) {
            (Ok(i), Some(binding)) => sockets[i].1 = binding,
            (Ok(i), None) => drop(sockets.remove(i)),
            (Err(i), Some(binding)) => mem::insert(&mut sockets, i, ((subvol, ino), binding))?,
            (Err(_), None) => {}
        }
        Ok(())
    }

    pub(crate) fn live(&self, subvol: u64, ino: u64) -> Option<KArc<BtrfsNode>> {
        let nodes = self.nodes.lock();
        let i = nodes.binary_search_by_key(&(subvol, ino), |(k, _)| *k).ok()?;
        nodes[i].1.upgrade()
    }

    /// Frees the files a crash left removed but open: those with orphan
    /// items, in every subvolume.
    fn clean_orphans(&self) -> Result<()> {
        self.freeing(|txn| {
            let volume = &self.volume;
            let mut key = Key::first(tree::FS, item::ROOT_ITEM);
            loop {
                let subvol = {
                    let cursor = Cursor::seek(volume, &volume.root_tree(), &key)?;
                    match cursor.key() {
                        Some(found) if found.objectid < crate::files::object::LAST_FREE => found,
                        _ => break,
                    }
                };
                key = Key::first(subvol.objectid + 1, item::ROOT_ITEM);
                if subvol.kind != item::ROOT_ITEM
                    || (subvol.objectid != tree::FS && subvol.objectid < tree::FIRST_FREE)
                {
                    continue;
                }
                let mut files = txn.files(volume, subvol.objectid);
                loop {
                    let orphans = files.orphans(64)?;
                    if orphans.is_empty() {
                        break;
                    }
                    for ino in orphans {
                        files.whole(|f| f.free_inode(ino, true))?;
                    }
                }
            }
            Ok(())
        })?;
        // Nothing may have changed.
        if let Some(tx) = &self.tx
            && !tx.lock().writer.is_dirty()
        {
            self.since.store(0, Ordering::Release);
        }
        Ok(())
    }

    /// Writes every node's data and inode, frees the files removed and
    /// closed, and commits.
    pub(crate) fn sync(&self) -> Result<()> {
        let Some(tx) = &self.tx else { return Ok(()) };
        let pinned = core::mem::replace(&mut *self.dirty.lock(), mem::vec());
        let mut failed = None;
        for node in &pinned {
            node.unpinned();
            if let Err(err) = node.flush() {
                failed.get_or_insert(err);
                // It stays dirty, to be tried again.
                node.pin();
            }
        }
        // Nodes go here, with no lock held: a removed one is reaped.
        drop(pinned);
        let reap = core::mem::replace(&mut *self.reap.lock(), mem::vec());
        {
            let mut txn = tx.lock();
            for &(subvol, ino) in &reap {
                txn.files(&self.volume, subvol).whole(|f| f.free_inode(ino, true))?;
            }
            self.commit_locked(&mut txn)?;
        }
        match failed {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Makes file `ino` of subvolume `subvol` durable, with the log of
    /// fsyncs when it can, with a commit otherwise (ADR 0154).
    pub(crate) fn sync_file(&self, subvol: u64, ino: u64) -> Result<()> {
        let Some(tx) = &self.tx else { return Ok(()) };
        let mut txn = tx.lock();
        match txn.writer.log_file(&self.volume, subvol, ino) {
            Ok(true) => Ok(()),
            Ok(false) | Err(Error::NoSpace | Error::NoMemory) => self.commit_locked(&mut txn),
            Err(e) => Err(e),
        }
    }

    /// Commits the running transaction.
    pub(crate) fn commit(&self) -> Result<()> {
        let Some(tx) = &self.tx else { return Ok(()) };
        self.commit_locked(&mut tx.lock())
    }

    pub(crate) fn commit_locked(&self, txn: &mut Txn) -> Result<()> {
        txn.writer.commit(&self.volume)?;
        // What waits for the next sync keeps the commit due.
        let waiting = !self.dirty.lock().is_empty() || !self.reap.lock().is_empty();
        self.since.store(if waiting { radix_vfs::now().max(1) } else { 0 }, Ordering::Release);
        Ok(())
    }

    /// Runs `f` with the workspace of compressed reads, made if need be.
    pub(crate) fn with_workspace<R>(
        &self,
        f: impl FnOnce(&mut Workspace) -> Result<R>,
    ) -> Result<R> {
        self.with_workspaces(false, |workspace, _| f(workspace))
    }

    /// Runs `f` with the workspace of compressed reads, made if need be,
    /// and, when `two` and `radix_vfs::join` may use another CPU, the
    /// second, for the half of a run that CPU uncompresses: made if there
    /// is memory for it, or else `None`.
    pub(crate) fn with_workspaces<R>(
        &self,
        two: bool,
        f: impl FnOnce(&mut Workspace, Option<&mut Workspace>) -> Result<R>,
    ) -> Result<R> {
        let mut slots = self.workspaces.lock();
        let [first, second] = &mut *slots;
        let first = match first {
            Some(workspace) => workspace,
            empty => empty.insert(Workspace::boxed()?),
        };
        let two = two && radix_vfs::joins();
        if two && second.is_none() {
            *second = Workspace::boxed().ok();
        }
        f(first, if two { second.as_deref_mut() } else { None })
    }

    /// Whether data written is compressed.
    pub(crate) fn compress(&self) -> bool {
        self.compress.load(Ordering::Relaxed)
    }

    /// Runs `f` with the packer of compressed writes, made if need be,
    /// and, when `two` and `radix_vfs::join` may use another CPU, the
    /// second, for the extent that CPU compresses: made if there is
    /// memory for it, or else `None`.
    pub(crate) fn with_packers<R>(
        &self,
        two: bool,
        f: impl FnOnce(&mut Packer, Option<&mut Packer>) -> Result<R>,
    ) -> Result<R> {
        let mut slots = self.packers.lock();
        let [first, second] = &mut *slots;
        let first = match first {
            Some(packer) => packer,
            empty => empty.insert(Packer::boxed()?),
        };
        let two = two && radix_vfs::joins();
        if two && second.is_none() {
            *second = Packer::boxed().ok();
        }
        f(first, if two { second.as_deref_mut() } else { None })
    }

    /// Drops the packers of compressed writes, unless a write is under
    /// way; returns the pages they held.
    fn drop_packers(&self) -> usize {
        let Some(mut slots) = self.packers.try_lock() else { return 0 };
        slots.iter_mut().filter_map(Option::take).map(|packer| packer.held()).sum()
    }

    /// Drops the workspaces of compressed reads, unless a read is under
    /// way; returns the pages they held.
    fn drop_workspace(&self) -> usize {
        let Some(mut slots) = self.workspaces.try_lock() else { return 0 };
        slots.iter_mut().filter_map(Option::take).map(|workspace| workspace.pages()).sum()
    }

    /// The node of inode `ino` of subvolume `subvol`, whose tree is
    /// `tree`: the one in use, or a new one.
    pub(crate) fn node_for(
        this: &KArc<Inner>,
        subvol: u64,
        tree: Root,
        ino: u64,
        read_only: bool,
    ) -> Result<KArc<BtrfsNode>> {
        let key = (subvol, ino);
        {
            let nodes = this.nodes.lock();
            if let Ok(i) = nodes.binary_search_by_key(&key, |(k, _)| *k)
                && let Some(node) = nodes[i].1.upgrade()
            {
                return Ok(node);
            }
        }
        let node = mem::arc(BtrfsNode::load(this.clone(), subvol, tree, ino, read_only)?)?;
        BtrfsNode::placed(&node);
        let mut nodes = this.nodes.lock();
        let weak = KArc::downgrade(&node);
        match nodes.binary_search_by_key(&key, |(k, _)| *k) {
            Ok(i) => match nodes[i].1.upgrade() {
                // Another lookup read it first: the new node goes, after
                // the lock.
                Some(other) => {
                    drop(nodes);
                    return Ok(other);
                }
                None => nodes[i].1 = weak,
            },
            Err(_) => {
                if nodes.len() == nodes.capacity() {
                    // Room is made by forgetting nodes no longer in use
                    // first; the table only grows past twice the nodes
                    // in use.
                    nodes.retain(|(_, weak)| weak.strong_count() > 0);
                }
                let i = nodes.binary_search_by_key(&key, |(k, _)| *k).unwrap_or_else(|i| i);
                if mem::insert(&mut nodes, i, (key, weak)).is_err() {
                    return Err(Error::NoMemory);
                }
            }
        }
        Ok(node)
    }

    /// The nodes in use, for shrinking their caches.
    fn live_nodes(&self) -> Result<KVec<KArc<BtrfsNode>>> {
        let nodes = self.nodes.lock();
        let mut live = mem::vec_with_capacity(nodes.len())?;
        for (_, weak) in nodes.iter() {
            if let Some(node) = weak.upgrade() {
                let _ = live.push_within_capacity(node);
            }
        }
        Ok(live)
    }
}

impl FileSystem for BtrfsFs {
    fn kind(&self) -> FsKind {
        FsKind::Btrfs
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    /// Its size and what is used of it, as the superblock keeps them and
    /// the running transaction changes them; Linux also takes the
    /// profiles and unallocated space into account, which Phase 10e's
    /// `btrfs filesystem usage` shows.
    fn info(&self) -> FsInfo {
        let volume = &self.inner.volume;
        let sb = volume.superblock();
        let sector = u64::from(sb.sectorsize);
        // What a file may still take, as Linux's `df` counts what is
        // available: data's room, less what dirty pages will take.
        let free = match &self.inner.tx {
            Some(tx) => {
                let room = tx.lock().writer.room(volume, crate::chunk::kind::DATA).unwrap_or(0);
                room.saturating_sub(self.inner.data_reserved.load(Ordering::Acquire))
            }
            None => sb.total_bytes.saturating_sub(sb.bytes_used),
        };
        FsInfo {
            block_size: sb.sectorsize,
            blocks: volume.total_bytes() / sector,
            free: free / sector,
            ..FsInfo::default()
        }
    }

    fn read_only(&self) -> bool {
        !self.inner.writable()
    }

    fn sync(&self) -> Result<()> {
        self.inner.sync()
    }

    fn scrub(&self, repair: bool) -> Result<radix_vfs::Scrub> {
        crate::scrub::scrub(&self.inner, repair)
    }

    fn replace_disk(&self, devid: u64, device: DeviceRef) -> Result<radix_vfs::Scrub> {
        crate::replace::replace(&self.inner, devid, &device)
    }

    fn trim(&self, minimum: u64) -> Result<u64> {
        crate::trim::trim(&self.inner, minimum)
    }

    fn add_disk(&self, device: DeviceRef) -> Result<u64> {
        crate::balance::add_disk(&self.inner, &device)
    }

    fn remove_disk(&self, devid: u64) -> Result<()> {
        crate::balance::remove_disk(&self.inner, devid)
    }

    fn balance(&self, plan: &radix_vfs::Balance) -> Result<radix_vfs::Balanced> {
        crate::balance::balance(&self.inner, plan)
    }

    /// Whether a commit is due: the oldest change the disk lacks is
    /// [`COMMIT_NS`] old, or more than [`DIRTY_PAGES_MAX`] pages wait.
    fn is_dirty(&self) -> bool {
        let since = self.inner.since.load(Ordering::Acquire);
        if since == 0 {
            return false;
        }
        if radix_vfs::now().saturating_sub(since) >= COMMIT_NS {
            return true;
        }
        // Each dirty page holds the room it reserved, so the room counts
        // them without a lock: summing the files' counts under the dirty
        // list's lock took each file's state after the list, which a sync
        // takes in the other order (the file's state, the transaction,
        // then the list).
        self.inner.data_reserved.load(Ordering::Acquire) > (DIRTY_PAGES_MAX * PAGE_SIZE) as u64
    }

    fn label(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        match self.inner.volume.superblock().label() {
            Some(label) => out.write_str(label),
            None => Ok(()),
        }
    }

    fn shrink(&self, max: usize) -> usize {
        let mut dropped = self.inner.volume.shrink(max);
        if dropped < max {
            dropped += self.inner.drop_workspace();
        }
        if dropped < max {
            dropped += self.inner.drop_packers();
        }
        if dropped < max {
            let Ok(live) = self.inner.live_nodes() else { return dropped };
            for node in &live {
                if dropped >= max {
                    break;
                }
                dropped += node.shrink(max - dropped);
            }
        }
        dropped
    }

    /// A node holds only memory: nothing is written when it goes, and a
    /// removed one's items go at the next sync.
    fn light_drops(&self) -> bool {
        true
    }
}
