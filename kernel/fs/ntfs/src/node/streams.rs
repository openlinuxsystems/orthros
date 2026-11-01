// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Alternate data streams as extended attributes (ADR 0241).
//!
//! A file's named `$DATA` attributes, its alternate data streams, are its
//! `user.` extended attributes, as ntfs-3g's `streams_interface=xattr`
//! shows them: `user.NAME` is the stream `NAME`, which Windows opens as
//! `file:NAME`. Windows keeps where a download came from there
//! (`Zone.Identifier`), and apps their own data.
//!
//! - **Read** on any file or directory, wherever its attribute list puts
//!   the stream. A stream of more than 64 KiB, Linux's largest value,
//!   fails with [`Error::OutOfRange`]; a compressed or encrypted one with
//!   [`Error::NotSupported`].
//! - **Written** in the file's base record when it has room, as Windows
//!   keeps a small stream, and in clusters of its own when it has not.
//!   When the record has room for neither, or for a new name, the file's
//!   own data leaves it, then its other streams do, the largest first, as
//!   Windows makes room. The room the file's own data is still to take in
//!   its record is kept for it. A file whose attributes spread over its
//!   records (as Windows moves the names of a file of many hard links) is
//!   not changed: [`Error::NotSupported`].
//! - **Sorted** among the record's attributes, and in its attribute list,
//!   by their names' upper cases, as Windows and ntfs-3g look them up
//!   ([`crate::edit::collate`]).
//! - **Names** are UTF-16 and take no `:`, `/`, `\` or NUL, as Windows
//!   names streams: another fails with [`Error::BadName`]. A stream whose
//!   name is not valid UTF-16, or too long as a name of an attribute, is
//!   not listed.
//! - **Other namespaces** (`trusted.`, `security.`, POSIX access lists)
//!   are not kept on NTFS: [`Error::NotSupported`], as on ntfs-3g without
//!   its `acl` option.
//!
//! A removed file's streams' clusters go with its record.

use radix_vfs::attributes::{NAME_MAX, VALUE_MAX};
use radix_vfs::cache::FileCache;
use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result, Setting};

use super::{Data, NtfsNode, now_coarse, write_bytes};
use crate::edit;
use crate::fs::{Stream, piece};
use crate::record::{Record, kind, u16_at, u32_at};
use crate::runs::{HOLE, Runs};

/// The namespace of the attributes that are streams.
const USER: &[u8] = b"user.";

/// The room a stream's runs take in its attribute, at most, that its
/// record makes for it in clusters: a few runs'.
const RUNS_ROOM: usize = 24;

/// The bytes of a stream in its record that moving it to clusters saves
/// room for: more than its attribute grows by, `0x28` bytes of sizes and
/// its runs.
const OUT_MIN: usize = 0x28 + RUNS_ROOM;

/// The UTF-16 bytes of the stream's name that the attribute `name`
/// stands for: [`Error::NotSupported`] for a name of another namespace,
/// and [`Error::BadName`] for one Windows takes for no stream.
fn stream_name(name: &[u8]) -> Result<KVec<u8>> {
    let rest = name.strip_prefix(USER).ok_or(Error::NotSupported)?;
    let text = core::str::from_utf8(rest).map_err(|_| Error::BadName)?;
    if text.is_empty() || text.contains([':', '/', '\\', '\0']) {
        return Err(Error::BadName);
    }
    let mut units = mem::vec_with_capacity(text.len() * 2)?;
    for unit in text.encode_utf16() {
        mem::extend(&mut units, &unit.to_le_bytes())?;
    }
    Ok(units)
}

/// The attribute's name for the stream named `units` (UTF-16 bytes) in
/// `out`, and its length; none for a name that is not valid UTF-16, or
/// too long for an attribute's.
fn listed_name(units: &[u8], out: &mut [u8; NAME_MAX]) -> Option<usize> {
    out[..USER.len()].copy_from_slice(USER);
    let mut len = USER.len();
    let units = units.as_chunks::<2>().0.iter().map(|u| u16::from_le_bytes(*u));
    for c in char::decode_utf16(units) {
        let c = c.ok()?;
        if c == '\0' {
            return None;
        }
        let n = c.len_utf8();
        if len + n > NAME_MAX {
            return None;
        }
        c.encode_utf8(&mut out[len..len + n]);
        len += n;
    }
    Some(len)
}

