// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Memory for the storage compartments: the heap and pages.
//!
//! In the kernel, collections live on the kernel heap with the
//! `MAY_SLEEP` flag (storage code runs in threads, never in interrupt
//! handlers), and pages come from the function the kernel registers with
//! [`set_page_source`], which takes them from the frame allocator. With
//! the `host` feature, for the tests on the host, collections use the
//! host's allocator and pages are host memory ([`Page::fake`]).
//!
//! The kernel's `alloc` has no infallible APIs (ADR 0017), so the helpers
//! here try and report [`NoMemory`] instead of aborting.

// The kernel heap is what the kernel uses; on the host and under Kani
// it is not.
#[cfg(any(test, kani, feature = "host"))]
use radix_mm as _;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use radix_hw::Page;

/// The allocator of the storage compartments' collections.
#[cfg(not(any(test, kani, feature = "host")))]
pub type Alloc = radix_mm::Kmalloc;
/// The allocator of the storage compartments' collections.
#[cfg(any(test, kani, feature = "host"))]
pub type Alloc = alloc::alloc::Global;

/// The allocator, to pass to `new_in`.
#[cfg(not(any(test, kani, feature = "host")))]
pub fn alloc() -> Alloc {
    radix_mm::HEAP.with(radix_mm::Flags::MAY_SLEEP)
}

/// The allocator, to pass to `new_in`.
#[cfg(any(test, kani, feature = "host"))]
pub fn alloc() -> Alloc {
    alloc::alloc::Global
}

/// A vector on the compartments' heap.
pub type KVec<T> = Vec<T, Alloc>;
/// A box on the compartments' heap.
pub type KBox<T> = Box<T, Alloc>;
/// A shared value on the compartments' heap.
pub type KArc<T> = Arc<T, Alloc>;

/// The heap or the page allocator had no memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoMemory;

/// An empty vector.
pub fn vec<T>() -> KVec<T> {
    Vec::new_in(alloc())
}

/// An empty vector with room for `capacity` items.
pub fn vec_with_capacity<T>(capacity: usize) -> Result<KVec<T>, NoMemory> {
    let mut v = vec();
    v.try_reserve_exact(capacity).map_err(|_| NoMemory)?;
    Ok(v)
}

/// Appends `value` to `v`, growing it if need be.
pub fn push<T>(v: &mut KVec<T>, value: T) -> Result<(), NoMemory> {
    v.try_reserve(1).map_err(|_| NoMemory)?;
    // Room was made above.
    v.push_within_capacity(value).map(|_| ()).map_err(|_| NoMemory)
}

/// Inserts `value` at `at` in `v`, moving what follows along (the
/// kernel's `alloc` has no `insert`, which cannot fail).
pub fn insert<T>(v: &mut KVec<T>, at: usize, value: T) -> Result<(), NoMemory> {
    push(v, value)?;
    v[at..].rotate_right(1);
    Ok(())
}

/// A copy of `bytes` on the heap.
pub fn copy(bytes: &[u8]) -> Result<KVec<u8>, NoMemory> {
    let mut v = vec_with_capacity(bytes.len())?;
    extend(&mut v, bytes)?;
    Ok(v)
}

/// Appends `bytes` to `v`, growing it if need be (the kernel's `alloc`
/// has no `extend_from_slice`, which cannot fail): as a string store does
/// ([`radix_mm::bytes::extend`]), not a byte at a time.
#[cfg(not(kani))]
pub fn extend(v: &mut KVec<u8>, bytes: &[u8]) -> Result<(), NoMemory> {
    radix_mm::bytes::extend(v, bytes).map_err(|_| NoMemory)
}

/// Appends `bytes` to `v`, growing it if need be.
#[cfg(kani)]
pub fn extend(v: &mut KVec<u8>, bytes: &[u8]) -> Result<(), NoMemory> {
    v.try_reserve(bytes.len()).map_err(|_| NoMemory)?;
    for &byte in bytes {
        // Room was made above.
        let _ = v.push_within_capacity(byte);
    }
    Ok(())
}

/// `value` in a box.
pub fn boxed<T>(value: T) -> Result<KBox<T>, NoMemory> {
    Box::try_new_in(value, alloc()).map_err(|_| NoMemory)
}

/// `value`, shared.
pub fn arc<T>(value: T) -> Result<KArc<T>, NoMemory> {
    Arc::try_new_in(value, alloc()).map_err(|_| NoMemory)
}

/// Where pages come from in the kernel.
static PAGE_SOURCE: radix_sync::Once<fn() -> Option<Page>> = radix_sync::Once::new();
/// Pages the caches hold, for statistics.
static PAGES: AtomicU64 = AtomicU64::new(0);

/// Registers the kernel's page allocator, once: a function that returns a
/// zeroed page, or `None` when memory runs out.
pub fn set_page_source(source: fn() -> Option<Page>) {
    let _ = PAGE_SOURCE.set(source);
}

/// A page a cache holds: counted while it lives.
pub struct CachePage(Page);

impl CachePage {
    /// A zeroed page.
    pub fn new() -> Result<CachePage, NoMemory> {
        #[cfg(any(test, feature = "host"))]
        let page = Some(Page::fake());
        #[cfg(not(any(test, feature = "host")))]
        let page = PAGE_SOURCE.get().and_then(|source| source());
        let page = page.ok_or(NoMemory)?;
        PAGES.fetch_add(1, Ordering::Relaxed);
        Ok(CachePage(page))
    }
}

impl core::ops::Deref for CachePage {
    type Target = Page;

    fn deref(&self) -> &Page {
        &self.0
    }
}

impl core::ops::DerefMut for CachePage {
    fn deref_mut(&mut self) -> &mut Page {
        &mut self.0
    }
}

impl Drop for CachePage {
    fn drop(&mut self) {
        PAGES.fetch_sub(1, Ordering::Relaxed);
    }
}

/// How many pages the caches hold.
pub fn pages() -> u64 {
    PAGES.load(Ordering::Relaxed)
}
