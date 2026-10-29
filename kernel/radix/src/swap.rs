// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Swap (Phase 10d, ADR 0152): Linux's swap areas, and the thread that
//! moves programs' anonymous memory out to them when memory runs short.
//!
//! A swap area is a disk or partition `mkswap` made
//! (`radix_block::swap`), turned on and off by control requests on its
//! block device (radix-block(2)'s `SWAP_ON` and `SWAP_OFF`, swapon(8)).
//! Each page after the header is a slot, whose byte in the area's map
//! counts the address spaces whose entries hold it: 0 for a free one, and
//! [`NOT_SLOT`] for the header and the bad pages. Slots are numbered
//! `area << 32 | page`, which a page table's entry keeps in 40 bits.
//! Areas are filled highest priority first, as on Linux, whose default
//! priorities count down from -2 in the order the areas are turned on.
//!
//! The `swap` thread wakes every second while an area is on, and when
//! the out-of-memory policy or a fault that found memory short asks
//! ([`reclaim`]). While free memory is under 1/32 of RAM it swaps pages
//! out until it is over 1/16 (`user::memory::swap_out`), and an ask gets
//! it over 1/16, or at least [`RECLAIM_MIN`] pages out. Swapping out
//! takes the address spaces' locks, and writes to disks, so it runs here,
//! in a thread that holds no lock, never inside an allocation; a fault
//! lets go of its space's lock before it asks. Pages come back on the
//! next fault at their address (`SpaceObject::resolve`), one at a time.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::Reverse;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

use radix_abi::Status;
use radix_abi::errors::{File as FileError, Kernel};
use radix_block::swap::Header;
use radix_block::{DeviceRef, SECTORS_PER_PAGE};
use radix_hal::UserMode;
use radix_klog::{error, info};
use radix_mm::{FRAMES, Flags, HEAP, Kmalloc, Owner, PAGE_SIZE};
use radix_sync::{Mutex, Once, SpinLock};

use crate::Arch;
use crate::fs::disk::Name;
use crate::sched::{self, Options, Policy, ThreadRef, wait};

/// The most areas on at once.
const MAX_AREAS: usize = radix_block::swap::MAX_AREAS;

/// The most pages written in one request, a run of slots: as many as a
/// disk's request lists on the stack (`fs::disk::queue::MAX_SEGMENTS`),
/// so that swapping out allocates no memory.
pub const BATCH: usize = 16;

/// A map's byte for a page that is no slot: the header, or a bad page.
const NOT_SLOT: u8 = u8::MAX;

/// The most entries a slot is counted for; a fork that would share it
/// with one more fails for want of memory.
const MAX_SHARES: u8 = NOT_SLOT - 1;

/// The most pages an area may have: 2^32, the slot's page part.
const MAX_PAGES: u64 = 1 << 32;

/// How often the thread looks at free memory while an area is on.
const INTERVAL_NS: u64 = 1_000_000_000;

/// The fewest pages an ask of the out-of-memory policy gets out.
const RECLAIM_MIN: u64 = 256;

/// How long [`make_room`] waits for each pass of the thread.
const ROOM_WAIT_NS: u64 = 200_000_000;

/// The most passes [`make_room`] asks for.
const ROOM_PASSES: u32 = 16;

/// One area.
struct Area {
    device: DeviceRef,
    name: Name,
    priority: i32,
    /// Its pages, the header's and the bad ones' included.
    pages: u64,
    /// Those that are slots.
    slots: u64,
    map: SpinLock<Map>,
}

type AreaRef = Arc<Area, Kmalloc>;

/// An area's slots.
struct Map {
    /// A byte a page: how many entries hold it, or [`NOT_SLOT`].
    counts: Vec<u8, Kmalloc>,
    /// The slots in use.
    used: u64,
    /// Where the next search for free slots starts.
    next: usize,
    /// Set while it is turned off: no slot is taken from it.
    closing: bool,
}

/// The areas on, by their number in a slot.
static AREAS: SpinLock<[Option<AreaRef>; MAX_AREAS]> = SpinLock::new([const { None }; MAX_AREAS]);