impl NtfsNode {
    /// The value of its attribute `name`: the bytes of the stream it
    /// stands for.
    pub(super) fn get_stream(&self, name: &[u8]) -> Result<KVec<u8>> {
        let units = stream_name(name)?;
        let clusters = self.fs.vol.geo.clusters;
        let mut value = None;
        {
            let _ns = self.fs.ns.lock();
            self.fs.vol.each_attr(self.record, &mut |attr| {
                if attr.kind() == kind::DATA && attr.name() == &units[..] {
                    piece(&mut value, attr, clusters)?;
                }
                Ok(())
            })?;
        }
        let value = value.ok_or(Error::NoAttribute)?;
        if value.size() > VALUE_MAX as u64 {
            return Err(Error::OutOfRange);
        }
        self.fs.vol.read_whole(&value, VALUE_MAX as u64)
    }

    /// The names of its attributes: one for each of its streams.
    pub(super) fn list_streams(&self) -> Result<KVec<u8>> {
        let mut names = mem::vec();
        let mut name = [0u8; NAME_MAX];
        let _ns = self.fs.ns.lock();
        self.fs.vol.each_attr(self.record, &mut |attr| {
            // A stream in clusters is listed at its first piece.
            if attr.kind() != kind::DATA
                || attr.unnamed()
                || (attr.non_resident() && attr.lowest_vcn() != 0)
            {
                return Ok(());
            }
            if let Some(len) = listed_name(attr.name(), &mut name) {
                mem::extend(&mut names, &name[..len])?;
                mem::push(&mut names, 0)?;
            }
            Ok(())
        })?;
        Ok(names)
    }

    /// Sets its attribute `name` to `value`, as `how` says: the stream
    /// it stands for holds `value`, made if it was not there.
    pub(super) fn set_stream(&self, name: &[u8], value: &[u8], how: Setting) -> Result<()> {
        let units = stream_name(name)?;
        self.writable()?;
        self.editable()?;
        let size = self.cache.as_ref().map_or(0, FileCache::size);
        let _ns = self.fs.ns.lock();
        let mut buf = self.read_rec()?;
        let old = edit::find(&buf, kind::DATA, &units);
        match (old, how) {
            (Some(_), Setting::Create) => return Err(Error::Exists),
            (None, Setting::Replace) => return Err(Error::NoAttribute),
            _ => {}
        }
        self.fs.changing()?;
        let (instance, freed) = self.take_out(&mut buf, &units)?;
        let mut moved = mem::vec();
        let taken = match self.put_stream(&mut buf, &units, instance, value, size) {
            Err(Error::NoSpace) => {
                // Room made as for a name: its own data leaves the record,
                // then its other streams, for this one in clusters.
                let need = (0x40 + units.len()).next_multiple_of(8) + RUNS_ROOM;
                let need = need + self.data_reserve(&buf, size);
                moved = self.make_room(&mut buf, need, size)?;
                match self.put_stream(&mut buf, &units, instance, value, size) {
                    Ok(taken) => taken,
                    Err(err) => {
                        self.give_back(&moved);
                        return Err(err);
                    }
                }
            }
            taken => taken?,
        };
        if let Err(err) = self.write_rec(&mut buf) {
            self.give_back(&taken);
            self.give_back(&moved);
            return Err(err);
        }
        self.give_back(&freed);
        self.attributes_changed();
        Ok(())
    }

    /// Removes its attribute `name`: the stream it stands for goes.
    pub(super) fn remove_stream(&self, name: &[u8]) -> Result<()> {
        let units = stream_name(name)?;
        self.writable()?;
        self.editable()?;
        let _ns = self.fs.ns.lock();
        let mut buf = self.read_rec()?;
        if edit::find(&buf, kind::DATA, &units).is_none() {
            return Err(Error::NoAttribute);
        }
        self.fs.changing()?;
        let (_, freed) = self.take_out(&mut buf, &units)?;
        self.write_rec(&mut buf)?;
        self.give_back(&freed);
        self.attributes_changed();
        Ok(())
    }

