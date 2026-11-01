// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! An NTFS volume: its master file table, the files Radix reads at mount,
//! the room on it, and the table of the nodes in use.
//!
//! A mount reads the boot sector, then the master file table's own
//! record, the first, which says where the rest of the table is; then
//! `$UpCase`, the up-case table, and `$Volume`, the label, version and
//! flags. Records are read and written through a cache of the device's
//! sectors, as the other filesystems' metadata is, so changes stay in
//! memory until the volume syncs; file data goes through each file's page
//! cache ([`crate::node`]). The first records are written to `$MFTMirr`
//! too, as Windows keeps them.
//!
//! # Writing
//!
//! Radix does not keep NTFS's journal, `$LogFile` (ADR 0237). A volume
//! is mounted to write only when Windows left it clean: not hibernated,
//! not marked dirty, and with a log that needs nothing replayed; any
//! other fails with [`Error::NotSupported`] and mounts read-only. Before
//! its first change Radix empties the log, as ntfs-3g does, so that
//! Windows starts a new one, and marks the volume dirty (in `$Volume`, at
//! once); a sync that has written every change, with none made
//! meanwhile, clears the mark, as for exFAT, so that after a power cut
//! `chkdsk` knows to check it.
//!
//! # Locks
//!
//! - `ns` (the namespace) is held by every directory operation (looking
//!   names up, listing, creating, removing, renaming, linking) and by a
//!   node's write-back, which changes its names' entries in its
//!   directories. A file's record is read and written only under it, but
//!   for the master file table's own and `$Volume`'s (under `space`), and
//!   a removed file's, freed when its last reference goes.
//! - A node's page cache, then its `state`, then `space` (the bitmaps),
//!   then `mft` (the table's runs), then the block cache. One node's
//!   state is held at a time, and `ns` before any of them, never after.
//! - `nodes` (the node table) and `dirty` (nodes waiting to sync) take
//!   no other lock under them.

use alloc::sync::Weak;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_block::cache::BlockCache;
use radix_block::mem::CachePage;
use radix_block::{DeviceRef, PAGE_SIZE, SECTOR, Segment};
use radix_sync::Mutex;
use radix_vfs::mem::{self, Alloc, KArc, KBox, KVec};
use radix_vfs::{Error, FileSystem, FsInfo, FsKind, Kind, Node, NodeRef, Result};

use crate::boot::Geometry;
use crate::edit::{self, Sizes, fix_down, put_u16};
use crate::index::{FileName, NAME_MAX, namespace};
use crate::list::Extent;
use crate::node::NtfsNode;
use crate::record::{
    self, Attr, Record, attr_flags, fix_up, kind, list_entries, name_is, record_of, u16_at, u32_at,
    u64_at,
};
use crate::runs::{HOLE, Runs};
use crate::space::{Bits, Clusters, read_runs, write_runs};
use crate::upcase::{self, Upcase};

/// The records of the system's files.
pub mod records {
    /// The master file table.
    pub const MFT: u64 = 0;
    /// The copy of its first records.
    pub const MFT_MIRROR: u64 = 1;
    /// The journal.
    pub const LOG: u64 = 2;
    /// The volume's label, version and flags.
    pub const VOLUME: u64 = 3;
    /// The root directory.
    pub const ROOT: u64 = 5;
    /// Which clusters are in use.
    pub const BITMAP: u64 = 6;
    /// The up-case table.
    pub const UPCASE: u64 = 10;
    /// The first record of a file of the user's: those before are the
    /// system's, which a listing leaves out, as Linux's drivers do.
    pub const FIRST_USER: u64 = 16;
    /// The first record a new file takes: those before are kept for the
    /// system, as Windows keeps them.
    pub const FIRST_FREE: u64 = 24;
}

/// Bits of the volume's flags.
pub mod volume_flags {
    /// The volume may not be consistent: Windows marks it while mounted,
    /// and `chkdsk` clears it.
    pub const DIRTY: u16 = 0x0001;
}

/// The largest attribute list Radix reads; Windows keeps them far
/// smaller.
const MAX_LIST: u64 = 1 << 20;

/// The records the master file table grows by at least; it grows by a
/// sixteenth of itself when that is more, so that the runs its record 0
/// must hold stay few however large it grows.
const GROW_RECORDS: u64 = 16;

/// The time NTFS counts from, 1601, as NTFS's 100 ns before 1970.
const NT_EPOCH: u64 = 116_444_736_000_000_000;

/// An NTFS time, 100 ns since 1601, as nanoseconds since 1970 (0 for
/// earlier).
pub fn nt_time(t: u64) -> u64 {
    t.saturating_sub(NT_EPOCH).saturating_mul(100)
}

/// A zeroed buffer of `len` bytes.
pub(crate) fn zeroed(len: usize) -> Result<KVec<u8>> {
    let mut v = mem::vec_with_capacity(len)?;
    for _ in 0..len {
        let _ = v.push_within_capacity(0u8);
    }
    Ok(v)
}

/// A non-resident attribute: its clusters and sizes.
pub(crate) struct Stream {
    pub(crate) runs: Runs,
    /// Its size in bytes.
    pub(crate) size: u64,
    /// How much was written: what lies past it reads as zeros.
    pub(crate) initialized: u64,
    /// Its flags ([`attr_flags`]).
    pub(crate) flags: u16,
    /// The log2 of a compression unit's clusters; 0 if not compressed.
    pub(crate) unit: u8,
}

impl Stream {
    /// Whether its data is compressed or sparse: its clusters are a unit's
    /// at a time, and it may have holes.
    pub(crate) fn packed(&self) -> bool {
        self.flags & (record::attr_flags::COMPRESSED | record::attr_flags::SPARSE) != 0
    }

    /// Whether its data is compressed.
    pub(crate) fn compressed(&self) -> bool {
        self.flags & record::attr_flags::COMPRESSED != 0
    }

    /// The clusters of its compression unit; 1 for data that has none.
    pub(crate) fn grain(&self) -> u64 {
        if self.packed() { 1 << self.unit } else { 1 }
    }

    /// An empty one.
    pub(crate) fn new() -> Stream {
        Stream { runs: Runs::new(), size: 0, initialized: 0, flags: 0, unit: 0 }
    }
}

/// An attribute's value, in its record or in clusters.
pub(crate) enum Value {
    Resident(KVec<u8>),
    Clusters(Stream),
}

impl Value {
    /// Its size in bytes.
    pub(crate) fn size(&self) -> u64 {
        match self {
            Value::Resident(bytes) => bytes.len() as u64,
            Value::Clusters(stream) => stream.size,
        }
    }

    /// Its flags ([`attr_flags`]); a resident value's are none that
    /// matter.
    pub(crate) fn flags(&self) -> u16 {
        match self {
            Value::Resident(_) => 0,
            Value::Clusters(stream) => stream.flags,
        }
    }
}

