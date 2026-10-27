// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A file, directory or link of a mounted Btrfs filesystem: an inode of
//! one of its subvolumes.
//!
//! A directory finds a name by its hash (`DIR_ITEM`) and lists its
//! entries in the order they were made (`DIR_INDEX`). A file's data goes
//! through its page cache, whose backing finds each page's extent and
//! reads it straight from the disk into the page, checking it against the
//! checksum tree and reading the next copy when it does not match, as
//! Linux does.

use core::any::Any;

use radix_block::{PAGE_SIZE, Page, SegmentMut};
use radix_vfs::cache::{Backing, FileCache};
use radix_vfs::mem::KArc;
use radix_vfs::{Error, In, Kind, ListFn, Node, NodeRef, Out, Result, SocketBinding, Stat};

use crate::dir::Entries;
use crate::extent::{self, Compression, Extent};
use crate::format::{Key, item, name_hash, tree};
use crate::fs::Inner;
use crate::inode::Inode;
use crate::tree::{Cursor, Root};

const PAGE: u64 = PAGE_SIZE as u64;

/// A node of a mounted Btrfs filesystem.
pub struct BtrfsNode {
    pub(crate) fs: KArc<Inner>,
    /// Its subvolume: the number of its tree, and the tree's root.
    pub(crate) subvol: u64,
    pub(crate) tree: Root,
    /// Its inode number in the subvolume.
    pub(crate) ino: u64,
    inode: Inode,
    kind: Kind,
    /// A file's page cache.
    cache: Option<FileCache>,
}

/// A number for the inode `ino` of subvolume `subvol`, unique among the
/// filesystem's nodes: the inode number in the top subvolume, as Linux's
/// `st_ino` is, and with the subvolume's number from bit 40 in the others
/// (Linux tells them apart by `st_dev` instead).
pub fn id_of(subvol: u64, ino: u64) -> u64 {
    if subvol == tree::FS { ino } else { ino ^ (subvol << 40) }
}

impl BtrfsNode {
    /// The node of inode `ino` of subvolume `subvol`, whose tree is
    /// `tree`, read from its inode item.
    pub(crate) fn load(fs: KArc<Inner>, subvol: u64, tree: Root, ino: u64) -> Result<BtrfsNode> {
        let key = Key::new(ino, item::INODE_ITEM, 0);
        let cursor = Cursor::seek(&fs.volume, &tree, &key)?;
        let inode = match cursor.get() {
            Some((found, data)) if found == key => Inode::read(data)?,
            _ => return Err(Error::Corrupt),
        };
        drop(cursor);
        let kind = inode.kind()?;
        let cache = (kind == Kind::File).then(|| FileCache::new(inode.size, inode.mtime));
        Ok(BtrfsNode { fs, subvol, tree, ino, inode, kind, cache })
    }

    fn is_dir(&self) -> Result<()> {
        if self.kind == Kind::Directory { Ok(()) } else { Err(Error::NotDirectory) }
    }

    /// The inode item.
    pub fn inode(&self) -> &Inode {
        &self.inode
    }

    /// What the entry `location` of this directory names: an inode here,
    /// or the top directory of another subvolume.
    fn open_entry(&self, location: &Key) -> Result<NodeRef> {
        let node = match location.kind {
            item::INODE_ITEM => {
                Inner::node_for(&self.fs, self.subvol, self.tree, location.objectid)?
            }
            item::ROOT_ITEM => {
                let (root, dir) =
                    self.fs.volume.subvolume(location.objectid)?.ok_or(Error::NotFound)?;
                Inner::node_for(&self.fs, location.objectid, root, dir)?
            }
            _ => return Err(Error::Corrupt),
        };
        Ok(node)
    }

