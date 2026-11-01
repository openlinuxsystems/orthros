// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Extended attributes (ADR 0256), as Linux's ext4 keeps them: first in
//! the inode, in the room after its extra part (`i_extra_isize`), then in
//! a block of their own that the inode names (`i_file_acl`), which inodes
//! with the same attributes may share, counting their references.
//!
//! Each place holds a list of entries: the name's length, its namespace
//! (the name index), where its value is, the value's size and a hash,
//! then the name without its namespace's prefix. Four zero bytes end the
//! list, and the values are at the end of the room, as Linux's
//! `ext4_xattr_set_entry` puts them. A block's entries are in the order
//! of their index, their name's length and their name, the order Linux
//! looks them up in, and the block has a hash of their hashes and, with
//! `metadata_csum`, a checksum.
//!
//! An attribute goes where Linux's `ext4_xattr_set_handle` puts it: in
//! the inode if it fits there, else in the block. A block shared with
//! another inode is copied before it changes. Access lists are kept in
//! ext4's own form ([`acl_to_disk`]) and given in Linux's
//! `posix_acl_xattr` form. A value kept in an inode of its own
//! (`ea_inode`) is listed and kept but not read or changed, and a value
//! too large for the block is refused ([`Error::NoSpace`]), as on a
//! volume without `ea_inode`.

use radix_vfs::acl::{self, Acl, tag};
use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result, Setting};

use crate::crc::crc32c;
use crate::fs::{Inner, Txn, zeroed};
use crate::inode::{Inode, at};
use crate::sb::{self, Geometry, compat};
use crate::{le16, le32, put16, put32};

/// The magic number of the attributes in an inode and of a block of
/// them.
pub(crate) const MAGIC: u32 = 0xea02_0000;

/// The size of a block's header: magic, references, blocks, hash and
/// checksum, then room kept.
const BLOCK_HEADER: usize = 32;

/// The size of an entry before its name.
const ENTRY: usize = 16;

/// The name indexes: Linux's `EXT4_XATTR_INDEX_*`.
mod index {
    pub const USER: u8 = 1;
    pub const ACCESS: u8 = 2;
    pub const DEFAULT: u8 = 3;
    pub const TRUSTED: u8 = 4;
    pub const SECURITY: u8 = 6;
}

/// The version of ext4's access lists (`EXT4_ACL_VERSION`).
const ACL_VERSION: u32 = 1;

/// The version of Linux's `posix_acl_xattr` form.
const XATTR_ACL_VERSION: u32 = 2;

/// `n` rounded up to a multiple of 4 (`EXT4_XATTR_SIZE`).
fn pad(n: usize) -> usize {
    (n + 3) & !3
}

/// The room an entry with a name of `len` bytes takes (`EXT4_XATTR_LEN`).
fn entry_len(len: usize) -> usize {
    pad(ENTRY + len)
}

/// The name index of attribute `name` and the rest of its name, as
/// Linux's handlers split them: [`Error::NotSupported`] for a namespace
/// ext4 has not.
pub(crate) fn split(name: &[u8]) -> Result<(u8, &[u8])> {
    if name == acl::ACCESS {
        return Ok((index::ACCESS, b""));
    }
    if name == acl::DEFAULT {
        return Ok((index::DEFAULT, b""));
    }
    for (index, prefix) in [
        (index::USER, &b"user."[..]),
        (index::TRUSTED, b"trusted."),
        (index::SECURITY, b"security."),
    ] {
        if let Some(rest) = name.strip_prefix(prefix) {
            if rest.len() > usize::from(u8::MAX) {
                return Err(Error::OutOfRange);
            }
            return Ok((index, rest));
        }
    }
    Err(Error::NotSupported)
}

/// Whether the attributes of index `index` are access lists.
pub(crate) fn is_list(index: u8) -> bool {
    matches!(index, index::ACCESS | index::DEFAULT)
}

/// Whether index `index` is that of the access list that decides.
pub(crate) fn is_access(index: u8) -> bool {
    index == index::ACCESS
}

/// Whether index `index` is that of a directory's default access list.
pub(crate) fn is_default(index: u8) -> bool {
    index == index::DEFAULT
}

