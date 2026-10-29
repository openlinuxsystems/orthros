// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The page cache of a file's data.
//!
//! A [`FileCache`] holds a file's size and the pages of its data that are
//! in memory, indexed by page number in a radix tree
//! ([`radix_block::pagemap`]). Reads copy from the pages to the caller,
//! fetching missing pages from the disk first; writes copy from the
//! caller into the pages and mark them dirty, and [`FileCache::sync`]
//! writes dirty pages back. Each copy goes straight between the caller's
//! memory and the page (one copy, as Linux's `read` makes), and the disk
//! moves pages by DMA, so data is never copied twice.
//!
//! What lies below is the file's [`Backing`]: the filesystem's view of
//! where its data lives on the disk. tmpfs has no disk: its pages are the
//! only copy, a page never written is a hole that reads as zeros, and
//! nothing is written back. A disk filesystem reads and writes pages
//! through its backing, which also allocates space when the file grows
//! and frees it when it shrinks.
//!
//! When a file grows past its end, the bytes between the old end and the
//! new data must read as zeros, though the disk may hold anything there:
//! the cache zeroes them in the pages it holds and has the backing zero
//! the rest on the disk ([`Backing::zero`]). Appending, the common case,
//! leaves no gap, so it costs nothing.
//!
//! One lock guards a file's pages and size. It is held while the disk
//! works, so a file's I/O is one request at a time (enough for Phase 7);
//! the lock order is a file's cache, then its filesystem's own locks,
//! which the backing takes. A page fault on a program's mapping of a file
//! holds the address space's lock and reads the file, taking this one,
//! so a copy to or from a program's memory under this lock never waits
//! for a page to be made: it stops, the lock is let go while the pages
//! are made ([`Out::fault_in`]), and it goes on where it stopped, as
//! Linux's `generic_perform_write` does. Writes and size changes also
//! hold a second lock, which faults never take, for the whole call, so
//! that one stopped partway is still not split by another write or cut:
//! an `O_APPEND` write stays in one piece.

use radix_block::mem::CachePage;
use radix_block::pagemap::PageMap;
use radix_block::{PAGE_SIZE, Page};
use radix_sync::{Mutex, MutexGuard};

use crate::{APPEND, Error, In, Out, Result, mem};

const PAGE: u64 = PAGE_SIZE as u64;

/// The most pages [`FileCache::sync`] hands its backing in one write:
/// 256 KiB, two of the extents Btrfs compresses, which it compresses at
/// once on two CPUs (ADR 0134).
pub const WRITE_RUN: usize = 64;

/// The fewest pages a read that misses asks its backing for beyond those
/// it copies, when it goes on from where the cached pages end or starts
/// at the file's start: 128 KiB, Linux's default `read_ahead_kb`.
pub const READ_AHEAD: usize = 32;

/// The most pages one read that misses asks its backing for: 1 MiB.
pub const READ_RUN: usize = 256;

/// The most of a program's memory made at once when a copy finds a page
/// missing: the pages it will copy next, so a large read into new memory
/// lets go of the lock once for each 64 KiB rather than for each page.
const FAULT_IN_MAX: usize = 16 * PAGE_SIZE;

/// Where a file's data lives below its cache.
pub trait Backing {
    /// Whether the data has a home on a disk. Without one (tmpfs), the
    /// cache's pages are the data: missing pages are holes, and pages are
    /// never written back or dropped.
    fn persistent(&self) -> bool;

    /// Reads the first `len` bytes of page `index` of the file into
    /// `page`, whose other bytes are zero.
    fn read_page(&self, _index: u64, _page: &mut Page, _len: usize) -> Result<()> {
        Ok(())
    }