/// What `$STANDARD_INFORMATION` says of a file.
#[derive(Clone, Copy, Default)]
pub(crate) struct Info {
    pub(crate) created: u64,
    pub(crate) modified: u64,
    /// When its record last changed.
    pub(crate) changed: u64,
    pub(crate) accessed: u64,
    /// Its DOS attributes ([`crate::index::attributes`]).
    pub(crate) attributes: u32,
    /// Its security descriptor's number in `$Secure` (NTFS 3.x).
    pub(crate) security: u32,
}

/// The attributes of a file Radix reads, from all its records.
#[derive(Default)]
pub(crate) struct Loaded {
    pub(crate) sequence: u16,
    /// Its record's flags ([`record::flags`]).
    pub(crate) flags: u16,
    pub(crate) info: Info,
    /// Its names but the short ones: its hard links.
    pub(crate) links: u32,
    /// The directory of its first such name.
    pub(crate) parent: u64,
    /// Its data: the unnamed `$DATA`.
    pub(crate) data: Option<Value>,
    /// Its directory index: `$I30`'s root, blocks and bitmap; or
    /// `$Reparse`'s `$R`, a view index.
    pub(crate) root: Option<KVec<u8>>,
    pub(crate) blocks: Option<Value>,
    pub(crate) bitmap: Option<Value>,
    pub(crate) reparse: Option<Value>,
    /// `$Volume`'s label and its version and flags.
    pub(crate) label: Option<KVec<u8>>,
    pub(crate) version: Option<(u8, u8, u16)>,
    /// Its unnamed `$BITMAP`: the master file table's, of its records.
    pub(crate) own_bitmap: Option<Value>,
    /// Whether its base record has an attribute list.
    pub(crate) list: bool,
    /// The pieces of its data in its extension records, in order.
    pub(crate) extents: Option<KVec<Extent>>,
    /// Whether attributes other than its data's are in its extension
    /// records, or it is the system's: Radix does not change it.
    pub(crate) listed: bool,
    /// Whether its index is `$R`, of reparse points, not of names.
    pub(crate) view: bool,
}

/// Adds the attribute `attr` to the value in `slot`: a resident one, or a
/// non-resident one's first or next piece.
fn piece(slot: &mut Option<Value>, attr: &Attr<'_>, clusters: u64) -> Result<()> {
    if !attr.non_resident() {
        if slot.is_some() {
            return Err(Error::Corrupt);
        }
        *slot = Some(Value::Resident(mem::copy(attr.value())?));
        return Ok(());
    }
    match slot {
        None if attr.lowest_vcn() == 0 => {
            let mut runs = Runs::new();
            runs.add(attr.runs(), 0, attr.highest_vcn(), clusters)?;
            let stream = Stream {
                runs,
                size: attr.size(),
                initialized: attr.initialized_size().min(attr.size()),
                flags: attr.flags(),
                unit: attr.compression_unit(),
            };
            *slot = Some(Value::Clusters(stream));
            Ok(())
        }
        Some(Value::Clusters(stream)) => {
            stream.runs.add(attr.runs(), attr.lowest_vcn(), attr.highest_vcn(), clusters)
        }
        _ => Err(Error::Corrupt),
    }
}

/// Takes what Radix reads of the attribute `attr` into `loaded`.
fn take(loaded: &mut Loaded, attr: &Attr<'_>, clusters: u64) -> Result<()> {
    let named = |want| name_is(attr.name(), want);
    match attr.kind() {
        kind::STANDARD_INFORMATION => {
            let v = attr.value();
            if v.len() < 0x24 {
                return Err(Error::Corrupt);
            }
            loaded.info = Info {
                created: nt_time(u64_at(v, 0)),
                modified: nt_time(u64_at(v, 8)),
                changed: nt_time(u64_at(v, 0x10)),
                accessed: nt_time(u64_at(v, 0x18)),
                attributes: u32_at(v, 0x20),
                security: if v.len() >= 0x38 { u32_at(v, 0x34) } else { 0 },
            };
        }
        kind::FILE_NAME => {
            let name = FileName::new(attr.value())?;
            if name.namespace() != namespace::DOS {
                loaded.links += 1;
                if loaded.parent == 0 {
                    loaded.parent = name.parent();
                }
            }
        }
        kind::DATA if attr.unnamed() => piece(&mut loaded.data, attr, clusters)?,
        kind::INDEX_ROOT if named("$I30") || named("$R") => {
            if attr.non_resident() || loaded.root.is_some() {
                return Err(Error::Corrupt);
            }
            loaded.view = named("$R");
            loaded.root = Some(mem::copy(attr.value())?);
        }
        kind::INDEX_ALLOCATION if named("$I30") || named("$R") => {
            piece(&mut loaded.blocks, attr, clusters)?;
        }
        kind::BITMAP if named("$I30") || named("$R") => piece(&mut loaded.bitmap, attr, clusters)?,
        kind::BITMAP if attr.unnamed() => piece(&mut loaded.own_bitmap, attr, clusters)?,
        kind::REPARSE_POINT => piece(&mut loaded.reparse, attr, clusters)?,
        kind::VOLUME_NAME => loaded.label = Some(mem::copy(attr.value())?),
        kind::VOLUME_INFORMATION => {
            let v = attr.value();
            if v.len() < 12 {
                return Err(Error::Corrupt);
            }
            loaded.version = Some((v[8], v[9], record::u16_at(v, 10)));
        }
        _ => {}
    }
    Ok(())
}

/// The master file table's clusters and how many records it has.
struct Mft {
    runs: Runs,
    records: u64,
}

/// The copy of the first records, and how many it has.
struct Mirror {
    runs: Runs,
    records: u64,
}

/// What reads and writes the volume's records and attributes.
pub(crate) struct Volume {
    pub(crate) geo: Geometry,
    pub(crate) cache: BlockCache,
    mft: Mutex<Mft>,
    /// `$MFTMirr`, on a volume mounted to write.
    mirror: Option<Mirror>,
}

impl Volume {
    /// How many records the master file table has.
    pub(crate) fn records(&self) -> u64 {
        self.mft.lock().records
    }

    /// Reads record `number` into `buf`, its update sequence put back.
    pub(crate) fn read_record(&self, number: u64, buf: &mut [u8]) -> Result<()> {
        let rs = u64::from(self.geo.record_size);
        {
            let mft = self.mft.lock();
            if number >= mft.records {
                return Err(Error::Corrupt);
            }
            read_runs(&self.cache, &self.geo, &mft.runs, number * rs, buf)?;
        }
        fix_up(buf, b"FILE")
    }

    /// Writes `buf` as record `number`, its update sequence put on, and to
    /// `$MFTMirr` too if it keeps that one: in the cache, or at once if
    /// `through`.
    pub(crate) fn write_record(&self, number: u64, buf: &[u8], through: bool) -> Result<()> {
        self.write_record_in(number, buf, through, false)
    }