/// What a name of index `index` starts with when listed; none for an
/// index Linux lists none of (its `ext4_xattr_handler_map`).
fn prefix(index: u8) -> Option<&'static [u8]> {
    Some(match index {
        index::USER => b"user.",
        index::ACCESS => acl::ACCESS,
        index::DEFAULT => acl::DEFAULT,
        index::TRUSTED => b"trusted.",
        index::SECURITY => b"security.",
        _ => return None,
    })
}

/// An attribute as ext4 keeps it.
pub(crate) struct Attr {
    index: u8,
    /// Its name, without its namespace's prefix.
    name: KVec<u8>,
    /// Its value; empty if it is in an inode of its own.
    value: KVec<u8>,
    /// The inode its value is in (`e_value_inum`); 0 if here.
    inum: u32,
    /// The size of its value.
    size: u32,
    /// Its hash as read: kept for a value in an inode of its own.
    hash: u32,
}

impl Attr {
    fn new(index: u8, name: &[u8], value: &[u8]) -> Result<Attr> {
        Ok(Attr {
            index,
            name: mem::copy(name)?,
            value: mem::copy(value)?,
            inum: 0,
            size: value.len() as u32,
            hash: 0,
        })
    }

    fn is(&self, index: u8, name: &[u8]) -> bool {
        self.index == index && *self.name == *name
    }

    /// The room it takes: its entry, and its value if here.
    fn room(&self) -> usize {
        entry_len(self.name.len()) + if self.inum == 0 { pad(self.value.len()) } else { 0 }
    }

    /// Whether it goes before `index` and `name` in a block, as Linux's
    /// `ext4_xattr_find_entry` compares them.
    fn before(&self, index: u8, name: &[u8]) -> bool {
        (self.index, self.name.len(), &*self.name) < (index, name.len(), name)
    }
}

/// The entries of a place: `room` holds them from `first`, and their
/// values' offsets count from its start. Linux's
/// `ext4_xattr_check_entries`: each entry and its value in the room, the
/// values past the end of the list.
fn read_entries(room: &[u8], first: usize, out: &mut KVec<Attr>) -> Result<()> {
    // The end of the list first, which the values are past.
    let mut at = first;
    loop {
        if at + 4 > room.len() {
            return Err(Error::Corrupt);
        }
        if le32(room, at) == 0 {
            break;
        }
        if at + ENTRY > room.len() {
            return Err(Error::Corrupt);
        }
        at += entry_len(usize::from(room[at]));
    }
    let values = at + 4;
    let mut at = first;
    while le32(room, at) != 0 {
        let len = usize::from(room[at]);
        let name = room.get(at + ENTRY..at + ENTRY + len).ok_or(Error::Corrupt)?;
        let offset = usize::from(le16(room, at + 2));
        let inum = le32(room, at + 4);
        let size = le32(room, at + 8);
        let value = if inum == 0 && size != 0 {
            let size = size as usize;
            if offset < values || offset + pad(size) > room.len() {
                return Err(Error::Corrupt);
            }
            mem::copy(&room[offset..offset + size])?
        } else {
            mem::vec()
        };
        mem::push(
            out,
            Attr {
                index: room[at + 1],
                name: mem::copy(name)?,
                value,
                inum,
                size,
                hash: le32(room, at + 12),
            },
        )?;
        at += entry_len(len);
    }
    Ok(())
}

/// Writes `attrs` into `room`: their entries from `first`, their values
/// from its end down, with offsets from its start, and zeros between.
/// Entries get hashes if `hashes`, as a block's do; in an inode Linux
/// leaves them 0.
fn write_entries(room: &mut [u8], first: usize, attrs: &[Attr], hashes: bool) {
    room[first..].fill(0);
    let mut at = first;
    let mut end = room.len();
    for attr in attrs {
        let len = attr.name.len();
        room[at] = len as u8;
        room[at + 1] = attr.index;
        put32(room, at + 4, attr.inum);
        put32(room, at + 8, attr.size);
        room[at + ENTRY..at + ENTRY + len].copy_from_slice(&attr.name);
        let hash = if attr.inum != 0 {
            attr.hash
        } else {
            let size = pad(attr.value.len());
            let mut offset = 0;
            if size != 0 {
                end -= size;
                offset = end;
                room[end..end + attr.value.len()].copy_from_slice(&attr.value);
            }
            put16(room, at + 2, offset as u16);
            if hashes { entry_hash(&attr.name, &room[offset..offset + size]) } else { 0 }
        };
        put32(room, at + 12, hash);
        at += entry_len(len);
    }
}