    /// Reads page `index` as [`Backing::read_page`] does. A backing that
    /// reads more of the file on the way (Btrfs uncompresses a whole
    /// extent to read a page of it) offers those pages to `more`, by
    /// index, with their bytes; the cache keeps each one it lacks, so that
    /// the work is not done again for them, as Linux adds a compressed
    /// extent's other pages to its cache. `more` returns false once it
    /// takes no more.
    fn read_pages(
        &self,
        index: u64,
        page: &mut Page,
        len: usize,
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<()> {
        let _ = more;
        self.read_page(index, page, len)
    }

    /// Reads the file's pages from `index` on into `pages`, one page each,
    /// in the file of `size` bytes, which the pages do not pass: as many
    /// as the backing reads at once (one extent's, one request to the
    /// disk). Returns how many it filled, at least one; their bytes past
    /// the file's end are zero. Pages past the run that it reads on the
    /// way go to `more`, as in [`Backing::read_pages`]. By default, the
    /// first page alone.
    fn read_run(
        &self,
        index: u64,
        pages: &mut [CachePage],
        size: u64,
        more: &mut dyn FnMut(u64, &[u8]) -> bool,
    ) -> Result<usize> {
        let Some(page) = pages.first_mut() else { return Err(Error::Io) };
        self.read_pages(index, page, page_len(index, size), more)?;
        Ok(1)
    }

    /// The most pages [`Backing::read_run`] fills at once: one, unless it
    /// does better. The page cache reads ahead only for a backing that
    /// reads more.
    fn run_pages(&self) -> usize {
        1
    }

    /// Writes the first `len` bytes of page `index` to the disk.
    fn write_page(&self, _index: u64, _page: &Page, _len: usize) -> Result<()> {
        Ok(())
    }

    /// Writes `pages`, the file's pages from `index` on, to the disk: all
    /// of each but the last, and the first `len` bytes of that. A backing
    /// that places each write anew (Btrfs, which copies on write) puts
    /// them in one extent; by default each is a [`Backing::write_page`].
    fn write_pages(&self, index: u64, pages: &[&Page], len: usize) -> Result<()> {
        let last = pages.len().saturating_sub(1);
        for (i, page) in pages.iter().enumerate() {
            let n = if i == last { len } else { PAGE_SIZE };
            self.write_page(index + i as u64, page, n)?;
        }
        Ok(())
    }

    /// Makes room for the file to be `size` bytes, before it grows.
    fn reserve(&self, size: u64) -> Result<()>;

    /// Makes room on the disk for `pages` clean pages about to change, as
    /// a filesystem that writes every change to a new place (Btrfs) must;
    /// by default there is nothing to do. Fails with [`Error::NoSpace`]
    /// when there is none, so that the write fails as on Linux, not the
    /// write-back after it.
    fn reserve_pages(&self, _pages: usize) -> Result<()> {
        Ok(())
    }

    /// Gives back the room [`Backing::reserve_pages`] made for `pages`
    /// pages, which were written back or dropped.
    fn release_pages(&self, _pages: usize) {}

    /// Sets the file's bytes from `from` to `to` (both multiples of the
    /// page size) to zero on the disk.
    fn zero(&self, _from: u64, _to: u64) -> Result<()> {
        Ok(())
    }

    /// The file is now `size` bytes: frees the room past it.
    fn truncate(&self, size: u64) -> Result<()>;

    /// The largest size the file may have.
    fn max_size(&self) -> u64 {
        u64::MAX / 2
    }
}

struct Slot {
    page: CachePage,
    dirty: bool,
}

struct State {
    pages: PageMap<Slot>,
    size: u64,
    dirty: usize,
    /// When the data last changed.
    modified: u64,
}

/// A file's size and cached pages.
pub struct FileCache {
    state: Mutex<State>,
    /// Held by a write or size change for the whole call, while `state`
    /// may be let go to make a page of the caller's memory.
    writing: Mutex<()>,
}

/// How far a copy under the lock got.
enum Copied {
    /// All of it: the bytes read or written in all.
    All(usize),
    /// Up to a page of the caller's memory that is not there yet; this
    /// many bytes are left to copy.
    Missing(usize),
}

/// The bytes of page `index` inside a file of `size` bytes.
pub fn page_len(index: u64, size: u64) -> usize {
    size.saturating_sub(index * PAGE).min(PAGE) as usize
}

impl FileCache {
    /// The cache of a file of `size` bytes, changed at `modified`, with
    /// nothing in memory yet.
    pub fn new(size: u64, modified: u64) -> FileCache {
        FileCache {
            state: Mutex::new(State { pages: PageMap::new(), size, dirty: 0, modified }),
            writing: Mutex::new(()),
        }
    }