    /// As [`Volume::write_record`], `$MFTMirr`'s copy first if
    /// `mirror_first`: written at once, the copy written last is the one
    /// a power cut in between leaves as it was.
    pub(crate) fn write_record_in(
        &self,
        number: u64,
        buf: &[u8],
        through: bool,
        mirror_first: bool,
    ) -> Result<()> {
        let rs = u64::from(self.geo.record_size);
        let mut out = mem::copy(buf)?;
        fix_down(&mut out)?;
        let records = self.mft.lock().records;
        if number >= records {
            return Err(Error::Corrupt);
        }
        let mirrored = self.mirror.as_ref().filter(|mirror| number < mirror.records);
        for mirror in [mirror_first, !mirror_first] {
            if mirror {
                if let Some(copy) = mirrored {
                    write_runs(&self.cache, &self.geo, &copy.runs, number * rs, &out, through)?;
                }
            } else {
                let mft = self.mft.lock();
                write_runs(&self.cache, &self.geo, &mft.runs, number * rs, &out, through)?;
            }
        }
        Ok(())
    }

    /// The attributes of the file whose base record is `number`, across
    /// its records. Fails with [`Error::Corrupt`] for a record no file
    /// is in, or that is another's.
    #[inline(never)]
    pub(crate) fn load(&self, number: u64) -> Result<Loaded> {
        let rs = self.geo.record_size as usize;
        let mut base = zeroed(rs)?;
        self.read_record(number, &mut base)?;
        let rec = Record::new(&base)?;
        if !rec.in_use() || rec.base() != 0 {
            return Err(Error::Corrupt);
        }
        let mut loaded =
            Loaded { sequence: rec.sequence(), flags: rec.flags(), ..Loaded::default() };
        let clusters = self.geo.clusters;
        let mut list = None;
        for attr in rec.attrs() {
            let attr = attr?;
            if attr.kind() == kind::ATTRIBUTE_LIST {
                list = Some(self.whole(&attr, MAX_LIST)?);
                break;
            }
        }
        let Some(list) = list else {
            for attr in rec.attrs() {
                take(&mut loaded, &attr?, clusters)?;
            }
            return Ok(loaded);
        };
        loaded.list = true;
        loaded.listed = number < records::FIRST_USER;
        // Every attribute is in the list, the base record's too, in order:
        // a non-resident one's pieces by their first VCN.
        let mut other = zeroed(rs)?;
        let mut other_number = None;
        for entry in list_entries(&list) {
            let entry = entry?;
            let at = record_of(entry.reference());
            let bytes: &[u8] = if at == number {
                &base
            } else {
                if entry.kind() == kind::DATA && entry.name().is_empty() {
                    let extent = Extent {
                        reference: entry.reference(),
                        lowest: entry.lowest_vcn(),
                        id: entry.id(),
                    };
                    mem::push(loaded.extents.get_or_insert_with(mem::vec), extent)?;
                } else {
                    loaded.listed = true;
                }
                if other_number != Some(at) {
                    self.read_record(at, &mut other)?;
                    let rec = Record::new(&other)?;
                    if !rec.in_use() || record_of(rec.base()) != number {
                        return Err(Error::Corrupt);
                    }
                    other_number = Some(at);
                }
                &other
            };
            let rec = Record::new(bytes)?;
            let mut hit = None;
            for attr in rec.attrs() {
                let attr = attr?;
                if attr.kind() == entry.kind() && attr.id() == entry.id() {
                    hit = Some(attr);
                    break;
                }
            }
            take(&mut loaded, &hit.ok_or(Error::Corrupt)?, clusters)?;
        }
        Ok(loaded)
    }

    /// The whole value of `attr`, of `max` bytes at most.
    fn whole(&self, attr: &Attr<'_>, max: u64) -> Result<KVec<u8>> {
        let mut value = None;
        piece(&mut value, attr, self.geo.clusters)?;
        self.read_whole(value.as_ref().ok_or(Error::Corrupt)?, max)
    }

    /// The whole of `value`, of `max` bytes at most.
    pub(crate) fn read_whole(&self, value: &Value, max: u64) -> Result<KVec<u8>> {
        if value.size() > max {
            return Err(Error::Corrupt);
        }
        let mut bytes = zeroed(value.size() as usize)?;
        self.read_value(value, 0, &mut bytes)?;
        Ok(bytes)
    }

    /// Reads `buf.len()` bytes of `value` from byte `at`; past its end, or
    /// past what was written of it, zeros. A compressed value is a file's
    /// data, which goes through its page cache, not here.
    pub(crate) fn read_value(&self, value: &Value, at: u64, buf: &mut [u8]) -> Result<()> {
        match value {
            Value::Resident(bytes) => {
                buf.fill(0);
                if let Some(rest) = bytes.get(at as usize..)
                    && at < bytes.len() as u64
                {
                    let n = rest.len().min(buf.len());
                    buf[..n].copy_from_slice(&rest[..n]);
                }
                Ok(())
            }
            Value::Clusters(stream) => self.read_stream(stream, at, buf),
        }
    }

    /// [`Volume::read_value`] of a value in clusters.
    pub(crate) fn read_stream(&self, stream: &Stream, at: u64, buf: &mut [u8]) -> Result<()> {
        if stream.flags & (attr_flags::COMPRESSED | attr_flags::ENCRYPTED) != 0 {
            return Err(Error::NotSupported);
        }
        let written = stream.initialized.saturating_sub(at).min(buf.len() as u64) as usize;
        read_runs(&self.cache, &self.geo, &stream.runs, at, &mut buf[..written])?;
        buf[written..].fill(0);
        Ok(())
    }
}