/// An entry's hash, of its name and its value's words, as Linux's
/// `ext4_xattr_hash_entry` (with the name's bytes unsigned, as Linux 6.2
/// and e2fsprogs 1.47 hash them).
fn entry_hash(name: &[u8], value: &[u8]) -> u32 {
    let mut hash = 0u32;
    for &b in name {
        hash = (hash << 5) ^ (hash >> 27) ^ u32::from(b);
    }
    for word in value.as_chunks::<4>().0 {
        hash = (hash << 16) ^ (hash >> 16) ^ u32::from_le_bytes(*word);
    }
    hash
}

/// A block's hash, of its entries' hashes; 0 if one is 0, as Linux's
/// `ext4_xattr_rehash`.
fn block_hash(block: &[u8]) -> u32 {
    let mut hash = 0u32;
    let mut at = BLOCK_HEADER;
    while le32(block, at) != 0 {
        let entry = le32(block, at + 12);
        if entry == 0 {
            return 0;
        }
        hash = (hash << 16) ^ (hash >> 16) ^ entry;
        at += entry_len(usize::from(block[at]));
    }
    hash
}

/// The checksum of block number `nr` of attributes, `block`, with its
/// checksum read as 0 (Linux's `ext4_xattr_block_csum`).
fn block_sum(geo: &Geometry, nr: u64, block: &[u8]) -> u32 {
    let mut sum = crc32c(geo.seed, &nr.to_le_bytes());
    sum = crc32c(sum, &block[..0x10]);
    sum = crc32c(sum, &[0; 4]);
    crc32c(sum, &block[0x14..])
}

/// Where the attributes in an inode of bytes `raw` start, at their magic
/// number: after its extra part, with room for the magic number and an
/// end. None for an inode with no room for them.
fn inode_room(raw: &[u8]) -> Option<usize> {
    if raw.len() <= 128 {
        return None;
    }
    let start = 128 + usize::from(le16(raw, at::EXTRA_ISIZE));
    (start.is_multiple_of(4) && start + 8 <= raw.len()).then_some(start)
}

/// Reads the attributes in the inode of bytes `raw` into `out`.
fn read_inode(raw: &[u8], out: &mut KVec<Attr>) -> Result<()> {
    match inode_room(raw) {
        Some(start) if le32(raw, start) == MAGIC => read_entries(&raw[start + 4..], 0, out),
        _ => Ok(()),
    }
}

/// Reads the attributes in block `nr` into `out`, checking its header
/// and checksum; gives the references it has.
fn read_block(fs: &Inner, nr: u64, out: &mut KVec<Attr>) -> Result<u32> {
    let geo = &fs.geo;
    if nr < geo.first_data_block || nr >= geo.blocks {
        return Err(Error::Corrupt);
    }
    let mut block = zeroed(geo.block_size as usize)?;
    fs.read(geo.offset(nr), &mut block)?;
    if le32(&block, 0) != MAGIC || le32(&block, 8) != 1 {
        return Err(Error::Corrupt);
    }
    if geo.metadata_csum() && le32(&block, 0x10) != block_sum(geo, nr, &block) {
        return Err(Error::Corrupt);
    }
    read_entries(&block, BLOCK_HEADER, out)?;
    Ok(le32(&block, 4))
}

/// Every attribute of an inode: in it, from its bytes `raw`, then in its
/// block `file_acl`.
fn read_all(fs: &Inner, raw: &[u8], file_acl: u64) -> Result<KVec<Attr>> {
    let mut attrs = mem::vec();
    read_inode(raw, &mut attrs)?;
    if file_acl != 0 {
        read_block(fs, file_acl, &mut attrs)?;
    }
    Ok(attrs)
}

/// The value of an inode's attribute of index `index` named `name`, from
/// its bytes `raw` and its block `file_acl`: [`Error::NoAttribute`] if it
/// has none. An access list is given as ext4 keeps it.
pub(crate) fn get(
    fs: &Inner,
    raw: &[u8],
    file_acl: u64,
    index: u8,
    name: &[u8],
) -> Result<KVec<u8>> {
    let attrs = read_all(fs, raw, file_acl)?;
    let attr = attrs.iter().find(|a| a.is(index, name)).ok_or(Error::NoAttribute)?;
    if attr.inum != 0 {
        return Err(Error::NotSupported);
    }
    Ok(mem::copy(&attr.value)?)
}