/// Held while an area is turned on or off, which may take a while.
static TURNING: Mutex<()> = Mutex::new(());

/// How many areas are on: what every page fault looks at.
static ON: AtomicUsize = AtomicUsize::new(0);

/// The priority the next area turned on without one gets.
static NEXT_PRIORITY: AtomicI32 = AtomicI32::new(-2);

/// Pages written out and read back in since boot, for `/processes/stat`
/// and vmstat(8).
pub static OUT: AtomicU64 = AtomicU64::new(0);
pub static IN: AtomicU64 = AtomicU64::new(0);

/// Asks of the out-of-memory policy, and those answered.
static ASKED: AtomicU64 = AtomicU64::new(0);
static DONE: AtomicU64 = AtomicU64::new(0);

static THREAD: Once<ThreadRef> = Once::new();

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

fn asked_key() -> usize {
    &raw const ASKED as usize
}

fn done_key() -> usize {
    &raw const DONE as usize
}

/// The area of `slot`, and the page in it.
fn area_of(slot: u64) -> Option<(AreaRef, usize)> {
    let area = AREAS.lock().get((slot >> 32) as usize)?.clone()?;
    Some((area, (slot & 0xffff_ffff) as usize))
}

/// Whether any area is on.
pub fn on_any() -> bool {
    ON.load(Ordering::Relaxed) > 0
}

/// Turns on the swap area on `device`, with radix-block(2)'s `SWAP_ON`
/// `flags`: their priority or the next default one. Fails with `in_use`
/// if it is on already, `read_only` on a device that refuses writes,
/// `invalid_argument` if it holds no swap area this kernel takes or the
/// flags are not known, `not_supported` for the flags that discard, and
/// `busy` if [`MAX_AREAS`] are on.
pub fn on(device: &DeviceRef, flags: u32) -> Result<(), Status> {
    use radix_abi::block::{SWAP_DISCARD_FLAGS, SWAP_PREFER, SWAP_PRIORITY_MASK};
    if flags & SWAP_DISCARD_FLAGS != 0 {
        return Err(Kernel::NotSupported.status(0));
    }
    if flags & !(SWAP_PREFER | SWAP_PRIORITY_MASK) != 0 {
        return Err(Kernel::InvalidArgument.status(0));
    }
    // The mask leaves 15 bits.
    let priority = (flags & SWAP_PREFER != 0).then_some((flags & SWAP_PRIORITY_MASK) as i32);
    let _turning = TURNING.lock();
    if radix_block::swap::is_on(&**device) {
        return Err(FileError::InUse.status(0));
    }
    let info = device.info();
    if info.read_only {
        return Err(FileError::ReadOnly.status(0));
    }
    let pages = (info.sectors / SECTORS_PER_PAGE).min(MAX_PAGES);
    let page = read_header(device)?;
    let header = Header::read(&page, pages).map_err(|_| Kernel::InvalidArgument.status(0))?;
    let last = u64::from(header.last_page);
    let mut counts = radix_mm::bytes::zeroed(last as usize + 1, kmalloc())
        .map_err(|_| Kernel::NoMemory.status(0))?;
    counts[0] = NOT_SLOT;
    let mut slots = last;
    for bad in header.bad(&page) {
        if counts[bad as usize] != NOT_SLOT {
            counts[bad as usize] = NOT_SLOT;
            slots -= 1;
        }
    }
    if slots == 0 {
        return Err(Kernel::InvalidArgument.status(0));
    }
    let name = crate::fs::disk::name_of(device).unwrap_or_default();
    let priority = priority.unwrap_or_else(|| NEXT_PRIORITY.fetch_sub(1, Ordering::Relaxed));
    let map = Map { counts, used: 0, next: 1, closing: false };
    let area = Area {
        device: device.clone(),
        name: name.clone(),
        priority,
        pages: last + 1,
        slots,
        map: SpinLock::new(map),
    };
    let area = Arc::try_new_in(area, kmalloc()).map_err(|_| Kernel::NoMemory.status(0))?;
    {
        let mut areas = AREAS.lock();
        let Some(free) = areas.iter_mut().find(|area| area.is_none()) else {
            return Err(Kernel::Busy.status(0));
        };
        // With the areas locked, and the device's raw writes stopped from
        // now on.
        if !radix_block::swap::claim(&**device) {
            return Err(Kernel::Busy.status(0));
        }
        *free = Some(area);
        ON.fetch_add(1, Ordering::Relaxed);
    }
    start_thread();
    // The thread sleeps while no area is on.
    wait::wake_all(asked_key());
    info!(
        "Swap: {name}: {} MiB in {slots} slots, priority {priority}{}",
        (slots * PAGE_SIZE as u64) >> 20,
        if header.bad_pages > 0 { ", bad pages left out" } else { "" }
    );
    Ok(())
}