/// The master file table's clusters and record count, from its own
/// record, the first, at the cluster the boot sector gives, and the
/// records its attribute list names, which the clusters found so far must
/// reach; and whether Radix can grow it: it has no attribute list, and
/// all of it was written.
#[inline(never)]
fn load_mft(cache: &BlockCache, geo: &Geometry) -> Result<(Runs, u64, bool)> {
    let rs = geo.record_size as usize;
    let mut first = zeroed(rs)?;
    cache.read(geo.cluster_offset(geo.mft), &mut first)?;
    fix_up(&mut first, b"FILE")?;
    let rec = Record::new(&first)?;
    if !rec.in_use() {
        return Err(Error::Corrupt);
    }
    let mut data = None;
    let mut list = None;
    for attr in rec.attrs() {
        let attr = attr?;
        match attr.kind() {
            kind::DATA if attr.unnamed() && attr.lowest_vcn() == 0 => {
                piece(&mut data, &attr, geo.clusters)?;
            }
            kind::ATTRIBUTE_LIST => {
                let mut value = None;
                piece(&mut value, &attr, geo.clusters)?;
                list = value;
            }
            _ => {}
        }
    }
    let Some(Value::Clusters(mut stream)) = data else { return Err(Error::Corrupt) };
    let growable = list.is_none() && stream.initialized == stream.size;
    if let Some(list) = list {
        let bytes = match &list {
            Value::Resident(bytes) => mem::copy(bytes)?,
            Value::Clusters(own) => {
                if own.size > MAX_LIST {
                    return Err(Error::Corrupt);
                }
                let mut bytes = zeroed(own.size as usize)?;
                read_runs(cache, geo, &own.runs, 0, &mut bytes)?;
                bytes
            }
        };
        let mut other = zeroed(rs)?;
        for entry in list_entries(&bytes) {
            let entry = entry?;
            if entry.kind() != kind::DATA || !entry.name().is_empty() || entry.lowest_vcn() == 0 {
                continue;
            }
            // The record of the next piece, through the pieces so far.
            let at = record_of(entry.reference()) * u64::from(geo.record_size);
            read_runs(cache, geo, &stream.runs, at, &mut other)?;
            fix_up(&mut other, b"FILE")?;
            let rec = Record::new(&other)?;
            let mut found = false;
            for attr in rec.attrs() {
                let attr = attr?;
                if attr.kind() == kind::DATA && attr.id() == entry.id() {
                    let (lowest, highest) = (attr.lowest_vcn(), attr.highest_vcn());
                    stream.runs.add(attr.runs(), lowest, highest, geo.clusters)?;
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(Error::Corrupt);
            }
        }
    }
    let records = stream.size / u64::from(geo.record_size);
    if records <= records::FIRST_USER
        || stream.runs.clusters() * u64::from(geo.cluster_size) < stream.size
    {
        return Err(Error::Corrupt);
    }
    Ok((stream.runs, records, growable))
}

/// What a lookup or listing works in: an index block, a name being looked
/// for and one being shown. Kilobytes, so it lives behind the volume's
/// lock rather than on the stack.
pub(crate) struct Scratch {
    pub(crate) block: KVec<u8>,
    pub(crate) units: [u16; NAME_MAX],
    pub(crate) text: [u8; NAME_MAX * 3],
}

/// The room on the volume, and what the volume says of itself on the
/// disk.
pub(crate) struct Space {
    pub(crate) clusters: Clusters,
    /// The master file table's `$BITMAP`, on a volume mounted to write.
    records: Option<Stream>,
    /// Where to look for a free record first.
    record_hint: u64,
    /// Whether `$Volume` on the disk says the volume is dirty.
    marked: bool,
    /// Whether the log was emptied.
    log_empty: bool,
}

/// What a volume's nodes share.
pub(crate) struct Inner {
    pub(crate) vol: Volume,
    pub(crate) read_only: bool,
    pub(crate) upcase: Upcase,
    /// The namespace lock (see the module documentation), with the room
    /// lookups and listings work in.
    pub(crate) ns: Mutex<KBox<Scratch>>,
    pub(crate) space: Mutex<Space>,
    /// The nodes in use, by record, sorted.
    nodes: Mutex<KVec<(u64, Weak<NtfsNode, Alloc>)>>,
    /// Nodes with changes in memory, kept alive until they are synced.
    dirty: Mutex<KVec<KArc<NtfsNode>>>,
    /// `$LogFile`'s data, on a volume mounted to write.
    log: Option<Stream>,
    /// The volume's flags as the mount found them.
    flags: u16,
    /// Whether the volume stays marked dirty, for Windows to check at
    /// its next start: one written although Windows hibernated with it.
    keep_dirty: AtomicBool,
}

impl Inner {
    /// The node of the file `reference` names: the one in use, or one
    /// made from its record. A reference whose sequence number is not its
    /// record's is to a file since removed, and fails with
    /// [`Error::Corrupt`].
    pub(crate) fn node_for(this: &KArc<Inner>, reference: u64) -> Result<KArc<NtfsNode>> {
        let number = record_of(reference);
        let sequence = record::sequence_of(reference);
        let mut nodes = this.nodes.lock();
        let at = nodes.binary_search_by_key(&number, |(n, _)| *n);
        if let Ok(i) = at
            && let Some(node) = nodes[i].1.upgrade()
        {
            return Ok(node);
        }
        let node = Inner::make(this, number, sequence)?;
        let weak = KArc::downgrade(&node);
        match at {
            Ok(i) => nodes[i].1 = weak,
            Err(_) => {
                if nodes.len() == nodes.capacity() {
                    // Room is made by forgetting nodes no longer in use
                    // first; the table only grows past twice the nodes in
                    // use.
                    nodes.retain(|(_, weak)| weak.strong_count() > 0);
                }
                let i = nodes.binary_search_by_key(&number, |(n, _)| *n).unwrap_or_else(|i| i);
                if mem::insert(&mut nodes, i, (number, weak)).is_err() {
                    drop(nodes);
                    drop(node);
                    return Err(Error::NoMemory);
                }
            }
        }
        Ok(node)
    }

    /// A node made from record `number`, which must have the sequence
    /// number `sequence` unless it is 0. A frame of its own, as what is
    /// read of a record is large.
    #[inline(never)]
    fn make(this: &KArc<Inner>, number: u64, sequence: u16) -> Result<KArc<NtfsNode>> {
        let loaded = this.vol.load(number)?;
        if sequence != 0 && sequence != loaded.sequence {
            return Err(Error::Corrupt);
        }
        NtfsNode::new(this.clone(), number, loaded)
    }

    /// The nodes in use.
    fn live_nodes(&self) -> Result<KVec<KArc<NtfsNode>>> {
        let nodes = self.nodes.lock();
        let mut live = mem::vec_with_capacity(nodes.len())?;
        for (_, weak) in nodes.iter() {
            if let Some(node) = weak.upgrade() {
                let _ = live.push_within_capacity(node);
            }
        }
        Ok(live)
    }

    /// Keeps `node` alive until the next sync: it has changes in memory.
    pub(crate) fn pin(&self, node: KArc<NtfsNode>) -> core::result::Result<(), KArc<NtfsNode>> {
        let mut dirty = self.dirty.lock();
        if dirty.try_reserve(1).is_err() {
            return Err(node);
        }
        let _ = dirty.push_within_capacity(node);
        Ok(())
    }

    /// How many clusters are free, as `$Bitmap` says: counted the first
    /// time.
    fn free_clusters(&self) -> Result<u64> {
        let mut space = self.space.lock();
        space.clusters.free(&self.vol.cache, &self.vol.geo)
    }

    /// Takes `want` clusters for a file's data, after `near` if it can
    /// ([`Clusters::take`]).
    pub(crate) fn take_clusters(&self, want: u64, near: Option<u64>) -> Result<KVec<(u64, u64)>> {
        let mut space = self.space.lock();
        space.clusters.take(&self.vol.cache, &self.vol.geo, want, near, false)
    }

    /// Gives back the `count` clusters from `first`; `metadata` drops them
    /// from the block cache too, an index's, whose blocks must not be
    /// written over what is put there next.
    pub(crate) fn give_clusters(&self, first: u64, count: u64, metadata: bool) -> Result<()> {
        if metadata {
            let geo = &self.vol.geo;
            let bytes = count * u64::from(geo.cluster_size);
            self.vol.cache.discard(geo.cluster_offset(first), bytes as usize);
        }
        let mut space = self.space.lock();
        space.clusters.give(&self.vol.cache, &self.vol.geo, first, count)
    }

    /// Takes a free record for a new file: the first free from where the
    /// last was taken, growing the master file table if none is.
    pub(crate) fn take_record(&self) -> Result<u64> {
        let mut space = self.space.lock();
        let space = &mut *space;
        for _ in 0..2 {
            let records = self.vol.records();
            let stream = space.records.as_ref().ok_or(Error::ReadOnly)?;
            let bits = Bits {
                cache: &self.vol.cache,
                geo: &self.vol.geo,
                runs: &stream.runs,
                len: records.min(stream.size * 8),
            };
            let hint = space.record_hint.max(records::FIRST_FREE);
            let found = match bits.find_clear(hint, records, 1)? {
                Some(found) => Some(found),
                None => bits.find_clear(records::FIRST_FREE, hint, 1)?,
            };
            if let Some((number, _)) = found {
                bits.set(number, 1, true)?;
                space.record_hint = number + 1;
                return Ok(number);
            }
            self.grow_mft(space)?;
        }
        Err(Error::NoSpace)
    }

    /// Gives back record `number`, which is no file's now.
    pub(crate) fn give_record(&self, number: u64) -> Result<()> {
        let mut space = self.space.lock();
        let space = &mut *space;
        let stream = space.records.as_ref().ok_or(Error::ReadOnly)?;
        let bits = Bits {
            cache: &self.vol.cache,
            geo: &self.vol.geo,
            runs: &stream.runs,
            len: stream.size * 8,
        };
        bits.set(number, 1, false)?;
        space.record_hint = space.record_hint.min(number);
        Ok(())
    }

    /// Grows the master file table by [`GROW_RECORDS`] records at least,
    /// a cluster's worth if that is more: with clusters after its last
    /// while they are free, else in one run, from its zone first. The new records are formatted
    /// free, its bitmap grows to have their bits, and its own record says
    /// so, all in the cache.
    #[inline(never)]
    fn grow_mft(&self, space: &mut Space) -> Result<()> {
        let geo = &self.vol.geo;
        let cache = &self.vol.cache;
        let rs = u64::from(geo.record_size);
        let cs = u64::from(geo.cluster_size);
        let (old, old_clusters, near) = {
            let mft = self.vol.mft.lock();
            let near = mft.runs.as_slice().last().map(|r| r.lcn + r.len);
            (mft.records, mft.runs.clusters(), near)
        };
        let bytes = ((old + GROW_RECORDS.max(old / 16)) * rs).next_multiple_of(cs);
        let target = bytes / rs;
        let need = (bytes / cs).saturating_sub(old_clusters);
        let got = space.clusters.take(cache, geo, need, near, true)?;
        let mut runs = Runs::new();
        {
            let mft = self.vol.mft.lock();
            for run in mft.runs.as_slice() {
                runs.append(run.lcn, run.len)?;
            }
        }
        for &(lcn, n) in &got {
            runs.append(lcn, n)?;
        }
        let stream = space.records.as_mut().ok_or(Error::ReadOnly)?;
        let bitmap_old = (stream.size, stream.initialized);
        let bitmap_bytes = target.div_ceil(64) * 8;
        let mut bitmap_got = mem::vec();
        let grown = (|| {
            let mut first = zeroed(rs as usize)?;
            self.vol.read_record(records::MFT, &mut first)?;
            // The bitmap's new bytes are zeros, on clusters of its own if
            // its own are full.
            if bitmap_bytes > stream.size {
                let have = stream.runs.clusters() * cs;
                if bitmap_bytes > have {
                    let near = stream.runs.as_slice().last().map(|r| r.lcn + r.len);
                    let more = (bitmap_bytes - have).div_ceil(cs);
                    bitmap_got = space.clusters.take(cache, geo, more, near, true)?;
                    for &(lcn, n) in &bitmap_got {
                        stream.runs.append(lcn, n)?;
                    }
                }
                let zeros = zeroed((bitmap_bytes - stream.size) as usize)?;
                write_runs(cache, geo, &stream.runs, stream.size, &zeros, false)?;
                stream.size = bitmap_bytes;
                stream.initialized = bitmap_bytes;
            }
            let clusters = runs.clusters();
            let sizes = Sizes {
                allocated: clusters * cs,
                size: target * rs,
                initialized: target * rs,
                ..Sizes::default()
            };
            replace_stream(&mut first, kind::DATA, &runs, clusters, sizes)?;
            let bits = stream.runs.clusters();
            let sizes = Sizes {
                allocated: bits * cs,
                size: stream.size,
                initialized: stream.initialized,
                ..Sizes::default()
            };
            replace_stream(&mut first, kind::BITMAP, &stream.runs, bits, sizes)?;
            Ok(first)
        })();
        let first: KVec<u8> = match grown {
            Ok(first) => first,
            Err(err) => {
                for &(lcn, n) in got.iter().chain(bitmap_got.iter()) {
                    let _ = space.clusters.give(cache, geo, lcn, n);
                }
                let _ = stream
                    .runs
                    .truncate(stream.runs.clusters() - bitmap_got.iter().map(|r| r.1).sum::<u64>());
                (stream.size, stream.initialized) = bitmap_old;
                return Err(err);
            }
        };
        // The new records, free, then the table's own record.
        let mut record = zeroed(rs as usize)?;
        for number in old..target {
            edit::format_record(&mut record, number, 1, 0);
            fix_down(&mut record)?;
            write_runs(cache, geo, &runs, number * rs, &record, false)?;
        }
        {
            let mut mft = self.vol.mft.lock();
            mft.runs = runs;
            mft.records = target;
        }
        self.vol.write_record(records::MFT, &first, false)
    }

    /// Notes a change about to be made: the first since the volume was
    /// last clean empties the log, if it was not emptied yet, and marks the
    /// volume dirty on the disk, at once.
    pub(crate) fn changing(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let mut space = self.space.lock();
        if space.marked {
            return Ok(());
        }
        if !space.log_empty {
            self.empty_log()?;
            space.log_empty = true;
        }
        self.write_flags(self.flags | volume_flags::DIRTY)?;
        space.marked = true;
        Ok(())
    }

    /// Fills `$LogFile` with 0xff, as ntfs-3g does: Windows then starts a
    /// new log rather than replay one that does not know what Radix
    /// changed. Written straight to the disk, and flushed.
    #[inline(never)]
    fn empty_log(&self) -> Result<()> {
        let Some(log) = &self.log else { return Ok(()) };
        let geo = &self.vol.geo;
        let cs = u64::from(geo.cluster_size);
        let device = self.vol.cache.device();
        let mut page = CachePage::new()?;
        page.bytes_mut().fill(0xff);
        let max = device.info().max_segments.clamp(1, 64);
        let mut segments = mem::vec_with_capacity(max)?;
        for run in log.runs.as_slice() {
            if run.lcn == HOLE {
                continue;
            }
            let start = geo.cluster_offset(run.lcn);
            let end = start + run.len * cs;
            self.vol.cache.discard(start, (end - start) as usize);
            let mut at = start;
            while at < end {
                segments.clear();
                let mut total = 0u64;
                while segments.len() < max && at + total < end {
                    let len = (end - at - total).min(PAGE_SIZE as u64) as usize;
                    let _ = segments.push_within_capacity(Segment { page: &page, offset: 0, len });
                    total += len as u64;
                }
                device.write(at / SECTOR, &segments)?;
                at += total;
            }
        }
        Ok(device.flush()?)
    }

    /// Writes the volume flags `flags` to `$Volume`, at once. Marked
    /// dirty, `$MFT`'s copy goes first, and marked clean, `$MFTMirr`'s:
    /// a power cut between the two leaves dirty the copy that is read.
    fn write_flags(&self, flags: u16) -> Result<()> {
        let mut buf = zeroed(self.vol.geo.record_size as usize)?;
        self.vol.read_record(records::VOLUME, &mut buf)?;
        let at = edit::find(&buf, kind::VOLUME_INFORMATION, &[]).ok_or(Error::Corrupt)?;
        let value = edit::value_mut(&mut buf, at);
        if value.len() < 12 {
            return Err(Error::Corrupt);
        }
        put_u16(value, 10, flags);
        let clean = flags & volume_flags::DIRTY == 0;
        self.vol.write_record_in(records::VOLUME, &buf, true, clean)
    }

    /// Writes every change to the disk: the nodes' data and records, then
    /// the bitmaps, directories and records in the cache, then flushes the
    /// device; then clears the dirty mark, unless something changed
    /// meanwhile.
    pub(crate) fn sync_all(&self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
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
        drop(pinned);
        self.vol.cache.sync()?;
        if let Some(err) = failed {
            return Err(err);
        }
        // Every change goes into the block cache under `ns` or `space`,
        // and a node is pinned before it takes clusters, so with both held
        // a clean cache and no pinned node mean the disk has it all.
        let _ns = self.ns.lock();
        let mut space = self.space.lock();
        if space.marked
            && !self.keep_dirty.load(Ordering::Relaxed)
            && !self.vol.cache.is_dirty()
            && self.dirty.lock().is_empty()
        {
            self.write_flags(self.flags & !volume_flags::DIRTY)?;
            space.marked = false;
        }
        Ok(())
    }
}

/// Puts a new copy of the unnamed non-resident attribute of type `kind`
/// in the record `buf`, with the runs `runs` (`clusters` of them) and the
/// sizes `sizes`, its number and flags kept. Fails with
/// [`Error::NoSpace`] if the record has no room, leaving it as it was.
pub(crate) fn replace_stream(
    buf: &mut [u8],
    kind: u32,
    runs: &Runs,
    clusters: u64,
    sizes: Sizes,
) -> Result<()> {
    replace_named(buf, kind, &[], runs, clusters, sizes)
}

/// [`replace_stream`] for the attribute named `name` (UTF-16 bytes); one
/// not there yet is added, with the record's next number.
pub(crate) fn replace_named(
    buf: &mut [u8],
    kind: u32,
    name: &[u8],
    runs: &Runs,
    clusters: u64,
    sizes: Sizes,
) -> Result<()> {
    let packed = runs.pack(0, clusters)?;
    let old = edit::find(buf, kind, name);
    let (instance, flags, old_len) = match old {
        Some(at) => (u16_at(buf, at + 0x0e), u16_at(buf, at + 0x0c), u32_at(buf, at + 4) as usize),
        None => (u16_at(buf, 0x28), 0, 0),
    };
    let either = attr_flags::COMPRESSED | attr_flags::SPARSE;
    let flags = flags & !either | sizes.packed & either;
    let highest = clusters.wrapping_sub(1);
    let attr = edit::non_resident(kind, name, instance, flags, 0, highest, &packed, sizes)?;
    if edit::room(buf) + old_len < attr.len() {
        return Err(Error::NoSpace);
    }
    match old {
        Some(at) => edit::remove(buf, at),
        None => {
            edit::next_instance(buf);
        }
    }
    edit::insert(buf, &attr)?;
    Ok(())
}

/// A mounted NTFS volume.
pub struct NtfsFs {
    inner: KArc<Inner>,
    root: KArc<NtfsNode>,
    /// Its label, in UTF-16.
    label: KVec<u16>,
    /// Its NTFS version, major and minor: 3.1 for Windows XP and later.
    version: (u8, u8),
    /// Its flags ([`volume_flags`]).
    flags: u16,
    /// Whether Windows hibernated with it mounted.
    hibernated: bool,
    /// Whether the mount threw Windows' hibernation away, to write it.
    discarded: bool,
    /// Why a writable mount of it would be refused, mounted read-only.
    kept: Option<&'static str>,
}

/// How an NTFS is mounted ([`NtfsFs::mount_with`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Refusing every change.
    pub read_only: bool,
    /// Written although Windows hibernated with it, or shut down with
    /// Fast Startup, as its owner asked, as ntfs-3g's `remove_hiberfile`
    /// does: the hibernation is thrown away, and with it what Windows
    /// held in memory, its programs and what it had not written of the
    /// volume. Windows then starts afresh, and checks the volume, which
    /// stays marked dirty (ADR 0238). It changes nothing of a volume
    /// Windows did not hibernate with.
    pub discard_hibernation: bool,
}