    /// The file's size.
    pub fn size(&self) -> u64 {
        self.state.lock().size
    }

    /// When the file's data last changed.
    pub fn modified(&self) -> u64 {
        self.state.lock().modified
    }

    /// Says the file's data last changed at `modified` (`set_times`).
    pub fn set_modified(&self, modified: u64) {
        self.state.lock().modified = modified;
    }

    /// How many pages it holds.
    pub fn pages(&self) -> usize {
        self.state.lock().pages.len()
    }

    /// The file's size, when its data last changed, and how many pages it
    /// holds, read together under one lock, for `stat`.
    pub fn status(&self) -> (u64, u64, usize) {
        let state = self.state.lock();
        (state.size, state.modified, state.pages.len())
    }

    /// How many pages wait to be written back.
    pub fn dirty_pages(&self) -> usize {
        self.state.lock().dirty
    }

    /// [`Self::dirty_pages`], read without the lock, by the cache's owner
    /// as it goes: any lock may be held then.
    pub fn dirty_pages_mut(&mut self) -> usize {
        self.state.get_mut().dirty
    }

    /// Whether some pages wait to be written back.
    pub fn is_dirty(&self) -> bool {
        self.state.lock().dirty > 0
    }

    /// Reads from `offset` into `out`; returns the bytes read.
    pub fn read(&self, offset: u64, out: &mut dyn Out, backing: &dyn Backing) -> Result<usize> {
        let mut done = 0;
        loop {
            match self.read_from(offset, out, backing, &mut done)? {
                Copied::All(read) => return Ok(read),
                Copied::Missing(left) => out.fault_in(done, left.min(FAULT_IN_MAX))?,
            }
        }
    }

    /// [`FileCache::read`] under the lock, from `done` bytes on, which it
    /// counts up as it copies.
    fn read_from(
        &self,
        offset: u64,
        out: &mut dyn Out,
        backing: &dyn Backing,
        done: &mut usize,
    ) -> Result<Copied> {
        let mut state = self.state.lock();
        // A file cut while the lock was let go ends the read early.
        if offset >= state.size {
            return Ok(Copied::All(*done));
        }
        let len = (out.len() as u64).min(state.size - offset) as usize;
        while *done < len {
            let at = offset + *done as u64;
            let (index, within) = (at / PAGE, (at % PAGE) as usize);
            let n = (PAGE_SIZE - within).min(len - *done);
            // A page in memory, the common case, is looked up once.
            let copied = if let Some(slot) = state.pages.get(index) {
                out.put_now(*done, &slot.page.bytes()[within..within + n])?
            } else {
                let last = (offset + len as u64 - 1) / PAGE;
                match load_run(&mut state, index, last, backing)? {
                    Some(slot) => out.put_now(*done, &slot.page.bytes()[within..within + n])?,
                    // A hole.
                    None => zero_now(out, *done, n)?,
                }
            };
            if !copied {
                return Ok(Copied::Missing(len - *done));
            }
            *done += n;
        }
        Ok(Copied::All(len.max(*done)))
    }

    /// Writes `data` at `offset` ([`APPEND`]: at the end); returns the
    /// bytes written. A fault partway through keeps what was written.
    pub fn write(&self, offset: u64, data: &dyn In, backing: &dyn Backing) -> Result<usize> {
        let _writing = self.writing.lock();
        let mut offset = offset;
        let mut done = 0;
        loop {
            let (wrote, copied) = self.write_from(&mut offset, data, done, backing);
            done += wrote;
            let made = match copied {
                Ok(Copied::All(total)) => return Ok(total),
                Ok(Copied::Missing(left)) => data.fault_in(done, left.min(FAULT_IN_MAX)),
                Err(err) => Err(err),
            };
            if let Err(err) = made {
                return if done == 0 { Err(err) } else { Ok(done) };
            }
        }
    }

