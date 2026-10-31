// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! An NTFS volume: its master file table, the files Radix reads at mount,
//! and the table of the nodes in use.
//!
//! A mount reads the boot sector, then the master file table's own
//! record, the first, which says where the rest of the table is; then
//! `$UpCase`, the up-case table, and `$Volume`, the label, version and
//! flags. Records are read through a cache of the device's sectors, as
//! the other filesystems' metadata is; file data goes through each file's
//! page cache ([`crate::node`]).

use alloc::sync::Weak;
use core::fmt;

use radix_block::DeviceRef;
use radix_block::cache::BlockCache;
use radix_sync::Mutex;
use radix_vfs::mem::{self, Alloc, KArc, KBox, KVec};
use radix_vfs::{Error, FileSystem, FsInfo, FsKind, Kind, Node, NodeRef, Result};

use crate::boot::Geometry;
use crate::index::{FileName, NAME_MAX, namespace};
use crate::node::NtfsNode;
use crate::record::{
    self, Attr, Record, attr_flags, fix_up, kind, list_entries, name_is, record_of, u32_at, u64_at,
};
use crate::runs::{HOLE, Runs};
use crate::upcase::{self, Upcase};

/// The records of the system's files.
pub mod records {
    /// The master file table.
    pub const MFT: u64 = 0;
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
    /// Its directory index: `$I30`'s root, blocks and bitmap.
    pub(crate) root: Option<KVec<u8>>,
    pub(crate) blocks: Option<Value>,
    pub(crate) bitmap: Option<Value>,
    pub(crate) reparse: Option<Value>,
    /// `$Volume`'s label and its version and flags.
    pub(crate) label: Option<KVec<u8>>,
    pub(crate) version: Option<(u8, u8, u16)>,
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
        kind::INDEX_ROOT if named("$I30") => {
            if attr.non_resident() || loaded.root.is_some() {
                return Err(Error::Corrupt);
            }
            loaded.root = Some(mem::copy(attr.value())?);
        }
        kind::INDEX_ALLOCATION if named("$I30") => piece(&mut loaded.blocks, attr, clusters)?,
        kind::BITMAP if named("$I30") => piece(&mut loaded.bitmap, attr, clusters)?,
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

/// What reads the volume's records and attributes.
pub(crate) struct Volume {
    pub(crate) geo: Geometry,
    pub(crate) cache: BlockCache,
    /// The master file table's clusters.
    mft: Runs,
    /// How many records it has.
    records: u64,
}

impl Volume {
    /// Reads record `number` into `buf`, its update sequence put back.
    pub(crate) fn read_record(&self, number: u64, buf: &mut [u8]) -> Result<()> {
        if number >= self.records {
            return Err(Error::Corrupt);
        }
        let rs = u64::from(self.geo.record_size);
        read_runs(&self.cache, &self.geo, &self.mft, number * rs, buf)?;
        fix_up(buf, b"FILE")
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
            Value::Clusters(stream) => {
                if stream.flags & (attr_flags::COMPRESSED | attr_flags::ENCRYPTED) != 0 {
                    return Err(Error::NotSupported);
                }
                let written = stream.initialized.saturating_sub(at).min(buf.len() as u64) as usize;
                read_runs(&self.cache, &self.geo, &stream.runs, at, &mut buf[..written])?;
                buf[written..].fill(0);
                Ok(())
            }
        }
    }
}

/// Reads `buf.len()` bytes from byte `at` of the attribute whose clusters
/// `runs` are, through `cache`; holes read as zeros.
fn read_runs(
    cache: &BlockCache,
    geo: &Geometry,
    runs: &Runs,
    at: u64,
    buf: &mut [u8],
) -> Result<()> {
    let cs = u64::from(geo.cluster_size);
    let mut done = 0;
    while done < buf.len() {
        let pos = at + done as u64;
        let (lcn, n) = runs.map(pos / cs).ok_or(Error::Corrupt)?;
        let within = pos % cs;
        let len = (n * cs - within).min((buf.len() - done) as u64) as usize;
        let piece = &mut buf[done..done + len];
        if lcn == HOLE {
            piece.fill(0);
        } else {
            cache.read(geo.cluster_offset(lcn) + within, piece)?;
        }
        done += len;
    }
    Ok(())
}

