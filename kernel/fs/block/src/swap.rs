// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Linux's swap area (ADR 0152): what `mkswap` writes in the first page
//! of a partition or file, `union swap_header` of Linux's
//! `include/linux/swap.h`, version 1, little-endian, for pages of 4 KiB.
//!
//! The first 1024 bytes are left for a boot loader or a disk label. Then
//! come the version, the number of the last page of the area, how many
//! of its pages are bad, its UUID and label, and the bad pages' numbers;
//! the page ends with `SWAPSPACE2`. Page 0 is the header; pages 1 to the
//! last hold what is swapped out, a page each, but for the bad ones.

use core::sync::atomic::{AtomicUsize, Ordering};

use radix_sync::SpinLock;

use crate::{BlockDevice, PAGE_SIZE};

/// What ends the header's page.
pub const MAGIC: &[u8; 10] = b"SWAPSPACE2";

/// Where the version is, after the boot loader's room.
const VERSION_AT: usize = 1024;

/// Where the numbers of the bad pages start.
const BAD_AT: usize = VERSION_AT + 4 * 3 + 16 + 16 + 4 * 117;

/// The most bad pages a header lists: as many numbers as fit before the
/// magic, as Linux's `MAX_SWAP_BADPAGES`.
pub const MAX_BAD: u32 = ((PAGE_SIZE - 10 - BAD_AT) / 4) as u32;

/// The most swap areas on at once.
pub const MAX_AREAS: usize = 8;

/// The devices whose swap areas are on, by the address of each: their
/// raw writes and mounts fail, as they hold what the kernel swapped out.
static ON: SpinLock<[usize; MAX_AREAS]> = SpinLock::new([0; MAX_AREAS]);

/// How many of [`ON`] are taken: with none, [`is_on`] answers without the
/// lock, as every raw write asks.
static AREAS: AtomicUsize = AtomicUsize::new(0);

fn address(device: &dyn BlockDevice) -> usize {
    core::ptr::from_ref(device).cast::<()>() as usize
}

/// Notes that `device`'s swap area is on: `false` if it was, or
/// [`MAX_AREAS`] are.
pub fn claim(device: &dyn BlockDevice) -> bool {
    let at = address(device);
    let mut on = ON.lock();
    if on.contains(&at) {
        return false;
    }
    let Some(free) = on.iter_mut().find(|slot| **slot == 0) else { return false };
    *free = at;
    AREAS.fetch_add(1, Ordering::Release);
    true
}

/// Notes that `device`'s swap area is off.
pub fn release(device: &dyn BlockDevice) {
    let at = address(device);
    for slot in ON.lock().iter_mut().filter(|slot| **slot == at) {
        *slot = 0;
        AREAS.fetch_sub(1, Ordering::Release);
    }
}

/// Whether `device`'s swap area is on.
pub fn is_on(device: &dyn BlockDevice) -> bool {
    AREAS.load(Ordering::Acquire) > 0 && ON.lock().contains(&address(device))
}

/// The header of a swap area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The number of its last page: it has this many pages after the
    /// header.
    pub last_page: u32,
    /// How many of them are bad.
    pub bad_pages: u32,
    /// Its UUID, as `mkswap` makes one.
    pub uuid: [u8; 16],
    /// Its label, up to the first NUL.
    pub label: [u8; 16],
}

/// Why a page is not a swap area's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// It does not end with [`MAGIC`]: no swap area, or the old format
    /// Linux dropped in 2.5 (`SWAP-SPACE`).
    NotSwap,
    /// A version other than 1, or another byte order.
    Version,
    /// No page to swap to, or more bad pages than fit.
    Corrupt,
}

impl Header {
    /// Reads the header in `page`, the first page of a device of
    /// `device_pages` pages: the area's last page is cut to the device's,
    /// as Linux cuts it.
    pub fn read(page: &[u8], device_pages: u64) -> Result<Header, HeaderError> {
        if page.len() < PAGE_SIZE || &page[PAGE_SIZE - 10..PAGE_SIZE] != MAGIC {
            return Err(HeaderError::NotSwap);
        }
        let word =
            |at: usize| u32::from_le_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]]);
        if word(VERSION_AT) != 1 {
            return Err(HeaderError::Version);
        }
        let last_page = u64::from(word(VERSION_AT + 4)).min(device_pages.saturating_sub(1));
        let bad_pages = word(VERSION_AT + 8);
        if last_page == 0 || bad_pages > MAX_BAD {
            return Err(HeaderError::Corrupt);
        }
        let mut uuid = [0; 16];
        uuid.copy_from_slice(&page[VERSION_AT + 12..VERSION_AT + 28]);
        let mut label = [0; 16];
        label.copy_from_slice(&page[VERSION_AT + 28..VERSION_AT + 44]);
        // It fits: the word was at most u32::MAX.
        Ok(Header { last_page: last_page as u32, bad_pages, uuid, label })
    }

    /// The numbers of its bad pages, from the header's page `page`; those
    /// past its last page, or the header itself, are left out.
    pub fn bad<'a>(&self, page: &'a [u8]) -> impl Iterator<Item = u32> + 'a {
        let last = self.last_page;
        page[BAD_AT..BAD_AT + 4 * self.bad_pages as usize]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&bytes| u32::from_le_bytes(bytes))
            .filter(move |&bad| bad != 0 && bad <= last)
    }

    /// Its label, as text, if it has one in UTF-8.
    pub fn label(&self) -> Option<&str> {
        let len = self.label.iter().position(|&b| b == 0).unwrap_or(self.label.len());
        core::str::from_utf8(&self.label[..len]).ok().filter(|label| !label.is_empty())
    }
}