    /// [`FileCache::write`] under the lock, of `data` from `from` bytes on;
    /// returns the bytes it wrote, with how far it got or what failed.
    /// `offset` is where `data` starts in the file, [`APPEND`] until the
    /// first call makes it the end.
    fn write_from(
        &self,
        offset: &mut u64,
        data: &dyn In,
        from: usize,
        backing: &dyn Backing,
    ) -> (usize, Result<Copied>) {
        let mut state = self.state.lock();
        if *offset == APPEND {
            *offset = state.size;
        }
        if data.len() == from {
            return (0, Ok(Copied::All(from)));
        }
        // As on Linux, a write that would pass the largest size the
        // filesystem allows writes what fits, and only one that starts
        // there fails.
        let max = backing.max_size();
        let start = match offset.checked_add(from as u64) {
            Some(start) if start < max => start,
            _ => return (0, Err(Error::TooBig)),
        };
        let len = (data.len() - from).min(usize::try_from(max - start).unwrap_or(usize::MAX));
        let end = start + len as u64;
        if end > state.size {
            if let Err(err) = backing.reserve(end) {
                return (0, Err(err));
            }
            if start > state.size {
                let size = state.size;
                if let Err(err) = zero_gap(&mut state, size, start, backing) {
                    // The room just reserved goes back with the write.
                    let _ = backing.truncate(size);
                    return (0, Err(err));
                }
            }
        }
        let mut done = 0;
        let mut stopped = None;
        while done < len {
            let at = start + done as u64;
            let (index, within) = (at / PAGE, (at % PAGE) as usize);
            let n = (PAGE_SIZE - within).min(len - done);
            let size = state.size;
            // A page written whole need not be read first.
            let whole = within == 0 && (n == PAGE_SIZE || at + n as u64 >= size);
            let made = whole && state.pages.get(index).is_none();
            let slot = match load(&mut state, index, backing, true, !whole) {
                Ok(Some(slot)) => slot,
                Ok(None) => unreachable!("loading to write always makes a page"),
                Err(err) => {
                    stopped = Some(Err(err));
                    break;
                }
            };
            // Room on the disk comes before the page changes.
            let reserved = !slot.dirty;
            if reserved && let Err(err) = backing.reserve_pages(1) {
                if made {
                    drop(state.pages.remove(index));
                }
                stopped = Some(Err(err));
                break;
            }
            let bytes = &mut slot.page.bytes_mut()[within..within + n];
            let copied = data.get_now(from + done, bytes);
            if !matches!(copied, Ok(true)) {
                if made {
                    // A page made to be written whole holds none of the
                    // file's bytes: it goes, and the file keeps its own.
                    drop(state.pages.remove(index));
                    backing.release_pages(1);
                } else {
                    // Some of the bytes may have been copied: those in
                    // the file go to the disk with the page, and those
                    // past its end are zeroed again, as all past it are.
                    let inside = size.saturating_sub(at).min(n as u64) as usize;
                    bytes[inside..].fill(0);
                    if inside > 0 && !core::mem::replace(&mut slot.dirty, true) {
                        state.dirty += 1;
                    } else if reserved && !slot.dirty {
                        backing.release_pages(1);
                    }
                }
                stopped = Some(copied.map(|_| Copied::Missing(len - done)));
                break;
            }
            let was_dirty = core::mem::replace(&mut slot.dirty, true);
            if !was_dirty {
                state.dirty += 1;
            }
            done += n;
        }
        let new_end = start + done as u64;
        if new_end > state.size {
            state.size = new_end;
        }
        if done > 0 {
            state.modified = crate::now();
        }
        // Room reserved past what was written is given back.
        if new_end < end && end > state.size {
            let size = state.size;
            let _ = backing.truncate(size);
        }
        (done, stopped.unwrap_or(Ok(Copied::All(from + done))))
    }

