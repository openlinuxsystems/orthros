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
use radix_sync::Mutex;

use crate::{APPEND, Error, In, Out, Result};

const PAGE: u64 = PAGE_SIZE as u64;

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

    /// Writes the first `len` bytes of page `index` to the disk.
    fn write_page(&self, _index: u64, _page: &Page, _len: usize) -> Result<()> {
        Ok(())
    }

    /// Makes room for the file to be `size` bytes, before it grows.
    fn reserve(&self, size: u64) -> Result<()>;

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
fn page_len(index: u64, size: u64) -> usize {
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
                match load(&mut state, index, backing, false, true)? {
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
        let len = data.len() - from;
        let Some(start) = offset.checked_add(from as u64) else { return (0, Err(Error::TooBig)) };
        let Some(end) = start.checked_add(len as u64) else { return (0, Err(Error::TooBig)) };
        if end > backing.max_size() {
            return (0, Err(Error::TooBig));
        }
        if len == 0 {
            return (0, Ok(Copied::All(from)));
        }
        if end > state.size {
            if let Err(err) = backing.reserve(end) {
                return (0, Err(err));
            }
            if start > state.size {
                let size = state.size;
                if let Err(err) = zero_gap(&mut state, size, start, backing) {
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
            let bytes = &mut slot.page.bytes_mut()[within..within + n];
            let copied = data.get_now(from + done, bytes);
            if !matches!(copied, Ok(true)) {
                if made {
                    // A page made to be written whole holds none of the
                    // file's bytes: it goes, and the file keeps its own.
                    drop(state.pages.remove(index));
                } else {
                    // Some of the bytes may have been copied: those in
                    // the file go to the disk with the page, and those
                    // past its end are zeroed again, as all past it are.
                    let inside = size.saturating_sub(at).min(n as u64) as usize;
                    bytes[inside..].fill(0);
                    if inside > 0 && !core::mem::replace(&mut slot.dirty, true) {
                        state.dirty += 1;
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

    /// Writes every dirty page back. Pages whose write failed stay dirty.
    pub fn sync(&self, backing: &dyn Backing) -> Result<()> {
        if !backing.persistent() {
            return Ok(());
        }
        let mut state = self.state.lock();
        let State { pages, size, dirty, .. } = &mut *state;
        let mut failed = None;
        let mut index = 0;
        while *dirty > 0
            && let Some((found, _)) = pages.next(index)
        {
            index = found + 1;
            let Some(slot) = pages.get_mut(found) else { break };
            if !slot.dirty {
                continue;
            }
            match backing.write_page(found, &slot.page, page_len(found, *size)) {
                Ok(()) => {
                    slot.dirty = false;
                    *dirty -= 1;
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
            backing.read_page(index, &mut page, len)?;
            radix_hw::device_wrote(page.bytes_mut());
        }
        state.pages.insert(index, Slot { page, dirty: false }).map_err(|_| Error::NoMemory)?;
    }
    Ok(state.pages.get_mut(index))
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
            slot.page.bytes_mut()[within..].fill(0);
            if !slot.dirty {
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