impl NtfsFs {
    /// Mounts the NTFS volume on `device`; `read_only` refuses every
    /// change. A writable mount of a volume Radix must not change (one
    /// Windows hibernated with, left dirty or with a log to replay, of an
    /// NTFS before 3.0, or whose master file table Radix cannot grow)
    /// fails with [`Error::NotSupported`], as a Btrfs that Radix cannot
    /// write does (ADR 0130), and Radix's mount makes it read-only (ADRs
    /// 0236 and 0237).
    pub fn mount(device: DeviceRef, read_only: bool) -> Result<KArc<NtfsFs>> {
        NtfsFs::mount_with(device, Options { read_only, ..Options::default() })
    }

    /// [`NtfsFs::mount`], as `options` say.
    pub fn mount_with(device: DeviceRef, options: Options) -> Result<KArc<NtfsFs>> {
        let read_only = options.read_only;
        let forced = options.discard_hibernation && !read_only;
        let Opened { inner, about, kept, unclean } = open(device, read_only, forced)?;
        let About { label, version, flags } = about;
        let root = Inner::node_for(&inner, records::ROOT)?;
        if root.kind != Kind::Directory {
            return Err(Error::Corrupt);
        }
        let hibernated = hibernated(&root);
        // Only a hibernation is written past, and what it left.
        if hibernated && !read_only && !forced || forced && !hibernated && unclean {
            return Err(Error::NotSupported);
        }
        let discarded = forced && hibernated;
        if discarded {
            inner.keep_dirty.store(true, Ordering::Relaxed);
            discard_hibernation(&root)?;
            inner.sync_all()?;
        }
        let kept = if hibernated {
            Some(
                "Windows hibernated with it, or shut down with Fast Startup; shut Windows down \
                 fully (Shift with Shut down) to write it, or remount it with \
                 --discard-hibernation to write it anyway, losing what Windows left open",
            )
        } else {
            kept
        };
        Ok(mem::arc(NtfsFs { inner, root, label, version, flags, hibernated, discarded, kept })?)
    }