    /// Makes the file `size` bytes long: cut, or grown with zeros.
    pub fn set_size(&self, size: u64, backing: &dyn Backing) -> Result<()> {
        let _writing = self.writing.lock();
        let mut state = self.state.lock();
        if size > backing.max_size() {
            return Err(Error::TooBig);
        }
        let old = state.size;
        if size > old {
            backing.reserve(size)?;
            zero_gap(&mut state, old, size, backing)?;
            state.size = size;
        } else if size < old {
            // Pages wholly past the end go, dirty or not; the part of the
            // last page past the end is zeroed, so growing later reads
            // zeros there.
            let first_gone = size.div_ceil(PAGE);
            let mut lost_dirty = 0;
            state.pages.retain(
                |index, slot| {
                    let keep = index < first_gone;
                    if !keep && slot.dirty {
                        lost_dirty += 1;
                    }
                    keep
                },
                drop,
            );
            state.dirty -= lost_dirty;
            backing.release_pages(lost_dirty);
            let within = (size % PAGE) as usize;
            if within != 0
                && let Some(slot) = state.pages.get_mut(size / PAGE)
            {
                slot.page.bytes_mut()[within..].fill(0);
            }
            state.size = size;
            backing.truncate(size)?;
        }
        state.modified = crate::now();
        Ok(())
    }

    /// Holds off writes and size changes until the guard goes, while the
    /// file's bytes change on the disk beneath the cache (a clone). Two
    /// files held at once are held in one order, the second `second`.
    pub fn hold(&self, second: bool) -> MutexGuard<'_, ()> {
        self.writing.lock_nested(u8::from(second))
    }

    /// Grows the file to `size` bytes, its new bytes zeros, as
    /// [`set_size`](FileCache::set_size) does, for a caller that
    /// [holds](FileCache::hold) it.
    pub fn grow_held(&self, size: u64, backing: &dyn Backing) -> Result<()> {
        let mut state = self.state.lock();
        let old = state.size;
        if size > old {
            if size > backing.max_size() {
                return Err(Error::TooBig);
            }
            backing.reserve(size)?;
            zero_gap(&mut state, old, size, backing)?;
            state.size = size;
        }
        Ok(())
    }

    /// Says the disk's bytes of the file from `from` to `to` changed
    /// beneath the cache, which the caller [held](FileCache::hold) after
    /// writing them back, and that the file is now at least `size` bytes
    /// long: the pages there go, to be read again.
    pub fn replaced(&self, from: u64, to: u64, size: u64, backing: &dyn Backing) {
        let mut state = self.state.lock();
        let (first, end) = (from / PAGE, to.div_ceil(PAGE));
        let mut lost_dirty = 0;
        state.pages.retain(
            |index, slot| {
                let keep = index < first || index >= end;
                if !keep && slot.dirty {
                    lost_dirty += 1;
                }
                keep
            },
            drop,
        );
        state.dirty -= lost_dirty;
        backing.release_pages(lost_dirty);
        state.size = state.size.max(size);
        state.modified = crate::now();
    }