/// The first page of `device`, read past every cache.
#[inline(never)]
fn read_header(device: &DeviceRef) -> Result<Vec<u8, Kmalloc>, Status> {
    let phys =
        FRAMES.alloc(0, Flags::MAY_SLEEP, Owner::Kernel).map_err(|_| Kernel::NoMemory.status(0))?;
    let read = device.read_frames(0, &[(phys, PAGE_SIZE as u32)]);
    let page = radix_mm::bytes::zeroed(PAGE_SIZE, kmalloc());
    if let (Ok(()), Ok(mut page)) = (read, page) {
        Arch::read_frames(phys, &mut page);
        let _ = FRAMES.free(phys, 0, Owner::Kernel);
        return Ok(page);
    }
    let _ = FRAMES.free(phys, 0, Owner::Kernel);
    match read {
        Err(radix_block::Error::Unsupported | radix_block::Error::OutOfRange) => {
            Err(Kernel::InvalidArgument.status(0))
        }
        Err(_) => Err(FileError::Io.status(0)),
        Ok(()) => Err(Kernel::NoMemory.status(0)),
    }
}

/// How many times turning an area off goes through every address space:
/// a fork of a space not yet looked at may give its slots to one added
/// after the look began.
const OFF_PASSES: usize = 4;

/// Turns off the swap area on `device`, once every page swapped out to
/// it is back in memory: fails with `invalid_argument` if none is on,
/// `no_memory` if the pages do not fit ([`room`] says at once if they
/// cannot) and `io` if one cannot be read, leaving it on.
pub fn off(device: &DeviceRef) -> Result<(), Status> {
    let _turning = TURNING.lock();
    let found = AREAS.lock().iter().enumerate().find_map(|(at, area)| {
        area.as_ref().filter(|a| Arc::ptr_eq(&a.device, device)).map(|_| at)
    });
    let Some(at) = found else { return Err(Kernel::InvalidArgument.status(0)) };
    let Some((area, _)) = area_of((at as u64) << 32) else {
        return Err(Kernel::InvalidArgument.status(0));
    };
    // More out on it than there is room for fails at once, rather than
    // filling memory first.
    if area.map.lock().used > room(at) {
        return Err(Kernel::NoMemory.status(0));
    }
    area.map.lock().closing = true;
    let mut back = Ok(());
    for _ in 0..OFF_PASSES {
        back = crate::user::memory::swap_in_all(at as u64);
        if back.is_err() || area.map.lock().used == 0 {
            break;
        }
    }
    let failed = match back {
        Err(crate::user::memory::Fault::Io) => Some(FileError::Io.status(0)),
        Err(_) => Some(Kernel::NoMemory.status(0)),
        Ok(()) => (area.map.lock().used != 0).then(|| Kernel::NoMemory.status(0)),
    };
    if let Some(status) = failed {
        area.map.lock().closing = false;
        return Err(status);
    }
    // Dropped with the lock let go: the map is freed with the last handle.
    let gone = AREAS.lock()[at].take();
    ON.fetch_sub(1, Ordering::Relaxed);
    drop(gone);
    radix_block::swap::release(&**device);
    info!("Swap: {}: off", area.name);
    Ok(())
}