    /// Where the volume's parts are.
    pub fn geometry(&self) -> &Geometry {
        &self.inner.vol.geo
    }

    /// Its NTFS version, major and minor.
    pub fn version(&self) -> (u8, u8) {
        self.version
    }

    /// Whether Windows left it marked as maybe inconsistent.
    pub fn dirty(&self) -> bool {
        self.flags & volume_flags::DIRTY != 0
    }

    /// Whether Windows hibernated with it mounted, or shut down with Fast
    /// Startup, which hibernates its kernel: what it holds in memory of
    /// the volume would be written back when it resumes, over whatever
    /// another system wrote.
    pub fn hibernated(&self) -> bool {
        self.hibernated
    }

    /// Whether the mount threw Windows' hibernation away, to write it
    /// ([`Options::discard_hibernation`]).
    pub fn discarded_hibernation(&self) -> bool {
        self.discarded
    }

    /// Why a writable mount of it would be refused, if it would, for a
    /// volume mounted read-only: what the kernel tells its owner.
    pub fn kept_from_writing(&self) -> Option<&'static str> {
        self.kept
    }
}

impl Drop for NtfsFs {
    fn drop(&mut self) {
        let _ = self.inner.sync_all();
        // What could not be written is lost, but the nodes must not keep
        // each other alive.
        let pinned = core::mem::replace(&mut *self.inner.dirty.lock(), mem::vec());
        drop(pinned);
    }
}