    /// Writes every dirty page back, in runs of up to [`WRITE_RUN`] pages
    /// next to each other in the file. Pages whose write failed stay
    /// dirty.
    pub fn sync(&self, backing: &dyn Backing) -> Result<()> {
        if !backing.persistent() {
            return Ok(());
        }
        let mut state = self.state.lock();
        let State { pages, size, dirty, .. } = &mut *state;
        let mut failed = None;
        let mut index = 0;
        while *dirty > 0
            && let Some((found, first)) = pages.next(index)
        {
            if !first.dirty {
                index = found + 1;
                continue;
            }
            let mut run = [&*first.page; WRITE_RUN];
            let mut n = 1;
            while n < WRITE_RUN
                && let Some(slot) = pages.get(found + n as u64)
                && slot.dirty
            {
                run[n] = &*slot.page;
                n += 1;
            }
            let last = found + n as u64 - 1;
            index = last + 1;
            match backing.write_pages(found, &run[..n], page_len(last, *size)) {
                Ok(()) => {
                    for at in found..=last {
                        if let Some(slot) = pages.get_mut(at) {
                            slot.dirty = false;
                        }
                    }
                    *dirty -= n;
                    backing.release_pages(n);
                }
                Err(err) => failed = Some(err),
            }
        }
        match failed {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Drops up to `max` clean pages, which the disk has; returns how many
    /// went. Pages of a file without a disk are never dropped.
    pub fn shrink(&self, max: usize, backing: &dyn Backing) -> usize {
        if !backing.persistent() || max == 0 {
            return 0;
        }
        let mut state = self.state.lock();
        let mut dropped = 0;
        state.pages.retain(
            |_, slot| {
                if dropped < max && !slot.dirty {
                    dropped += 1;
                    false
                } else {
                    true
                }
            },
            drop,
        );
        dropped
    }
}

/// Page `index`, read from the backing if it is not in memory (unless
/// `fill` is false: the caller overwrites all of it). Without a disk, a
/// missing page is a hole: `None`, unless the caller is writing (`make`),
/// when a zeroed page is made.
fn load<'s>(
    state: &'s mut State,
    index: u64,
    backing: &dyn Backing,
    make: bool,
    fill: bool,
) -> Result<Option<&'s mut Slot>> {
    if state.pages.get(index).is_none() {
        if !backing.persistent() && !make {
            return Ok(None);
        }
        let mut page = CachePage::new()?;
        let len = page_len(index, state.size);
        if backing.persistent() && fill && len > 0 {
            let (pages, size) = (&mut state.pages, state.size);
            let mut more = |at, bytes: &[u8]| keep(pages, size, index, at, bytes);
            backing.read_pages(index, &mut page, len, &mut more)?;
            radix_hw::device_wrote(page.bytes_mut());
        }
        state.pages.insert(index, Slot { page, dirty: false }).map_err(|_| Error::NoMemory)?;
    }
    Ok(state.pages.get_mut(index))
}

/// Page `index`, which is not in memory, read from the backing for a read
/// that copies up to page `last`, with the pages after it that one read
/// of the backing gives: up to `last`, and more when the read starts the
/// file (three times what it asks for) or goes on from cached pages (as
/// many as are cached right before it), [`READ_AHEAD`] at least, up to
/// [`READ_RUN`] in all and no more than the backing reads at once, and
/// none that is in memory or past the end. Without a disk, or with one
/// read a page at a time, [`load`]'s.
fn load_run<'s>(
    state: &'s mut State,
    index: u64,
    last: u64,
    backing: &dyn Backing,
) -> Result<Option<&'s mut Slot>> {
    let most = backing.run_pages().min(READ_RUN) as u64;
    if !backing.persistent() || most <= 1 || state.size <= index * PAGE {
        return load(state, index, backing, false, true);
    }
    // A read from the start reads ahead three times what it asks for, and
    // one that goes on from cached pages as many as it goes on from: the
    // window doubles while a reader keeps on.
    let wanted = last + 1 - index;
    let behind =
        (1..=most.min(index)).take_while(|&n| state.pages.get(index - n).is_some()).count();
    let ahead = match (index, behind) {
        (0, _) => (3 * wanted).max(READ_AHEAD as u64),
        (_, 0) => 0,
        (_, behind) => (behind as u64).max(READ_AHEAD as u64),
    };
    let end_page = state.size.div_ceil(PAGE);
    let end = (last + 1).saturating_add(ahead).min(index + most).min(end_page);
    // Short of memory, a shorter run.
    let mut pages =
        mem::vec_with_capacity((end - index) as usize).or_else(|_| mem::vec_with_capacity(1))?;
    for at in index..end {
        if pages.len() == pages.capacity() || at != index && state.pages.get(at).is_some() {
            break;
        }
        match CachePage::new() {
            Ok(page) => mem::push(&mut pages, page)?,
            Err(e) if at == index => return Err(e.into()),
            Err(_) => break,
        }
    }
    // Pages of the run are the backing's to fill, not to offer.
    let run = index..index + pages.len() as u64;
    let (map, size) = (&mut state.pages, state.size);
    let mut more = |at, bytes: &[u8]| run.contains(&at) || keep(map, size, index, at, bytes);
    let read = backing.read_run(index, &mut pages, size, &mut more)?;
    for (at, mut page) in (index..).zip(pages.into_iter().take(read.max(1))) {
        radix_hw::device_wrote(page.bytes_mut());
        if state.pages.get(at).is_none() {
            state.pages.insert(at, Slot { page, dirty: false }).map_err(|_| Error::NoMemory)?;
        }
    }
    Ok(state.pages.get_mut(index))
}