/// The pages that what is out on the area numbered `at` could come back
/// into: free memory, the page cache's, which is dropped for them, and the
/// free slots of the other areas, where others can go out to.
fn room(at: usize) -> u64 {
    let areas: [Option<AreaRef>; MAX_AREAS] = AREAS.lock().clone();
    let others: u64 = areas
        .iter()
        .enumerate()
        .filter(|&(other, _)| other != at)
        .filter_map(|(_, area)| area.as_ref())
        .map(|area| {
            let map = area.map.lock();
            if map.closing { 0 } else { area.slots - map.used }
        })
        .sum();
    FRAMES.free_pages() + FRAMES.owner_pages(Owner::PageCache) + others
}

/// Takes up to `want` free slots that follow one another in one area,
/// from the area of highest priority with room: the first and how many.
pub fn take(want: usize) -> Option<(u64, usize)> {
    let mut order: [Option<(i32, usize, AreaRef)>; MAX_AREAS] = [const { None }; MAX_AREAS];
    {
        let areas = AREAS.lock();
        for (at, area) in areas.iter().enumerate() {
            order[at] = area.as_ref().map(|area| (area.priority, at, area.clone()));
        }
    }
    // Highest priority first, and the first turned on of equals.
    order.sort_unstable_by_key(|entry| entry.as_ref().map(|(p, at, _)| (Reverse(*p), *at)));
    for (_, at, area) in order.iter().flatten() {
        let mut map = area.map.lock();
        if map.closing || map.used == area.slots {
            continue;
        }
        let len = map.counts.len();
        let start = map.next.clamp(1, len - 1);
        // From where the last search ended, round to it again.
        let mut first = None;
        for step in 0..len - 1 {
            let page = 1 + (start - 1 + step) % (len - 1);
            if map.counts[page] == 0 {
                first = Some(page);
                break;
            }
        }
        let Some(first) = first else { continue };
        let mut count = 0;
        while count < want && first + count < len && map.counts[first + count] == 0 {
            map.counts[first + count] = 1;
            count += 1;
        }
        map.used += count as u64;
        map.next = first + count;
        return Some(((*at as u64) << 32 | first as u64, count));
    }
    None
}

/// Lets go of one entry's hold on `slot`: the last frees it.
pub fn free(slot: u64) {
    let Some((area, page)) = area_of(slot) else { return };
    let mut map = area.map.lock();
    match map.counts.get(page).copied() {
        Some(0 | NOT_SLOT) | None => {
            drop(map);
            error!("Swap: slot {slot:#x} freed but not in use");
        }
        Some(1) => {
            map.counts[page] = 0;
            map.used -= 1;
        }
        Some(count) => map.counts[page] = count - 1,
    }
}

/// One more entry holds `slot` (a fork's): `false` if it is held by as
/// many as it counts.
pub fn share(slot: u64) -> bool {
    let Some((area, page)) = area_of(slot) else { return false };
    let mut map = area.map.lock();
    match map.counts.get(page).copied() {
        Some(count) if (1..MAX_SHARES).contains(&count) => {
            map.counts[page] = count + 1;
            true
        }
        _ => false,
    }
}

/// Writes `frames`, a page each, to the slots from `first` on, which
/// follow one another in one area.
pub fn write(first: u64, frames: &[u64]) -> Result<(), radix_block::Error> {
    let (area, page) = area_of(first).ok_or(radix_block::Error::OutOfRange)?;
    let mut parts = [(0u64, 0u32); BATCH];
    for (part, &frame) in parts.iter_mut().zip(frames) {
        *part = (frame, PAGE_SIZE as u32);
    }
    let parts = &parts[..frames.len().min(BATCH)];
    area.device.write_frames(page as u64 * SECTORS_PER_PAGE, parts)?;
    OUT.fetch_add(parts.len() as u64, Ordering::Relaxed);
    Ok(())
}