/// The names of an inode's attributes, each with a NUL after it, those
/// in the inode first, as Linux's `ext4_listxattr` gives them.
pub(crate) fn list(fs: &Inner, raw: &[u8], file_acl: u64) -> Result<KVec<u8>> {
    let attrs = read_all(fs, raw, file_acl)?;
    let mut names = mem::vec();
    for attr in &attrs {
        let Some(prefix) = prefix(attr.index) else { continue };
        mem::extend(&mut names, prefix)?;
        mem::extend(&mut names, &attr.name)?;
        mem::extend(&mut names, &[0])?;
    }
    Ok(names)
}

/// The room `attrs` take, leaving out the one at `but`.
fn room_of(attrs: &[Attr], but: Option<usize>) -> usize {
    attrs.iter().enumerate().filter(|&(i, _)| Some(i) != but).map(|(_, a)| a.room()).sum()
}

/// Sets an inode's attribute of index `index` named `name` to `value`,
/// as `how` says, or removes it given none, as Linux's
/// `ext4_xattr_set_handle` does: into the inode, whose bytes are `raw`
/// and fields `inode`, if it fits there, else into its block, which a
/// new one near `home` replaces if it has none or shares it. False if
/// nothing changed: the attribute had the value. The checks come before
/// any change, and a change that fails part way fails the volume.
#[allow(clippy::too_many_arguments)]
pub(crate) fn change(
    fs: &Inner,
    txn: &mut Txn,
    raw: &mut [u8],
    inode: &mut Inode,
    home: u64,
    index: u8,
    name: &[u8],
    value: Option<(&[u8], Setting)>,
) -> Result<bool> {
    let geo = &fs.geo;
    let mut here = mem::vec();
    read_inode(raw, &mut here)?;
    let mut there = mem::vec();
    let refs = if inode.file_acl != 0 { read_block(fs, inode.file_acl, &mut there)? } else { 0 };
    let in_inode = here.iter().position(|a| a.is(index, name));
    let in_block = there.iter().position(|a| a.is(index, name));
    let found = match (in_inode, in_block) {
        (Some(i), _) => Some(&here[i]),
        (None, Some(i)) => Some(&there[i]),
        (None, None) => None,
    };
    if found.is_some_and(|a| a.inum != 0) {
        return Err(Error::NotSupported);
    }
    match (value, found) {
        (None | Some((_, Setting::Replace)), None) => return Err(Error::NoAttribute),
        (Some((_, Setting::Create)), Some(_)) => return Err(Error::Exists),
        (Some((value, _)), Some(attr)) if *attr.value == *value => return Ok(false),
        _ => {}
    }
    let (mut here_changed, mut there_changed) = (false, false);
    match value {
        None => {
            if let Some(i) = in_inode {
                here.remove(i);
                here_changed = true;
            } else if let Some(i) = in_block {
                there.remove(i);
                there_changed = true;
            }
        }
        Some((value, _)) => {
            let attr = Attr::new(index, name, value)?;
            // The inode first, as Linux tries it.
            let fits = inode_room(raw).is_some_and(|start| {
                room_of(&here, in_inode) + attr.room() + 4 <= raw.len() - start - 4
            });
            if fits {
                match in_inode {
                    Some(i) => here[i] = attr,
                    None => mem::push(&mut here, attr)?,
                }
                here_changed = true;
                if let Some(i) = in_block {
                    there.remove(i);
                    there_changed = true;
                }
            } else {
                let room = geo.block_size as usize - BLOCK_HEADER;
                if room_of(&there, in_block) + attr.room() + 4 > room {
                    return Err(Error::NoSpace);
                }
                match in_block {
                    Some(i) => there[i] = attr,
                    None => {
                        let at = there
                            .iter()
                            .position(|a| !a.before(index, name))
                            .unwrap_or(there.len());
                        mem::insert(&mut there, at, attr)?;
                    }
                }
                there_changed = true;
                if let Some(i) = in_inode {
                    here.remove(i);
                    here_changed = true;
                }
            }
        }
    }
    // A new block, if one is needed: for the first attributes there, or
    // in place of one another inode shares.
    let new_block = there_changed && !there.is_empty() && (inode.file_acl == 0 || refs > 1);
    let mut block = if there_changed { Some(zeroed(geo.block_size as usize)?) } else { None };
    if new_block {
        fs.room(txn, 1, 0)?;
    }
    let changed = (|| {
        let per = u64::from(geo.block_size) / 512;
        if let Some(block) = &mut block {
            if there.is_empty() {
                fs.drop_attributes(txn, inode.file_acl)?;
                inode.file_acl = 0;
                inode.sectors = inode.sectors.saturating_sub(per);
            } else {
                let nr = if new_block {
                    let (nr, _) = fs.take_blocks(txn, home, 1)?;
                    if inode.file_acl != 0 {
                        // Shared: the others keep it.
                        fs.drop_attributes(txn, inode.file_acl)?;
                    } else {
                        inode.sectors += per;
                    }
                    inode.file_acl = nr;
                    nr
                } else {
                    inode.file_acl
                };
                put32(block, 0, MAGIC);
                put32(block, 4, 1);
                put32(block, 8, 1);
                write_entries(block, BLOCK_HEADER, &there, true);
                let hash = block_hash(block);
                put32(block, 0xc, hash);
                if geo.metadata_csum() {
                    let sum = block_sum(geo, nr, block);
                    put32(block, 0x10, sum);
                }
                fs.write(txn, geo.offset(nr), block)?;
            }
        }
        if here_changed && let Some(start) = inode_room(raw) {
            put32(raw, start, if here.is_empty() { 0 } else { MAGIC });
            write_entries(&mut raw[start + 4..], 0, &here, false);
        }
        // The volume says it has attributes, as Linux's
        // `ext4_xattr_update_super_block` has it say.
        let features = le32(&txn.sb, sb::at::COMPAT);
        if features & compat::EXT_ATTR == 0 {
            put32(&mut txn.sb, sb::at::COMPAT, features | compat::EXT_ATTR);
        }
        Ok(())
    })();
    if changed.is_err() {
        fs.fail();
    }
    changed.map(|()| true)
}

