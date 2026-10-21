// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The kernel heap: slab caches with per-CPU magazines.
//!
//! Requests up to 8 KiB are served from the size classes of
//! [`crate::class`]: each class has a cache of slabs ([`crate::slab`]) and,
//! in front of it, a magazine layer ([`crate::magazine`]) so that most
//! allocations and frees stay on the CPU making them, without a shared
//! lock. Larger requests take a block of frames of their own.
//!
//! Every call states its context with [`Flags`] (spec 3.4), and every
//! failure is an [`AllocError`] the caller handles. The kernel's global
//! allocator, [`ExplicitOnly`], refuses all requests, so code that uses
//! `alloc` collections must pass an allocator carrying flags:
//! `Box::try_new_in(value, HEAP.with(Flags::MAY_SLEEP))`.
//!
//! # Locks
//!
//! The heap takes its locks in one order: a CPU's cache, then a class's
//! depot, then a class's slab cache, then a frame allocator zone, and holds
//! them with interrupts disabled. It never calls the frame allocator's
//! reclaim hook while holding one: a new slab is allocated with no lock
//! held, and the heap's own bookkeeping (magazines, per-CPU caches) is
//! allocated without reclaim. So [`Heap::drain`], the reclaim hook, can
//! take every lock.

#![allow(unsafe_code)]

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use radix_sync::{Once, SpinLock};

use crate::class::{self, CLASSES, Class};
use crate::context::{AllocError, Context, Flags};
use crate::frames::Frames;
use crate::magazine::{Depot, Loaded, MAGAZINE_BYTES, Magazine};
use crate::page::{Owner, PAGE_SHIFT, PAGE_SIZE};
use crate::platform::{MAX_CPUS, NoInterrupts, Platform};
use crate::raw;
use crate::slab::{self, Corruption, CorruptionKind, Env, SlabCache};

/// The class magazines are allocated from.
const MAGAZINE_CLASS: usize = 11;
// In statement position: Kani's `assert!` ends with a semicolon.
const _: () = {
    assert!(class::SIZES[MAGAZINE_CLASS] as usize == MAGAZINE_BYTES);
};
/// Full magazines a depot keeps before returning objects to the slabs.
const DEPOT_FULL: usize = 32;
/// Empty magazines a depot keeps before freeing them.
const DEPOT_EMPTY: usize = 32;

/// Flags for the heap's own bookkeeping: no reserves, no zeroing.
const BOOKKEEPING: Flags = Flags::MAY_SLEEP;

/// One CPU's magazines, in a frame of its own on the CPU's node.
struct CpuCache {
    classes: SpinLock<[Loaded; CLASSES]>,
}

const _: () = {
    assert!(size_of::<CpuCache>() <= PAGE_SIZE);
};

/// A snapshot of one size class, for reports.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClassStats {
    /// Object size.
    pub size: usize,
    /// Slabs the class holds.
    pub slabs: u64,
    /// Bytes in those slabs.
    pub slab_bytes: u64,
    /// Objects in use by callers.
    pub in_use: u64,
    /// Free objects cached in magazines.
    pub cached: u64,
    /// Slabs set aside after damage was found.
    pub quarantined: u64,
}

/// A snapshot of the whole heap.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeapStats {
    /// Objects in use by callers, in all classes.
    pub objects: u64,
    /// Bytes those objects take (rounded up to their classes).
    pub object_bytes: u64,
    /// Bytes of slabs.
    pub slab_bytes: u64,
    /// Free objects cached in magazines.
    pub cached: u64,
    /// Magazines (the heap's own bookkeeping, in the 256-byte class).
    pub magazines: u64,
    /// CPUs with a cache.
    pub cpus: u64,
    /// Blocks of frames allocated for large requests.
    pub large_blocks: u64,
    /// Bytes in those blocks.
    pub large_bytes: u64,
    /// Requests that failed.
    pub failures: u64,
    /// Damage found.
    pub corruptions: u64,
}