/// What `$Volume` says of a volume.
struct About {
    label: KVec<u16>,
    version: (u8, u8),
    flags: u16,
}

/// What a writable mount needs besides what any mount reads: the
/// clusters of `$MFTMirr` and `$LogFile`, the master file table's bitmap
/// and `$Bitmap`'s clusters. Fails with [`Error::NotSupported`] for a
/// volume Radix must not write ([`NtfsFs::mount`]).
struct Writable {
    mirror: Mirror,
    log: Stream,
    records: Stream,
    /// Whether Windows left it dirty, or changes in its log, which a mount
    /// that discards its hibernation writes over.
    unclean: bool,
}

/// The volume on `device`, but for its root: its geometry, master file
/// table and up-case table, and what `$Volume` says of it ([`NtfsFs::mount`]).
#[inline(never)]
fn open(device: DeviceRef, read_only: bool, forced: bool) -> Result<Opened> {
    let info = device.info();
    let cache = BlockCache::new(device);
    let geo = read_geometry(&cache, info.sectors * radix_block::SECTOR)?;
    // Its sectors are read alone, which a disk with larger ones cannot do:
    // Linux refuses such a volume too.
    if geo.sector_size < info.logical_block {
        return Err(Error::NotSupported);
    }
    let (mft, records, growable) = load_mft(&cache, &geo)?;
    let vol = Volume { geo, cache, mft: Mutex::new(Mft { runs: mft, records }), mirror: None };
    let upcase = load_upcase(&vol)?;
    let about = about(&vol)?;
    let bitmap = load_bitmap(&vol)?;
    let (writable, kept) = write_state(&vol, &about, growable, read_only, forced)?;
    let unclean = writable.as_ref().is_some_and(|w| w.unclean);
    let inner = assemble(vol, read_only, upcase, bitmap, writable, about.flags)?;
    Ok(Opened { inner, about, kept, unclean })
}

/// What [`open`] found.
struct Opened {
    inner: KArc<Inner>,
    about: About,
    /// Why it could not be written, mounted read-only.
    kept: Option<&'static str>,
    /// Whether it was written past a dirty flag or a log to replay.
    unclean: bool,
}

/// What a mount of `vol` writes with ([`writable`]; `forced` past a
/// dirty flag and a log to replay), if it is not `read_only`; mounted
/// read-only, why it could not be written instead.
#[inline(never)]
fn write_state(
    vol: &Volume,
    about: &About,
    growable: bool,
    read_only: bool,
    forced: bool,
) -> Result<(Option<Writable>, Option<&'static str>)> {
    if !read_only {
        return Ok((Some(writable(vol, about, growable, forced)?), None));
    }
    let kept = if about.version.0 != 3 {
        Some("it is of an NTFS before 3.0, which Dukovo only reads")
    } else if about.flags & volume_flags::DIRTY != 0 {
        Some("it is marked to be checked; let Windows check it to write it")
    } else if !growable {
        Some("Dukovo cannot grow its file table yet")
    } else if writable(vol, about, growable, false).is_err() {
        Some(
            "Windows left changes in its journal to replay; start Windows and shut it down \
             fully to write it",
        )
    } else {
        None
    };
    Ok((None, kept))
}

/// The mounted volume, from what [`open`] read of `vol`; a frame of its
/// own, as [`Inner`] is large.
#[inline(never)]
fn assemble(
    mut vol: Volume,
    read_only: bool,
    upcase: Upcase,
    bitmap: Runs,
    writable: Option<Writable>,
    flags: u16,
) -> Result<KArc<Inner>> {
    let geo = vol.geo;
    let (log, records) = match writable {
        Some(Writable { mirror, log, records, .. }) => {
            vol.mirror = Some(mirror);
            (Some(log), Some(records))
        }
        None => (None, None),
    };
    let zone_end = (geo.mft + geo.clusters / 8).min(geo.clusters);
    let clusters = Clusters { bitmap, free: None, hint: zone_end, zone: (geo.mft, zone_end) };
    let space = Space { clusters, records, record_hint: 0, marked: false, log_empty: false };
    let scratch = scratch(geo.index_size)?;
    Ok(mem::arc(Inner {
        vol,
        read_only,
        upcase,
        ns: Mutex::new(scratch),
        space: Mutex::new(space),
        nodes: Mutex::new(mem::vec()),
        dirty: Mutex::new(mem::vec()),
        log,
        flags,
        keep_dirty: AtomicBool::new(false),
    })?)
}

/// What `$Volume` says of `vol`: its label, version and flags. Volumes of
/// NTFS 1.x to 3.x are read.
#[inline(never)]
fn about(vol: &Volume) -> Result<About> {
    let volume = vol.load(records::VOLUME)?;
    let (major, minor, flags) = volume.version.ok_or(Error::Corrupt)?;
    if !(1..=3).contains(&major) {
        return Err(Error::NotSupported);
    }
    let mut label = mem::vec();
    for unit in volume.label.as_deref().unwrap_or_default().as_chunks::<2>().0.iter().take(128) {
        mem::push(&mut label, u16::from_le_bytes(*unit))?;
    }
    Ok(About { label, version: (major, minor), flags })
}

/// `$Bitmap`'s clusters.
#[inline(never)]
fn load_bitmap(vol: &Volume) -> Result<Runs> {
    match vol.load(records::BITMAP)?.data {
        Some(Value::Clusters(stream)) if stream.size >= vol.geo.clusters.div_ceil(8) => {
            Ok(stream.runs)
        }
        _ => Err(Error::Corrupt),
    }
}