/// Keeps page `at` of a file of `size` bytes, which a backing read along
/// with page `index`, unless the cache holds it already or it is past the
/// end; returns whether the cache takes more.
fn keep(pages: &mut PageMap<Slot>, size: u64, index: u64, at: u64, bytes: &[u8]) -> bool {
    let len = page_len(at, size);
    if at == index || len == 0 || pages.get(at).is_some() {
        return true;
    }
    let Ok(mut page) = CachePage::new() else { return false };
    let n = len.min(bytes.len());
    page.bytes_mut()[..n].copy_from_slice(&bytes[..n]);
    pages.insert(at, Slot { page, dirty: false }).is_ok()
}

/// Zeroes `len` bytes of `out` from `at`, without waiting for a page, as
/// [`Out::put_now`] copies.
fn zero_now(out: &mut dyn Out, at: usize, len: usize) -> Result<bool> {
    let zeros = [0u8; 256];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(zeros.len());
        if !out.put_now(at + done, &zeros[..n])? {
            return Ok(false);
        }
        done += n;
    }
    Ok(true)
}

/// Makes the bytes from `from` to `to` read as zeros, as a file grows
/// over them: the part in the page of `from` is zeroed in the cache, and
/// whole pages after it on the disk (or, cached, in the cache).
fn zero_gap(state: &mut State, from: u64, to: u64, backing: &dyn Backing) -> Result<()> {
    if to <= from {
        return Ok(());
    }
    // The page holding the old end: its bytes past the end are zero
    // already (reads zero them, `set_size` zeroes them on a cut), but on
    // a disk with this page not cached they may not be.
    let first_whole = from.div_ceil(PAGE);
    if !from.is_multiple_of(PAGE) && backing.persistent() {
        let index = from / PAGE;
        let within = (from % PAGE) as usize;
        if let Some(slot) = load(state, index, backing, true, true)? {
            // Those bytes are past the end, so zeroing them changes no
            // byte of the file even if there is no room to write it.
            slot.page.bytes_mut()[within..].fill(0);
            if !slot.dirty {
                backing.reserve_pages(1)?;
                slot.dirty = true;
                state.dirty += 1;
            }
        }
    }
    if !backing.persistent() {
        // Holes read as zeros already, and cached pages past the end were
        // zeroed when the file was cut.
        return Ok(());
    }
    // Up to the end of the page of `to`: what of it lies past `to` is past
    // the end of the file, and must read as zeros too when it grows.
    let last_whole = to.div_ceil(PAGE);
    if first_whole < last_whole {
        // Cached pages in the gap are zeroed in memory too.
        let mut index = first_whole;
        while let Some((found, _)) = state.pages.next(index) {
            if found >= last_whole {
                break;
            }
            if let Some(slot) = state.pages.get_mut(found) {
                slot.page.bytes_mut().fill(0);
            }
            index = found + 1;
        }
        backing.zero(first_whole * PAGE, last_whole * PAGE)?;
    }
    Ok(())
}