/// ext4's form of an access list given in Linux's `posix_acl_xattr`
/// form, as `ext4_acl_to_disk` writes it: a version, then each entry's
/// tag and permissions, and a number for a named user's or group's.
pub(crate) fn acl_to_disk(value: &[u8]) -> Result<KVec<u8>> {
    let list = Acl::parse(value)?.ok_or(Error::InvalidArgument)?;
    let mut out = mem::vec_with_capacity(4 + list.entries().len() * 8)?;
    mem::extend(&mut out, &ACL_VERSION.to_le_bytes())?;
    for entry in list.entries() {
        mem::extend(&mut out, &entry.tag.to_le_bytes())?;
        mem::extend(&mut out, &entry.perm.to_le_bytes())?;
        if matches!(entry.tag, tag::USER | tag::GROUP) {
            mem::extend(&mut out, &entry.id.to_le_bytes())?;
        }
    }
    Ok(out)
}

/// Linux's `posix_acl_xattr` form of an access list ext4 keeps, as
/// `ext4_acl_from_disk` reads it: [`Error::InvalidArgument`] for one it
/// cannot read, and [`Error::NoAttribute`] for one with no entries.
pub(crate) fn acl_from_disk(disk: &[u8]) -> Result<KVec<u8>> {
    if disk.len() < 4 || le32(disk, 0) != ACL_VERSION {
        return Err(Error::InvalidArgument);
    }
    if disk.len() == 4 {
        return Err(Error::NoAttribute);
    }
    let mut out = mem::vec_with_capacity(4 + (disk.len() - 4) * 2)?;
    mem::extend(&mut out, &XATTR_ACL_VERSION.to_le_bytes())?;
    let mut at = 4;
    while at < disk.len() {
        if at + 4 > disk.len() {
            return Err(Error::InvalidArgument);
        }
        let tag_and_perm = &disk[at..at + 4];
        let (id, len) = match le16(disk, at) {
            tag::USER | tag::GROUP if at + 8 <= disk.len() => (le32(disk, at + 4), 8),
            tag::USER_OBJ | tag::GROUP_OBJ | tag::MASK | tag::OTHER => (u32::MAX, 4),
            _ => return Err(Error::InvalidArgument),
        };
        mem::extend(&mut out, tag_and_perm)?;
        mem::extend(&mut out, &id.to_le_bytes())?;
        at += len;
    }
    Ok(out)
}