/// What a writable mount of `vol` needs ([`Writable`]); `growable` says
/// whether Radix can grow its master file table.
#[inline(never)]
fn writable(vol: &Volume, about: &About, growable: bool, forced: bool) -> Result<Writable> {
    let dirty = about.flags & volume_flags::DIRTY != 0;
    if about.version.0 != 3 || dirty && !forced || !growable {
        return Err(Error::NotSupported);
    }
    let rs = u64::from(vol.geo.record_size);
    let cs = u64::from(vol.geo.cluster_size);
    let mirror = plain_stream(vol, records::MFT_MIRROR, false)?;
    // Windows keeps four records in the mirror, or a cluster's worth.
    let count = if cs <= 4 * rs { 4 } else { cs / rs };
    if mirror.size < count * rs {
        return Err(Error::NotSupported);
    }
    let log = plain_stream(vol, records::LOG, false)?;
    let clean = log_clean(vol, &log)?;
    if !clean && !forced {
        return Err(Error::NotSupported);
    }
    let records = plain_stream(vol, records::MFT, true)?;
    if records.size * 8 < vol.records() {
        return Err(Error::Corrupt);
    }
    let unclean = dirty || !clean;
    Ok(Writable { mirror: Mirror { runs: mirror.runs, records: count }, log, records, unclean })
}

/// Record `number`'s data, or its own `$BITMAP` if `bitmap`, if it is a
/// plain stream of clusters: not compressed, sparse or encrypted, and
/// written to its end. A frame of its own, as a [`Loaded`] is large.
#[inline(never)]
fn plain_stream(vol: &Volume, number: u64, bitmap: bool) -> Result<Stream> {
    let loaded = vol.load(number)?;
    match if bitmap { loaded.own_bitmap } else { loaded.data } {
        Some(Value::Clusters(stream)) if stream.flags == 0 && stream.initialized == stream.size => {
            Ok(stream)
        }
        _ => Err(Error::NotSupported),
    }
}

/// Bits of a log's restart area's flags.
const RESTART_CLEAN: u16 = 0x0002;

/// No client of the log: its list of clients in use is empty.
const NO_CLIENT: u16 = 0xffff;

/// Whether the log `log` of `vol` needs nothing replayed: it is empty (all
/// 0xff, as ntfs-3g and Radix leave it), or each restart page that is one
/// says the volume was left clean or has no client using the log, as
/// ntfs-3g reads it.
#[inline(never)]
fn log_clean(vol: &Volume, log: &Stream) -> Result<bool> {
    const PAGE: usize = 4096;
    let mut page = zeroed(PAGE)?;
    vol.read_stream(log, 0, &mut page)?;
    if page.iter().all(|&b| b == 0xff) {
        return Ok(true);
    }
    // The restart pages are the first two of the system's page size.
    let size = u32_at(&page, 0x10) as usize;
    if &page[..4] != b"RSTR" || !(PAGE..=(64 << 10)).contains(&size) || !size.is_power_of_two() {
        return Ok(false);
    }
    let mut pages = 0;
    for at in [0, size] {
        let mut restart = zeroed(size)?;
        vol.read_stream(log, at as u64, &mut restart)?;
        if fix_up(&mut restart, b"RSTR").is_err() {
            continue;
        }
        let area = usize::from(u16_at(&restart, 0x18));
        let Some(ra) = restart.get(area..area + 0x10) else { return Ok(false) };
        let in_use = u16_at(ra, 0x0c);
        let flags = u16_at(ra, 0x0e);
        if in_use != NO_CLIENT && flags & RESTART_CLEAN == 0 {
            return Ok(false);
        }
        pages += 1;
    }
    Ok(pages > 0)
}

/// The scratch space of lookups and listings, for index blocks of
/// `index_size` bytes.
#[inline(never)]
fn scratch(index_size: u32) -> Result<KBox<Scratch>> {
    Ok(mem::boxed(Scratch {
        block: zeroed(index_size as usize)?,
        units: [0; NAME_MAX],
        text: [0; NAME_MAX * 3],
    })?)
}

/// The geometry the boot sector gives, on a device of `bytes` bytes. Each
/// step of a mount that reads the disk has a frame of its own: a mount
/// runs in a system call, on a 16 KiB stack.
#[inline(never)]
fn read_geometry(cache: &BlockCache, bytes: u64) -> Result<Geometry> {
    let mut boot = [0u8; 512];
    cache.read(0, &mut boot)?;
    Geometry::parse(&boot, bytes)
}

/// The volume's up-case table, `$UpCase`'s data.
#[inline(never)]
fn load_upcase(vol: &Volume) -> Result<Upcase> {
    let loaded = vol.load(records::UPCASE)?;
    let data = loaded.data.ok_or(Error::Corrupt)?;
    Upcase::load(data.size().min(upcase::BYTES), |at, buf| vol.read_value(&data, at, buf))
}

/// Throws away the hibernation in the root's `hiberfil.sys`: its first
/// page becomes zeros, which Windows takes as no hibernation, as it does
/// once it has resumed.
#[inline(never)]
fn discard_hibernation(root: &NtfsNode) -> Result<()> {
    let file = root.lookup("hiberfil.sys")?;
    let len = file.stat().size.min(PAGE_SIZE as u64) as usize;
    let zeros = zeroed(len)?;
    file.write(0, &radix_vfs::Slice(&zeros))?;
    Ok(())
}

/// Whether the root's `hiberfil.sys` starts as a hibernated system's does
/// (`hibr`, or `HIBR` since Windows 8): Windows clears it when it resumes.
#[inline(never)]
fn hibernated(root: &NtfsNode) -> bool {
    let Ok(file) = root.lookup("hiberfil.sys") else { return false };
    let mut start = [0u8; 4];
    let mut out = radix_vfs::SliceMut(&mut start);
    matches!(file.read(0, &mut out), Ok(4)) && (&start == b"hibr" || &start == b"HIBR")
}

impl FileSystem for NtfsFs {
    fn kind(&self) -> FsKind {
        FsKind::Ntfs
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        let geo = &self.inner.vol.geo;
        FsInfo {
            block_size: geo.cluster_size,
            blocks: geo.clusters,
            free: self.inner.free_clusters().unwrap_or(0),
            ..FsInfo::default()
        }
    }

    fn read_only(&self) -> bool {
        self.inner.read_only
    }

    fn label(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        for c in char::decode_utf16(self.label.iter().copied()) {
            out.write_char(c.unwrap_or(char::REPLACEMENT_CHARACTER))?;
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.inner.sync_all()
    }

    fn is_dirty(&self) -> bool {
        !self.inner.dirty.lock().is_empty() || self.inner.vol.cache.is_dirty()
    }

    fn shrink(&self, max: usize) -> usize {
        let mut dropped = self.inner.vol.cache.shrink(max);
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
}