/// The kernel heap.
pub struct Heap {
    frames: &'static Frames,
    caches: [SpinLock<SlabCache>; CLASSES],
    depots: [SpinLock<Depot>; CLASSES],
    cpus: [Once<&'static CpuCache>; MAX_CPUS],
    magazines: AtomicU64,
    large_blocks: AtomicU64,
    large_pages: AtomicU64,
    failures: AtomicU64,
    corruptions: AtomicU64,
    on_corruption: Once<fn(&Corruption)>,
}

impl Heap {
    /// A heap taking its memory from `frames`. It works once the frame
    /// allocator has a platform and memory.
    pub const fn new(frames: &'static Frames) -> Heap {
        Heap {
            frames,
            caches: [const { SpinLock::new(SlabCache::new()) }; CLASSES],
            depots: [const { SpinLock::new(Depot::new()) }; CLASSES],
            cpus: [const { Once::new() }; MAX_CPUS],
            magazines: AtomicU64::new(0),
            large_blocks: AtomicU64::new(0),
            large_pages: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            corruptions: AtomicU64::new(0),
            on_corruption: Once::new(),
        }
    }

    /// The frame allocator the heap uses.
    pub fn frames(&self) -> &'static Frames {
        self.frames
    }

    /// Registers the function told about heap damage (the kernel panics).
    /// Without one, the heap counts the damage, sets the damaged object
    /// aside and goes on.
    pub fn set_corruption_handler(&self, handler: fn(&Corruption)) {
        let _ = self.on_corruption.set(handler);
    }

    /// An [`core::alloc::Allocator`] that allocates from this heap with
    /// `flags`.
    pub const fn with(&'static self, flags: Flags) -> Kmalloc {
        Kmalloc { heap: self, flags }
    }

    fn env(&self) -> Option<(&'static dyn Platform, Env<'static>)> {
        let platform = self.frames.platform()?;
        Some((platform, Env { frames: self.frames, direct_map: platform.direct_map() }))
    }

    fn report(&self, corruption: Corruption) {
        self.corruptions.fetch_add(1, Ordering::Relaxed);
        if let Some(handler) = self.on_corruption.get() {
            handler(&corruption);
        }
    }

