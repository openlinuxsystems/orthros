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
//! Btrfs is read-only until Phase 10c, which writes it.
//!
//! # Locks
//!
//! `nodes` (the node table) is held only to find or add a node, never
//! while reading the disk: a node is read first, and when another was
//! added for the same inode meanwhile, that one is used and the new one
//! dropped, which only frees memory. A file's page cache is taken before
//! the volume's block cache, which is taken before the disk's.

use alloc::sync::Weak;
use core::fmt;

use radix_block::{DeviceRef, PAGE_SIZE};
use radix_sync::Mutex;
use radix_vfs::mem::{self, Alloc, KArc, KVec};
use radix_vfs::{Error, FileSystem, FsInfo, FsKind, Kind, Node, NodeRef, Result};

use crate::dir::Entries;
use crate::format::{Key, item, name_hash, tree};
use crate::node::BtrfsNode;
use crate::tree::{Cursor, Root};
use crate::volume::Volume;

/// An entry of the node table: a node's subvolume and inode number, and
/// the node while it is in use.
type Entry = ((u64, u64), Weak<BtrfsNode, Alloc>);

/// What a filesystem's nodes share.
pub(crate) struct Inner {
    pub(crate) volume: Volume,
    /// The checksum tree's root, if it has one.
    pub(crate) csums: Option<Root>,
    /// The nodes in use, by subvolume and inode number, sorted.
    nodes: Mutex<KVec<Entry>>,
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
        let logical_block = device.info().logical_block;
        let inner = Inner::open(device)?;
        let volume = &inner.volume;
        let sectorsize = volume.superblock().sectorsize;
        if sectorsize as usize != PAGE_SIZE || logical_block > sectorsize {
            return Err(Error::NotSupported);
        }
        let subvol = default_subvolume(volume)?;
        let (top, dir) = volume.subvolume(subvol)?.ok_or(Error::Corrupt)?;
        let root = Inner::node_for(&inner, subvol, top, dir)?;
        if root.kind() != Kind::Directory {
            return Err(Error::Corrupt);
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
fn default_subvolume(volume: &Volume) -> Result<u64> {
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
    /// Opens the volume on `device`, on the heap: a volume is too large
    /// for [`BtrfsFs::mount`]'s stack frame to hold as well.
    #[inline(never)]
    fn open(device: DeviceRef) -> Result<KArc<Inner>> {
        let volume = Volume::open(device)?;
        let csums = volume.tree(tree::CSUM)?;
        Ok(mem::arc(Inner { volume, csums, nodes: Mutex::new(mem::vec()) })?)
    }

    /// The node of inode `ino` of subvolume `subvol`, whose tree is
    /// `tree`: the one in use, or a new one.
    pub(crate) fn node_for(
        this: &KArc<Inner>,
        subvol: u64,
        tree: Root,
        ino: u64,
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
        let node = mem::arc(BtrfsNode::load(this.clone(), subvol, tree, ino)?)?;
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

    /// Its size and what is used of it, as the superblock keeps them;
    /// Linux also takes the profiles and unallocated space into account,
    /// which Phase 10e's `btrfs filesystem usage` shows.
    fn info(&self) -> FsInfo {
        let sb = self.inner.volume.superblock();
        let sector = u64::from(sb.sectorsize);
        FsInfo {
            block_size: sb.sectorsize,
            blocks: sb.total_bytes / sector,
            free: sb.total_bytes.saturating_sub(sb.bytes_used) / sector,
            ..FsInfo::default()
        }
    }

    fn read_only(&self) -> bool {
        true
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

    /// A node holds only memory: nothing is written when it goes.
    fn light_drops(&self) -> bool {
        true
    }
}