    /// The bytes its own data, of `size` bytes, is still to take in its
    /// record `buf` when it is next stored, which memory has ahead of the
    /// record: more of it in the record, or its record's attribute for
    /// where its clusters are, as they are now.
    fn data_reserve(&self, buf: &[u8], size: u64) -> usize {
        let st = self.state.lock();
        let had = edit::find(buf, kind::DATA, &[]).map_or(0, |at| u32_at(buf, at + 4) as usize);
        let will = match &st.data {
            _ if st.fixed.is_some() => had,
            Some(Data::Resident(_)) => (0x18 + size as usize).next_multiple_of(8),
            // Data spread over extension records leaves its base record.
            Some(Data::Clusters(stream)) if !st.list => {
                let header = if stream.packed() { 0x48 } else { 0x40 };
                match stream.runs.pack(0, stream.runs.clusters()) {
                    Ok(runs) => (header + runs.len()).next_multiple_of(8),
                    Err(_) => had,
                }
            }
            _ => had,
        };
        will.saturating_sub(had)
    }

    /// Takes the stream named `units` out of its record `buf`: its
    /// attribute's number, to keep, and the clusters it held, to free once
    /// `buf` is written. A stream not there gets the record's next number.
    fn take_out(&self, buf: &mut [u8], units: &[u8]) -> Result<(u16, KVec<(u64, u64)>)> {
        let Some(at) = edit::find(buf, kind::DATA, units) else {
            return Ok((edit::next_instance(buf), mem::vec()));
        };
        let instance = u16_at(buf, at + 0x0e);
        let mut freed = mem::vec();
        if buf[at + 8] != 0 {
            let rec = Record::new(buf)?;
            let mut runs = Runs::new();
            for attr in rec.attrs() {
                let attr = attr?;
                if attr.kind() == kind::DATA && attr.id() == instance {
                    if attr.lowest_vcn() != 0 {
                        return Err(Error::NotSupported);
                    }
                    let clusters = self.fs.vol.geo.clusters;
                    runs.add(attr.runs(), 0, attr.highest_vcn(), clusters)?;
                }
            }
            for run in runs.as_slice() {
                if run.lcn != HOLE {
                    mem::push(&mut freed, (run.lcn, run.len))?;
                }
            }
        }
        edit::remove(buf, at);
        Ok((instance, freed))
    }

    /// Puts the stream named `units`, attribute number `instance`,
    /// holding `value`, in its record `buf`: in it if it has room, or in
    /// clusters taken for it, written now, which it returns. Fails with
    /// [`Error::NoSpace`] if the record has room for neither, the room its
    /// own data, `size` bytes, still takes kept.
    fn put_stream(
        &self,
        buf: &mut [u8],
        units: &[u8],
        instance: u16,
        value: &[u8],
        size: u64,
    ) -> Result<KVec<(u64, u64)>> {
        let reserve = self.data_reserve(buf, size);
        let resident = edit::resident(kind::DATA, units, instance, value, false)?;
        if resident.len() + reserve <= edit::room(buf) {
            edit::insert_in(buf, &resident, &self.fs.upcase)?;
            return Ok(mem::vec());
        }
        drop(resident);
        self.put_clusters(buf, units, instance, value, reserve)
    }

