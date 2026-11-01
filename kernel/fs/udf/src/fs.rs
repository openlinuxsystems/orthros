// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The filesystem: the volume, and the nodes in use.
//!
//! A node is made for a file when it is looked up, from the file entry
//! its ICB points to, and kept in a table while it is in use, so that
//! each file has one node.

use alloc::sync::Weak;
use core::fmt;

use radix_block::DeviceRef;
use radix_block::cache::BlockCache;
use radix_sync::Mutex;
use radix_vfs::mem::{self, Alloc, KArc, KBox, KVec};
use radix_vfs::{Error, FileSystem, FsInfo, FsKind, Kind, Node, NodeRef, Result};

use crate::format::{LongAd, ident, tag};
use crate::node::{Entry, UdfNode, kind_of};
use crate::volume::Volume;

/// What the filesystem's nodes share.
pub(crate) struct Inner {
    pub(crate) cache: BlockCache,
    pub(crate) volume: KBox<Volume>,
    nodes: Mutex<KVec<(u64, Weak<UdfNode, Alloc>)>>,
}

/// A UDF volume, mounted read-only.
pub struct UdfFs {
    inner: KArc<Inner>,
    root: KArc<UdfNode>,
}

impl UdfFs {
    /// Mounts the UDF volume on `device`. Fails with
    /// [`Error::UnknownFilesystem`] if it holds none, and with
    /// [`Error::ReadOnly`] when asked to write.
    pub fn mount(device: DeviceRef, read_only: bool) -> Result<KArc<UdfFs>> {
        let cache = BlockCache::new(device);
        let volume = Volume::find(&cache)?;
        if !read_only {
            return Err(Error::ReadOnly);
        }
        let root = volume.root;
        let inner = mem::arc(Inner { cache, volume, nodes: Mutex::new(mem::vec()) })?;
        let root = Inner::node_at(&inner, root)?;
        if root.kind() != Kind::Directory {
            return Err(Error::Corrupt);
        }
        Ok(mem::arc(UdfFs { inner, root })?)
    }
}

impl Inner {
    /// The kind of the file whose ICB is `icb`, from its entry's first
    /// bytes.
    pub(crate) fn kind_at(&self, icb: LongAd) -> Result<Kind> {
        let (at, _) = self.volume.locate(icb.partition, icb.block)?;
        let mut head = [0u8; 28];
        self.cache.read(at, &mut head)?;
        Ok(match tag(&head, Some(icb.block)) {
            Some(ident::FILE_ENTRY | ident::EXTENDED_FILE_ENTRY) => kind_of(head[27]),
            _ => Kind::File,
        })
    }

    /// The node in use for the file whose ICB is at byte `at`.
    fn live(&self, at: u64) -> Option<KArc<UdfNode>> {
        let nodes = self.nodes.lock();
        let i = nodes.binary_search_by_key(&at, |(k, _)| *k).ok()?;
        nodes[i].1.upgrade()
    }

    /// The node for the file whose ICB is `icb`: the one in use, or a new
    /// one.
    pub(crate) fn node_at(this: &KArc<Inner>, icb: LongAd) -> Result<KArc<UdfNode>> {
        let (at, _) = this.volume.locate(icb.partition, icb.block)?;
        if let Some(node) = this.live(at) {
            return Ok(node);
        }
        let entry = Entry::read(&this.volume, &this.cache, icb)?;
        let node = UdfNode::new(this, at, entry)?;
        let weak = KArc::downgrade(&node);
        let mut nodes = this.nodes.lock();
        match nodes.binary_search_by_key(&at, |(k, _)| *k) {
            Ok(i) => match nodes[i].1.upgrade() {
                // Another thread made it meanwhile: its node is the one.
                Some(other) => return Ok(other),
                None => nodes[i].1 = weak,
            },
            Err(_) => {
                if nodes.len() == nodes.capacity() {
                    nodes.retain(|(_, weak)| weak.strong_count() > 0);
                }
                let i = nodes.binary_search_by_key(&at, |(k, _)| *k).unwrap_or_else(|i| i);
                mem::insert(&mut nodes, i, (at, weak))?;
            }
        }
        Ok(node)
    }

    /// The nodes in use, for shrinking their caches.
    fn live_nodes(&self) -> Result<KVec<KArc<UdfNode>>> {
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

impl FileSystem for UdfFs {
    fn kind(&self) -> FsKind {
        FsKind::Udf
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        let volume = &self.inner.volume;
        FsInfo {
            block_size: volume.block as u32,
            blocks: volume.blocks,
            free: 0,
            files: 0,
            free_files: 0,
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    fn label(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        out.write_str(self.inner.volume.label.name().unwrap_or(""))
    }

    fn shrink(&self, max: usize) -> usize {
        let mut dropped = self.inner.cache.shrink(max);
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

    fn light_drops(&self) -> bool {
        true
    }
}