/// The master file table's clusters and record count, from its own
/// record, the first, at the cluster the boot sector gives, and the
/// records its attribute list names, which the clusters found so far must
/// reach.
#[inline(never)]
fn load_mft(cache: &BlockCache, geo: &Geometry) -> Result<(Runs, u64)> {
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
    Ok((stream.runs, records))
}

/// What a lookup or listing works in: an index block, a name being looked
/// for and one being shown. Kilobytes, so it lives behind the volume's
/// lock rather than on the stack.
pub(crate) struct Scratch {
    pub(crate) block: KVec<u8>,
    pub(crate) units: [u16; NAME_MAX],
    pub(crate) text: [u8; NAME_MAX * 3],
}

/// What a volume's nodes share.
pub(crate) struct Inner {
    pub(crate) vol: Volume,
    pub(crate) read_only: bool,
    pub(crate) upcase: Upcase,
    /// The room lookups and listings work in, under its lock.
    pub(crate) scratch: Mutex<KBox<Scratch>>,
    /// The nodes in use, by record, sorted.
    nodes: Mutex<KVec<(u64, Weak<NtfsNode, Alloc>)>>,
    /// The clusters free, counted when first asked for.
    free: Mutex<Option<u64>>,
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

    /// How many clusters are free, as `$Bitmap` says: counted the first
    /// time, a sector's worth of bits at a time.
    fn free_clusters(&self) -> Result<u64> {
        if let Some(free) = *self.free.lock() {
            return Ok(free);
        }
        let loaded = self.vol.load(records::BITMAP)?;
        let bitmap = loaded.data.ok_or(Error::Corrupt)?;
        let clusters = self.vol.geo.clusters;
        let mut buf = [0u8; 512];
        let mut used = 0;
        let mut at = 0;
        while at < clusters.div_ceil(8) {
            self.vol.read_value(&bitmap, at, &mut buf)?;
            for (i, byte) in buf.iter().enumerate() {
                let first = (at + i as u64) * 8;
                if first >= clusters {
                    break;
                }
                let bits = (clusters - first).min(8) as u32;
                let mask = if bits == 8 { 0xff } else { (1u8 << bits) - 1 };
                used += u64::from((byte & mask).count_ones());
            }
            at += buf.len() as u64;
        }
        let free = clusters - used.min(clusters);
        *self.free.lock() = Some(free);
        Ok(free)
    }
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
}

impl NtfsFs {
    /// Mounts the NTFS volume on `device`, read-only: Radix does not write
    /// NTFS yet, so a writable mount fails with [`Error::NotSupported`],
    /// as a Btrfs that Radix cannot write does (ADR 0130), and Radix's
    /// mount makes it read-only (ADR 0236).
    pub fn mount(device: DeviceRef, read_only: bool) -> Result<KArc<NtfsFs>> {
        let (inner, About { label, version, flags }) = open(device, read_only)?;
        let root = Inner::node_for(&inner, records::ROOT)?;
        if root.kind != Kind::Directory {
            return Err(Error::Corrupt);
        }
        let hibernated = hibernated(&root);
        Ok(mem::arc(NtfsFs { inner, root, label, version, flags, hibernated })?)
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
}

/// What `$Volume` says of a volume.
struct About {
    label: KVec<u16>,
    version: (u8, u8),
    flags: u16,
}

/// The volume on `device`, but for its root: its geometry, master file
/// table and up-case table, and what `$Volume` says of it ([`NtfsFs::mount`]).
#[inline(never)]
fn open(device: DeviceRef, read_only: bool) -> Result<(KArc<Inner>, About)> {
    let info = device.info();
    let cache = BlockCache::new(device);
    let geo = read_geometry(&cache, info.sectors * radix_block::SECTOR)?;
    // Its sectors are read alone, which a disk with larger ones cannot do:
    // Linux refuses such a volume too.
    if geo.sector_size < info.logical_block || !read_only {
        return Err(Error::NotSupported);
    }
    let (mft, records) = load_mft(&cache, &geo)?;
    let vol = Volume { geo, cache, mft, records };
    let upcase = load_upcase(&vol)?;
    let about = about(&vol)?;
    let scratch = scratch(geo.index_size)?;
    let inner = mem::arc(Inner {
        vol,
        read_only: true,
        upcase,
        scratch: Mutex::new(scratch),
        nodes: Mutex::new(mem::vec()),
        free: Mutex::new(None),
    })?;
    Ok((inner, about))
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
        Ok(())
    }

    fn is_dirty(&self) -> bool {
        false
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