    /// Reads the file's page `index`, `len` bytes of it, into `page`.
    fn read_page(&self, index: u64, page: &mut Page, len: usize) -> Result<()> {
        let pos = index * PAGE;
        let volume = &self.fs.volume;
        let key = Key::new(self.ino, item::EXTENT_DATA, pos);
        let bytes = page.bytes_mut();
        // The extent that starts at `pos` or before it; when it ends
        // before `pos`, or there is none, the page is in a hole.
        let cursor = Cursor::seek_last(volume, &self.tree, &key)?;
        let found = match &cursor {
            Some(cursor) => match cursor.get() {
                Some((at, data)) if at.is(self.ino, item::EXTENT_DATA) => {
                    let extent = Extent::read(data)?;
                    let into = pos - at.offset;
                    (into < extent.len()).then_some((into, extent))
                }
                _ => None,
            },
            None => None,
        };
        match found {
            None | Some((_, Extent::Prealloc { .. })) => bytes.fill(0),
            Some((into, Extent::Inline { compression, data, .. })) => {
                if compression != Compression::None {
                    return Err(Error::NotSupported);
                }
                // An inline extent is the file's start, and its only extent.
                let from = usize::try_from(into).map_err(|_| Error::Corrupt)?;
                let n = data.len().saturating_sub(from).min(PAGE_SIZE);
                bytes[..n].copy_from_slice(&data[from..from + n]);
                bytes[n..].fill(0);
            }
            Some((
                into,
                Extent::Regular {
                    disk_bytenr, disk_num_bytes, offset, num_bytes, compression, ..
                },
            )) => {
                if disk_bytenr == 0 {
                    bytes.fill(0);
                } else if compression != Compression::None {
                    return Err(Error::NotSupported);
                } else {
                    // Within the extent on the disk, and a whole sector
                    // of it: damage may say otherwise.
                    if offset.checked_add(num_bytes).is_none_or(|end| end > disk_num_bytes) {
                        return Err(Error::Corrupt);
                    }
                    // `into` is below `num_bytes`, so this cannot overflow.
                    let logical = disk_bytenr
                        .checked_add(offset + into)
                        .filter(|at| at.is_multiple_of(PAGE))
                        .ok_or(Error::Corrupt)?;
                    drop(cursor);
                    self.read_data(logical, page)?;
                }
            }
        }
        // The disk's bytes past the end of the file are not the file's.
        page.bytes_mut()[len..].fill(0);
        Ok(())
    }

    /// Reads the data sector at logical address `logical` into `page`,
    /// from the first copy whose checksum matches.
    fn read_data(&self, logical: u64, page: &mut Page) -> Result<()> {
        let volume = &self.fs.volume;
        let sum = if self.inode.has_sums() {
            let csums = self.fs.csums.as_ref().ok_or(Error::Io)?;
            // Data without its checksum fails, as on Linux.
            Some(extent::data_sum(volume, csums, logical)?.ok_or(Error::Io)?)
        } else {
            None
        };
        let place = volume.place(logical)?;
        if place.len < PAGE {
            return Err(Error::Corrupt);
        }
        let device = volume.device();
        let csum = volume.superblock().csum_type;
        let mut failed = Error::Io;
        for &physical in place.copies() {
            if physical.checked_add(PAGE).is_none_or(|end| end > volume.device_bytes()) {
                failed = Error::Corrupt;
                continue;
            }
            let read = device.read(
                physical / radix_block::SECTOR,
                &mut [SegmentMut { page: &mut *page, offset: 0, len: PAGE_SIZE }],
            );
            if let Err(e) = read {
                failed = e.into();
                continue;
            }
            radix_hw::device_wrote(page.bytes_mut());
            match &sum {
                Some(sum) if !csum.matches(page.bytes(), sum) => failed = Error::Io,
                _ => return Ok(()),
            }
        }
        Err(failed)
    }
}

/// A file's page cache backing: its extents, read-only.
struct FileBacking<'a>(&'a BtrfsNode);

impl Backing for FileBacking<'_> {
    fn persistent(&self) -> bool {
        true
    }

    fn read_page(&self, index: u64, page: &mut Page, len: usize) -> Result<()> {
        self.0.read_page(index, page, len)
    }

    fn reserve(&self, _size: u64) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn truncate(&self, _size: u64) -> Result<()> {
        Err(Error::ReadOnly)
    }
}