    /// Allocates memory for `layout`.
    pub fn alloc(&self, layout: Layout, flags: Flags) -> Result<NonNull<u8>, AllocError> {
        let result = self.alloc_inner(layout, flags);
        if result.is_err() {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    fn alloc_inner(&self, layout: Layout, flags: Flags) -> Result<NonNull<u8>, AllocError> {
        let (platform, env) = self.env().ok_or(AllocError)?;
        let Some(class) = Class::for_layout(layout.size(), layout.align()) else {
            return self.alloc_large(env, layout, flags);
        };
        let addr = match self.take_from_magazines(platform, class) {
            Some(addr) => addr,
            None => self.take_from_slabs(env, class, flags, true)?,
        };
        if flags.zero {
            // SAFETY: the object is ours: it was just taken from a free list
            // or magazine, and the slab is mapped by the direct map.
            unsafe { raw::zero(addr, layout.size().max(16)) };
        } else {
            // Clear the poison word, so freeing an object nobody wrote to
            // is not taken for a double free.
            // SAFETY: as above.
            unsafe { raw::write_word(addr + 8, 0) };
        }
        NonNull::new(addr as *mut u8).ok_or(AllocError)
    }

    /// Takes an object from this CPU's magazines or the depot.
    fn take_from_magazines(&self, platform: &'static dyn Platform, class: Class) -> Option<usize> {
        let irqs = NoInterrupts::new(Some(platform));
        let cpu = self.cpu_cache(platform)?;
        let mut classes = cpu.classes.lock();
        let slot = &mut classes[class.index()];
        let addr = match slot.pop() {
            Some(addr) => addr,
            None => {
                let mut depot = self.depots[class.index()].lock();
                let full = depot.full.pop()?;
                // Both of the CPU's magazines are empty (or missing).
                let empty = slot.previous.take();
                slot.previous = slot.loaded.replace(full);
                let surplus = match empty {
                    Some(empty) if depot.empty.len() < DEPOT_EMPTY => {
                        depot.empty.push(empty);
                        None
                    }
                    other => other,
                };
                drop(depot);
                if let Some(magazine) = surplus {
                    self.free_magazine(magazine);
                }
                slot.pop()?
            }
        };
        // SAFETY: objects in magazines are free objects of their class,
        // owned by the magazine we just took this one from.
        let checked = unsafe { slab::check_free(class, addr) };
        drop(classes);
        drop(irqs);
        match checked {
            Ok(_) => Some(addr),
            Err(corruption) => {
                // Leave the object out of circulation.
                self.report(corruption);
                None
            }
        }
    }

    /// Takes an object from the class's slabs, adding a slab if needed.
    fn take_from_slabs(
        &self,
        env: Env<'static>,
        class: Class,
        flags: Flags,
        reclaim: bool,
    ) -> Result<usize, AllocError> {
        for _ in 0..4 {
            let taken = {
                let _irqs = NoInterrupts::new(self.frames.platform());
                self.caches[class.index()].lock().take(env, class)
            };
            match taken {
                Ok(Some(addr)) => return Ok(addr),
                Ok(None) => {}
                Err(corruption) => {
                    self.report(corruption);
                    continue;
                }
            }
            let slab = Flags { zero: false, ..flags };
            let phys = self.frames.alloc_inner(
                class.slab_order(),
                slab,
                Owner::Slab,
                class.index() as u8,
                reclaim && flags.context == Context::MaySleep,
            )?;
            let pfn = phys >> PAGE_SHIFT;
            // SAFETY: the frames were just allocated for this slab.
            unsafe { slab::format(env, class, pfn) };
            let _irqs = NoInterrupts::new(self.frames.platform());
            self.caches[class.index()].lock().add(env, pfn);
        }
        Err(AllocError)
    }

    fn alloc_large(
        &self,
        env: Env<'static>,
        layout: Layout,
        flags: Flags,
    ) -> Result<NonNull<u8>, AllocError> {
        let order = class::large_order(layout.size(), layout.align()).ok_or(AllocError)?;
        let phys = self.frames.alloc(order, flags, Owner::Heap)?;
        self.large_blocks.fetch_add(1, Ordering::Relaxed);
        self.large_pages.fetch_add(1 << order, Ordering::Relaxed);
        NonNull::new((env.direct_map + phys as usize) as *mut u8).ok_or(AllocError)
    }

    /// Frees memory that [`Heap::alloc`] returned.
    ///
    /// The heap checks what it can: a pointer that is not an object of
    /// `layout`'s class, or an object that is already free, is reported as
    /// damage and left alone.
    ///
    /// # Safety
    ///
    /// `ptr` must have come from this heap with the same `layout`, and must
    /// not be used afterwards.
    pub unsafe fn free(&self, ptr: NonNull<u8>, layout: Layout) {
        let Some((platform, env)) = self.env() else { return };
        let addr = ptr.as_ptr() as usize;
        let Some(class) = Class::for_layout(layout.size(), layout.align()) else {
            return self.free_large(env, addr, layout);
        };
        let Some(location) = env.locate(class, addr) else {
            let size = class.size();
            return self.report(Corruption {
                kind: CorruptionKind::BadPointer,
                address: addr,
                size,
            });
        };
        // SAFETY: `locate` found the object in a slab of this heap, and the
        // caller is giving it up.
        if unsafe { slab::looks_free(addr) } {
            let size = class.size();
            return self.report(Corruption {
                kind: CorruptionKind::DoubleFree,
                address: addr,
                size,
            });
        }
        // SAFETY: as above.
        unsafe { slab::mark_free(addr, u16::MAX) };
        if !self.to_magazines(platform, env, class, addr) {
            self.to_slabs(env, class, addr, location);
        }
    }

    /// Puts a free object in this CPU's magazines. Returns `false` when it
    /// must go back to its slab instead.
    fn to_magazines(
        &self,
        platform: &'static dyn Platform,
        env: Env<'static>,
        class: Class,
        addr: usize,
    ) -> bool {
        let _irqs = NoInterrupts::new(Some(platform));
        let Some(cpu) = self.cpu_cache(platform) else { return false };
        let mut classes = cpu.classes.lock();
        let slot = &mut classes[class.index()];
        let capacity = class.magazine_rounds();
        let Err(addr) = slot.push(addr, capacity) else { return true };
        // Both magazines are full (or missing): swap in an empty one.
        let empty = self.depots[class.index()].lock().empty.pop();
        let Some(empty) = empty.or_else(|| self.new_magazine(env)) else { return false };
        let full = slot.previous.take();
        slot.previous = slot.loaded.replace(empty);
        let pushed = slot.push(addr, capacity).is_ok();
        if let Some(full) = full {
            let mut depot = self.depots[class.index()].lock();
            if depot.full.len() < DEPOT_FULL {
                depot.full.push(full);
            } else {
                drop(depot);
                self.flush(env, class, full);
            }
        }
        pushed
    }

    /// Returns an object to its slab.
    fn to_slabs(
        &self,
        env: Env<'static>,
        class: Class,
        addr: usize,
        location: (u64, &crate::page::PageInfo, usize),
    ) {
        let given = {
            let _irqs = NoInterrupts::new(self.frames.platform());
            self.caches[class.index()].lock().give(env, class, addr, location)
        };
        match given {
            Ok(Some(pfn)) => self.release_slab(class, pfn),
            Ok(None) => {}
            Err(corruption) => self.report(corruption),
        }
    }

    fn release_slab(&self, class: Class, pfn: u64) {
        if self.frames.free(pfn << PAGE_SHIFT, class.slab_order(), Owner::Slab).is_err() {
            // The slab's record changed under us: damage.
            self.report(Corruption {
                kind: CorruptionKind::Bookkeeping,
                address: pfn as usize,
                size: class.size(),
            });
        }
    }

    /// Returns a magazine's objects to their slabs, then frees the
    /// magazine.
    fn flush(&self, env: Env<'static>, class: Class, magazine: &'static mut Magazine) {
        while let Some(addr) = magazine.pop() {
            match env.locate(class, addr) {
                Some(location) => self.to_slabs(env, class, addr, location),
                None => self.report(Corruption {
                    kind: CorruptionKind::BadPointer,
                    address: addr,
                    size: class.size(),
                }),
            }
        }
        self.free_magazine(magazine);
    }

    fn new_magazine(&self, env: Env<'static>) -> Option<&'static mut Magazine> {
        let class = Class::from_index(MAGAZINE_CLASS)?;
        let addr = self.take_from_slabs(env, class, BOOKKEEPING, false).ok()?;
        self.magazines.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the object was just taken from a slab of the 256-byte
        // class, which is aligned to 256 bytes, and nothing else uses it.
        Some(unsafe { raw::place_mut(addr, Magazine::new()) })
    }

    fn free_magazine(&self, magazine: &'static mut Magazine) {
        let Some((_, env)) = self.env() else { return };
        let Some(class) = Class::from_index(MAGAZINE_CLASS) else { return };
        let addr = core::ptr::from_mut(magazine).addr();
        if let Some(location) = env.locate(class, addr) {
            self.magazines.fetch_sub(1, Ordering::Relaxed);
            // SAFETY: the magazine is an object of the 256-byte class, and
            // giving up its only reference gives it back to the heap.
            unsafe { slab::mark_free(addr, u16::MAX) };
            self.to_slabs(env, class, addr, location);
        }
    }

    fn free_large(&self, env: Env<'static>, addr: usize, layout: Layout) {
        let Some(order) = class::large_order(layout.size(), layout.align()) else {
            let size = layout.size();
            return self.report(Corruption {
                kind: CorruptionKind::BadPointer,
                address: addr,
                size,
            });
        };
        let phys = addr.wrapping_sub(env.direct_map) as u64;
        match self.frames.free(phys, order, Owner::Heap) {
            Ok(()) => {
                self.large_blocks.fetch_sub(1, Ordering::Relaxed);
                self.large_pages.fetch_sub(1 << order, Ordering::Relaxed);
            }
            Err(error) => self.report(Corruption {
                kind: if error == crate::buddy::FreeError::AlreadyFree {
                    CorruptionKind::DoubleFree
                } else {
                    CorruptionKind::BadPointer
                },
                address: addr,
                size: PAGE_SIZE << order,
            }),
        }
    }

    /// This CPU's cache, set up on first use. Call with interrupts
    /// disabled.
    fn cpu_cache(&self, platform: &'static dyn Platform) -> Option<&'static CpuCache> {
        let cpu = platform.current_cpu();
        let slot = self.cpus.get(cpu)?;
        if let Some(cache) = slot.get() {
            return Some(cache);
        }
        let node = platform.cpu_node(cpu).min(u8::MAX.into()) as u8;
        let flags = BOOKKEEPING.prefer_node(node);
        let phys = self.frames.alloc_inner(0, flags, Owner::PerCpu, 0, false).ok()?;
        let addr = platform.direct_map() + phys as usize;
        let empty = CpuCache { classes: SpinLock::new([const { Loaded::new() }; CLASSES]) };
        // SAFETY: the frame was just allocated for this cache, is mapped by
        // the direct map and is never freed: a CPU's cache lives as long as
        // the kernel.
        let cache = unsafe { raw::place(addr, empty) };
        // Only this CPU sets its slot, with interrupts disabled.
        slot.set(cache).ok().copied()
    }

    /// Returns every cached object to its slab and every empty slab to the
    /// frame allocator. The frame allocator calls it before failing a
    /// `MaySleep` allocation; tests call it to check for leaks.
    pub fn drain(&self) {
        let Some((platform, env)) = self.env() else { return };
        for slot in &self.cpus {
            let Some(cpu) = slot.get() else { continue };
            for class in Class::all() {
                let _irqs = NoInterrupts::new(Some(platform));
                let mut classes = cpu.classes.lock();
                let slot = &mut classes[class.index()];
                for magazine in [slot.loaded.take(), slot.previous.take()].into_iter().flatten() {
                    self.flush(env, class, magazine);
                }
            }
        }
        for class in Class::all() {
            loop {
                let magazine = {
                    let _irqs = NoInterrupts::new(Some(platform));
                    let mut depot = self.depots[class.index()].lock();
                    depot.full.pop().or_else(|| depot.empty.pop())
                };
                match magazine {
                    Some(magazine) => self.flush(env, class, magazine),
                    None => break,
                }
            }
        }
        // Magazines go back to the 256-byte class, so release empty slabs
        // last.
        for class in Class::all() {
            loop {
                let slab = {
                    let _irqs = NoInterrupts::new(Some(platform));
                    self.caches[class.index()].lock().take_empty(env)
                };
                match slab {
                    Some(pfn) => self.release_slab(class, pfn),
                    None => break,
                }
            }
        }
    }

    /// Calls `f` with a snapshot of every class that holds slabs.
    pub fn class_stats(&self, mut f: impl FnMut(&ClassStats)) {
        let platform = self.frames.platform();
        let mut cached = [0u64; CLASSES];
        for cpu in self.cpus.iter().filter_map(Once::get) {
            let _irqs = NoInterrupts::new(platform);
            let classes = cpu.classes.lock();
            for (count, slot) in cached.iter_mut().zip(classes.iter()) {
                *count += slot.len() as u64;
            }
        }
        for (count, depot) in cached.iter_mut().zip(&self.depots) {
            let _irqs = NoInterrupts::new(platform);
            *count += depot_objects(&depot.lock());
        }
        for class in Class::all() {
            let (slabs, in_use, quarantined) = {
                let _irqs = NoInterrupts::new(platform);
                let cache = self.caches[class.index()].lock();
                (cache.slabs, cache.in_use, cache.quarantined)
            };
            if slabs == 0 && quarantined == 0 {
                continue;
            }
            let cached = cached[class.index()];
            f(&ClassStats {
                size: class.size(),
                slabs,
                slab_bytes: slabs * class.slab_bytes() as u64,
                in_use: in_use.saturating_sub(cached),
                cached,
                quarantined,
            });
        }
    }

    /// A snapshot of the heap.
    pub fn stats(&self) -> HeapStats {
        let mut stats = HeapStats::default();
        let magazines = self.magazines.load(Ordering::Relaxed);
        self.class_stats(|class| {
            stats.objects += class.in_use;
            stats.object_bytes += class.in_use * class.size as u64;
            stats.slab_bytes += class.slab_bytes;
            stats.cached += class.cached;
        });
        // Magazines are objects of the heap, not of its callers.
        stats.objects = stats.objects.saturating_sub(magazines);
        stats.object_bytes = stats.object_bytes.saturating_sub(magazines * MAGAZINE_BYTES as u64);
        stats.magazines = magazines;
        stats.cpus = self.cpus.iter().filter(|slot| slot.get().is_some()).count() as u64;
        stats.large_blocks = self.large_blocks.load(Ordering::Relaxed);
        stats.large_bytes = self.large_pages.load(Ordering::Relaxed) * PAGE_SIZE as u64;
        stats.failures = self.failures.load(Ordering::Relaxed);
        stats.corruptions = self.corruptions.load(Ordering::Relaxed);
        stats
    }
}

/// Objects held in a depot's full magazines.
fn depot_objects(depot: &Depot) -> u64 {
    depot.full.iter().map(|m| m.len() as u64).sum()
}

/// The heap with fixed flags, as a [`core::alloc::Allocator`] for `alloc`
/// collections.
#[derive(Clone, Copy)]
pub struct Kmalloc {
    #[cfg_attr(kani, allow(dead_code))]
    heap: &'static Heap,
    flags: Flags,
}

impl Kmalloc {
    /// The flags its allocations use.
    pub fn flags(&self) -> Flags {
        self.flags
    }
}

/// A well-aligned dangling pointer for zero-sized allocations.
#[cfg(not(kani))]
fn dangling(layout: Layout) -> NonNull<u8> {
    NonNull::new(core::ptr::without_provenance_mut(layout.align())).unwrap_or(NonNull::dangling())
}

// SAFETY: memory from `allocate` stays valid until `deallocate`, because the
// heap hands each object to one caller at a time; copies of a `Kmalloc` use
// the same heap, so memory from one can be freed through another.
#[cfg(not(kani))]
unsafe impl core::alloc::Allocator for Kmalloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, core::alloc::AllocError> {
        if layout.size() == 0 {
            return Ok(NonNull::slice_from_raw_parts(dangling(layout), 0));
        }
        let ptr = self.heap.alloc(layout, self.flags).map_err(|_| core::alloc::AllocError)?;
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, core::alloc::AllocError> {
        if layout.size() == 0 {
            return Ok(NonNull::slice_from_raw_parts(dangling(layout), 0));
        }
        let ptr =
            self.heap.alloc(layout, self.flags.zeroed()).map_err(|_| core::alloc::AllocError)?;
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() != 0 {
            // SAFETY: the caller guarantees `ptr` came from `allocate` with
            // `layout`, which took it from this heap.
            unsafe { self.heap.free(ptr, layout) };
        }
    }
}

/// The kernel's global allocator: it refuses every request, so that every
/// allocation names its context through [`Heap::with`].
pub struct ExplicitOnly;

// SAFETY: `alloc` never returns memory, so there is nothing to keep valid.
unsafe impl GlobalAlloc for ExplicitOnly {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use proptest::prelude::*;

    use super::*;
    use crate::tests::{become_cpu, heap_with};

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    /// Frames the heap holds once drained: one per CPU cache.
    fn assert_drained(heap: &Heap) {
        heap.drain();
        let frames = heap.frames();
        let stats = heap.stats();
        assert_eq!((stats.objects, stats.cached, stats.magazines), (0, 0, 0), "{stats:?}");
        assert_eq!((stats.slab_bytes, stats.large_blocks, stats.corruptions), (0, 0, 0));
        assert_eq!(frames.owner_pages(Owner::Slab), 0);
        assert_eq!(frames.owner_pages(Owner::Heap), 0);
        assert_eq!(frames.free_pages() + frames.owner_pages(Owner::PerCpu), frames.managed_pages());
        frames.check().unwrap();
    }

    #[test]
    fn small_and_large_objects_round_trip() {
        let heap = heap_with(1024, 1);
        let mut objects = Vec::new();
        for (size, align) in
            [(1, 1), (24, 8), (100, 64), (3000, 8), (8192, 4096), (9000, 8), (1 << 20, 4096)]
        {
            let layout = layout(size, align);
            let ptr = heap.alloc(layout, Flags::MAY_SLEEP.zeroed()).unwrap();
            assert!(ptr.as_ptr().addr().is_multiple_of(align));
            // SAFETY: the heap just gave us `size` zeroed bytes.
            let bytes = unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), size) };
            assert!(bytes.iter().all(|&b| b == 0));
            bytes.fill(0xa5);
            objects.push((ptr, layout));
        }
        let stats = heap.stats();
        assert_eq!((stats.objects, stats.large_blocks), (5, 2));
        for (ptr, layout) in objects {
            // SAFETY: allocated above with this layout.
            unsafe { heap.free(ptr, layout) };
        }
        assert_eq!(heap.stats().objects, 0);
        // The small objects wait in this CPU's magazines.
        assert_eq!(heap.stats().cached, 5);
        assert_drained(heap);
    }

    #[test]
    fn damage_is_found_and_contained() {
        let heap = heap_with(256, 1);
        let small = layout(64, 8);
        let a = heap.alloc(small, Flags::ATOMIC).unwrap();
        // SAFETY: allocated above.
        unsafe { heap.free(a, small) };
        // A double free is noticed and ignored.
        // SAFETY: deliberately wrong, to test the check: the object is free
        // but still mapped and owned by the heap.
        unsafe { heap.free(a, small) };
        assert_eq!(heap.stats().corruptions, 1);
        // A pointer into the middle of an object, or freed with the wrong
        // size, is not an object.
        let b = heap.alloc(small, Flags::ATOMIC).unwrap();
        assert_eq!(b, a, "magazines hand back the last object freed");
        // SAFETY: deliberately wrong, as above; the checks reject it before
        // writing anything.
        unsafe { heap.free(b.byte_add(8), small) };
        // SAFETY: as above.
        unsafe { heap.free(b, layout(4000, 8)) };
        assert_eq!(heap.stats().corruptions, 3);
        // A write after free is caught when the object is handed out again.
        // SAFETY: allocated above with this layout.
        unsafe { heap.free(b, small) };
        // SAFETY: deliberately wrong: the object is free, but its memory
        // still belongs to the heap, so the write lands nowhere else.
        unsafe { b.byte_add(8).cast::<u64>().write(0x1234) };
        let c = heap.alloc(small, Flags::ATOMIC).unwrap();
        assert_ne!(c, b);
        assert_eq!(heap.stats().corruptions, 4);
        // SAFETY: allocated above with this layout.
        unsafe { heap.free(c, small) };
    }

    static RECLAIM_HEAP: OnceLock<&'static Heap> = OnceLock::new();

    #[test]
    fn may_sleep_allocations_reclaim_cached_memory() {
        let heap = *RECLAIM_HEAP.get_or_init(|| heap_with(512, 1));
        heap.frames().set_reclaim(|| RECLAIM_HEAP.get().unwrap().drain());
        // Fill memory with 8 KiB objects, then free them all: nearly all
        // stay cached in magazines, keeping their slabs.
        let small = layout(8192, 8);
        let mut objects = Vec::new();
        while let Ok(ptr) = heap.alloc(small, Flags::MAY_SLEEP) {
            objects.push(ptr);
        }
        assert!(objects.len() > 200);
        for ptr in objects {
            // SAFETY: allocated above with this layout.
            unsafe { heap.free(ptr, small) };
        }
        assert!(heap.stats().slab_bytes > 1 << 20);
        // An atomic caller cannot wait for reclaim; a sleeping one can.
        let big = layout(1 << 20, 4096);
        assert!(heap.alloc(big, Flags::ATOMIC).is_err());
        let ptr = heap.alloc(big, Flags::MAY_SLEEP).unwrap();
        // SAFETY: allocated above with this layout.
        unsafe { heap.free(ptr, big) };
        assert_drained(heap);
    }

    /// The Phase 2 exit check on the host: eight CPUs on two nodes run
    /// 40 million random operations, about 20 million allocations and as
    /// many frees.
    #[test]
    fn cpus_stress_the_heap_without_leaks() {
        let heap = heap_with(8192, 2);
        let threads: Vec<_> = (0..8)
            .map(|cpu| {
                std::thread::spawn(move || {
                    become_cpu(cpu);
                    crate::stress::run(heap, cpu as u64 + 1, 5_000_000)
                })
            })
            .collect();
        let mut total = crate::stress::StressReport::default();
        for thread in threads {
            total.add(&thread.join().unwrap());
        }
        assert_eq!((total.mismatches, total.failures), (0, 0), "{total:?}");
        assert_eq!(total.allocations, total.frees);
        assert!(total.allocations > 15_000_000, "{total:?}");
        assert_drained(heap);
    }

    proptest! {
        /// Live objects never overlap, keep their alignment, and nothing
        /// is left over once everything is freed.
        #[test]
        fn live_objects_are_disjoint(
            ops in proptest::collection::vec((0usize..12_000, 0u32..13, any::<bool>(), 0usize..4), 1..150),
        ) {
            let heap = heap_with(512, 1);
            let mut live: Vec<(NonNull<u8>, Layout)> = Vec::new();
            for (size, shift, alloc, cpu) in ops {
                become_cpu(cpu);
                if alloc || live.is_empty() {
                    let layout = layout(size.max(1), 1 << shift);
                    let Ok(ptr) = heap.alloc(layout, Flags::ATOMIC) else { continue };
                    let start = ptr.as_ptr().addr();
                    prop_assert!(start.is_multiple_of(layout.align()));
                    for (other, other_layout) in &live {
                        let other = other.as_ptr().addr();
                        prop_assert!(start + layout.size() <= other || other + other_layout.size() <= start);
                    }
                    live.push((ptr, layout));
                } else {
                    let (ptr, layout) = live.swap_remove(size % live.len());
                    // SAFETY: allocated above with this layout.
                    unsafe { heap.free(ptr, layout) };
                }
            }
            for (ptr, layout) in live {
                // SAFETY: allocated above with this layout.
                unsafe { heap.free(ptr, layout) };
            }
            become_cpu(0);
            assert_drained(heap);
        }
    }
}