    /// [`Self::put_stream`] in clusters: those it takes, written now, and
    /// returned. Fails with [`Error::NoSpace`] if its record `buf` has no
    /// room for where they are, with `reserve` bytes kept, or the value is
    /// empty.
    fn put_clusters(
        &self,
        buf: &mut [u8],
        units: &[u8],
        instance: u16,
        value: &[u8],
        reserve: usize,
    ) -> Result<KVec<(u64, u64)>> {
        if value.is_empty() {
            return Err(Error::NoSpace);
        }
        let cs = u64::from(self.fs.vol.geo.cluster_size);
        let len = value.len() as u64;
        let clusters = len.div_ceil(cs);
        let taken = self.fs.take_clusters(clusters, None)?;
        let put = (|| {
            let mut stream = Stream { size: len, initialized: len, ..Stream::new() };
            for &(lcn, n) in &taken {
                stream.runs.append(lcn, n)?;
            }
            let packed = stream.runs.pack(0, clusters)?;
            let sizes = edit::Sizes {
                allocated: clusters * cs,
                size: len,
                initialized: len,
                ..edit::Sizes::default()
            };
            let attr = edit::non_resident(
                kind::DATA,
                units,
                instance,
                0,
                0,
                clusters - 1,
                &packed,
                sizes,
            )?;
            if attr.len() + reserve > edit::room(buf) {
                return Err(Error::NoSpace);
            }
            write_bytes(&self.fs, &stream, value)?;
            edit::insert_in(buf, &attr, &self.fs.upcase)?;
            Ok(())
        })();
        match put {
            Ok(()) => Ok(taken),
            Err(err) => {
                self.give_back(&taken);
                Err(err)
            }
        }
    }

    /// Moves its streams in its record `buf` to clusters of their own,
    /// the largest first, until `buf` has `need` bytes of room, as
    /// Windows makes room in a record; returns the clusters they took.
    /// Fails with [`Error::NoSpace`] if moving them all leaves too little,
    /// giving back what they took; `buf` is then the caller's to drop.
    pub(super) fn streams_out(&self, buf: &mut [u8], need: usize) -> Result<KVec<(u64, u64)>> {
        let mut taken = mem::vec();
        let moved = (|| {
            while edit::room(buf) < need {
                // Only a value longer than where its clusters are saves
                // room.
                let at = edit::find_all(buf, kind::DATA)
                    .filter(|&at| buf[at + 8] == 0 && buf[at + 9] != 0)
                    .filter(|&at| u32_at(buf, at + 0x10) as usize > OUT_MIN)
                    .max_by_key(|&at| u32_at(buf, at + 0x10))
                    .ok_or(Error::NoSpace)?;
                let units = mem::copy(edit::name_at(buf, at))?;
                let value = mem::copy(edit::value_mut(buf, at))?;
                let instance = u16_at(buf, at + 0x0e);
                self.fs.changing()?;
                edit::remove(buf, at);
                let got = self.put_clusters(buf, &units, instance, &value, 0)?;
                for run in got {
                    mem::push(&mut taken, run)?;
                }
            }
            Ok(())
        })();
        match moved {
            Ok(()) => Ok(taken),
            Err(err) => {
                self.give_back(&taken);
                Err(err)
            }
        }
    }

    /// Gives back the clusters `runs` (runs of `(lcn, clusters)`).
    pub(super) fn give_back(&self, runs: &[(u64, u64)]) {
        for &(lcn, n) in runs {
            let _ = self.fs.give_clusters(lcn, n, false);
        }
    }

    /// Notes that its attributes changed: its change time is now. The
    /// caller holds `ns`; without the memory to keep it until the volume
    /// syncs, the time is not kept, as [`NtfsNode::touch`] does.
    fn attributes_changed(&self) {
        {
            let mut st = self.state.lock();
            st.info.changed = now_coarse();
            st.behind();
        }
        let _ = self.pin();
    }

    /// The clusters its streams hold in its base record `buf`, which go
    /// when the file does.
    pub(super) fn streams_clusters(&self, buf: &[u8]) -> Result<KVec<(u64, u64)>> {
        let rec = Record::new(buf)?;
        let clusters = self.fs.vol.geo.clusters;
        let mut held = mem::vec();
        for attr in rec.attrs() {
            let attr = attr?;
            if attr.kind() != kind::DATA
                || attr.unnamed()
                || !attr.non_resident()
                || attr.lowest_vcn() != 0
            {
                continue;
            }
            let mut runs = Runs::new();
            runs.add(attr.runs(), 0, attr.highest_vcn(), clusters)?;
            for run in runs.as_slice() {
                if run.lcn != HOLE {
                    mem::push(&mut held, (run.lcn, run.len))?;
                }
            }
        }
        Ok(held)
    }
}