impl Node for BtrfsNode {
    fn stat(&self) -> Stat {
        // Whole sectors, as Linux counts them.
        let sector = u64::from(self.fs.volume.superblock().sectorsize);
        Stat {
            kind: self.kind,
            size: self.inode.size,
            allocated: self.inode.nbytes.next_multiple_of(sector),
            id: id_of(self.subvol, self.ino),
            created: self.inode.otime,
            modified: self.inode.mtime,
            accessed: self.inode.atime,
        }
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        self.is_dir()?;
        let volume = &self.fs.volume;
        let key = Key::new(self.ino, item::DIR_ITEM, name_hash(name.as_bytes()));
        let location = {
            let cursor = Cursor::seek(volume, &self.tree, &key)?;
            let Some((found, data)) = cursor.get() else { return Err(Error::NotFound) };
            if found != key {
                return Err(Error::NotFound);
            }
            // Names whose hashes are the same share the item.
            let mut location = None;
            for entry in Entries::of(data) {
                let entry = entry?;
                if entry.name == name.as_bytes() {
                    location = Some(entry.location);
                    break;
                }
            }
            location.ok_or(Error::NotFound)?
        };
        self.open_entry(&location)
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        self.is_dir()?;
        let volume = &self.fs.volume;
        let mut at =
            Cursor::seek(volume, &self.tree, &Key::new(self.ino, item::DIR_INDEX, cursor))?;
        while let Some((key, data)) = at.get() {
            if !key.is(self.ino, item::DIR_INDEX) {
                break;
            }
            for entry in Entries::of(data) {
                let entry = entry?;
                // Radix's names are UTF-8; others are left out (Phase 10b).
                let Ok(name) = core::str::from_utf8(entry.name) else { continue };
                let kind = match entry.kind {
                    Some(kind) => kind,
                    None => self.open_entry(&entry.location)?.kind(),
                };
                if !f(key.offset + 1, name, kind) {
                    return Ok(());
                }
            }
            at.advance()?;
        }
        Ok(())
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        match &self.cache {
            Some(cache) => cache.read(offset, out, &FileBacking(self)),
            None if self.kind == Kind::Directory => Err(Error::IsDirectory),
            None => Err(Error::NotSupported),
        }
    }

    fn write(&self, _offset: u64, _data: &dyn In) -> Result<usize> {
        match self.kind {
            Kind::Directory => Err(Error::IsDirectory),
            _ => Err(Error::ReadOnly),
        }
    }

    fn set_size(&self, _size: u64) -> Result<()> {
        match self.kind {
            Kind::Directory => Err(Error::IsDirectory),
            _ => Err(Error::ReadOnly),
        }
    }

    fn set_times(&self, _accessed: Option<u64>, _modified: Option<u64>) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn create(&self, _name: &str, _kind: Kind) -> Result<NodeRef> {
        self.is_dir()?;
        Err(Error::ReadOnly)
    }

    fn make_link(&self, _name: &str, _target: &str) -> Result<NodeRef> {
        self.is_dir()?;
        Err(Error::ReadOnly)
    }

    fn make_socket(&self, _name: &str, _binding: SocketBinding) -> Result<NodeRef> {
        self.is_dir()?;
        Err(Error::ReadOnly)
    }

    fn remove(&self, _name: &str) -> Result<()> {
        self.is_dir()?;
        Err(Error::ReadOnly)
    }

    fn rename(&self, _from: &str, _to_dir: &dyn Node, _to: &str) -> Result<()> {
        self.is_dir()?;
        Err(Error::ReadOnly)
    }

    fn read_link(&self, out: &mut dyn Out) -> Result<usize> {
        if self.kind != Kind::Symlink {
            return Err(Error::NotLink);
        }
        let volume = &self.fs.volume;
        let key = Key::new(self.ino, item::EXTENT_DATA, 0);
        let cursor = Cursor::seek(volume, &self.tree, &key)?;
        let Some((found, data)) = cursor.get() else { return Err(Error::Corrupt) };
        if found != key {
            return Err(Error::Corrupt);
        }
        // A link's path is inline, and not compressed. It is as long as
        // its inode says, as Linux reads it: `mkfs.btrfs` keeps a NUL
        // after it.
        let Extent::Inline { compression: Compression::None, data, .. } = Extent::read(data)?
        else {
            return Err(Error::Corrupt);
        };
        let len = usize::try_from(self.inode.size).map_or(data.len(), |size| size.min(data.len()));
        let n = len.min(out.len());
        out.put(0, &data[..n])?;
        Ok(len)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl BtrfsNode {
    /// Drops up to `max` pages of its cache; returns how many.
    pub(crate) fn shrink(&self, max: usize) -> usize {
        match &self.cache {
            Some(cache) => cache.shrink(max, &FileBacking(self)),
            None => 0,
        }
    }
}