/// Reads `slot` into the frame at `phys`.
pub fn read(slot: u64, phys: u64) -> Result<(), radix_block::Error> {
    let (area, page) = area_of(slot).ok_or(radix_block::Error::OutOfRange)?;
    area.device.read_frames(page as u64 * SECTORS_PER_PAGE, &[(phys, PAGE_SIZE as u32)])?;
    IN.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// The pages the areas have to swap to, and those in use.
pub fn totals() -> (u64, u64) {
    let areas: [Option<AreaRef>; MAX_AREAS] = AREAS.lock().clone();
    areas
        .iter()
        .flatten()
        .fold((0, 0), |(slots, used), area| (slots + area.slots, used + area.map.lock().used))
}

/// `/processes/swaps`: each area on, as Linux's `/proc/swaps` lists them
/// (sizes in KiB).
pub fn list(text: &mut impl core::fmt::Write) -> core::fmt::Result {
    let areas: [Option<AreaRef>; MAX_AREAS] = AREAS.lock().clone();
    writeln!(text, "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority")?;
    for area in areas.iter().flatten() {
        let used = area.map.lock().used;
        let mut path = radix_klog::TextBuf::<32>::new();
        let _ = write!(path, "/devices/{}", area.name.as_str());
        writeln!(
            text,
            "{:<40}partition\t{}\t\t{}\t\t{}",
            path.as_str(),
            (area.pages - 1) * PAGE_SIZE as u64 / 1024,
            used * PAGE_SIZE as u64 / 1024,
            area.priority
        )?;
    }
    Ok(())
}

/// Starts the thread, at the first area turned on.
fn start_thread() {
    if THREAD.get().is_some() {
        return;
    }
    match sched::spawn("swap", run, 0, Options::new().policy(Policy::Fair { nice: 0 })) {
        Ok(thread) => {
            let _ = THREAD.set(thread);
        }
        Err(err) => error!("Swap: could not start the swap thread: {err}"),
    }
}

fn run(_: usize) {
    let asked = || ASKED.load(Ordering::Acquire) != DONE.load(Ordering::Acquire);
    loop {
        if on_any() {
            let next = sched::now_ns().saturating_add(INTERVAL_NS);
            wait::wait_until_or_timeout(asked_key(), "swap", next, asked);
        } else {
            // With no area on, asleep until one is turned on.
            wait::wait_until(asked_key(), "swap", || on_any() || asked());
        }
        let asked = ASKED.load(Ordering::Acquire);
        let (free, managed) = (FRAMES.free_pages(), FRAMES.managed_pages());
        let mut want = if free < managed / 32 { mark() - free } else { 0 };
        if asked != DONE.load(Ordering::Acquire) {
            want = mark().saturating_sub(free).max(RECLAIM_MIN);
        }
        if want > 0 && on_any() {
            crate::user::memory::swap_out(want);
        }
        DONE.store(asked, Ordering::Release);
        wait::wake_all(done_key());
    }
}

/// The free memory the swap thread swaps out to: 1/16 of RAM.
pub fn mark() -> u64 {
    FRAMES.managed_pages() / 16
}

/// Has the swap thread swap pages out until `pages` are free, for a
/// caller about to allocate them with locks held under which the thread
/// would pass its memory over: a fork, which copies its space's tables
/// with the space's regions locked. Stops at a pass that swaps nothing
/// out; nothing to do with no area on.
pub fn make_room(pages: u64) {
    for _ in 0..ROOM_PASSES {
        if !on_any() || FRAMES.free_pages() >= pages || !reclaim(ROOM_WAIT_NS) {
            return;
        }
    }
}

/// Asks the swap thread to swap pages out and waits for it, up to
/// `timeout_ns`: whether any went. `false` at once with no area on, and
/// for the swap thread itself, which cannot wait for itself.
pub fn reclaim(timeout_ns: u64) -> bool {
    let Some(thread) = THREAD.get() else { return false };
    if !on_any() || sched::current().is_some_and(|me| ThreadRef::ptr_eq(&me, thread)) {
        return false;
    }
    let before = OUT.load(Ordering::Acquire);
    let ticket = ASKED.fetch_add(1, Ordering::AcqRel) + 1;
    wait::wake_all(asked_key());
    let deadline = sched::now_ns().saturating_add(timeout_ns);
    wait::wait_until_or_timeout(done_key(), "swap out", deadline, || {
        DONE.load(Ordering::Acquire) >= ticket
    });
    OUT.load(Ordering::Acquire) > before
}
