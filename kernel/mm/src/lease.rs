// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Memory lent for as long as the borrower wants it, which comes back
//! when the borrower gives it back (ADR 0182).
//!
//! smoltcp keeps the buffers a socket is made with for as long as the
//! socket lives, as `&'a mut` slices. A program's socket lives as long as
//! the program keeps it, so the kernel lends them as `&'static mut`, and
//! their memory comes back only through a [`Lease`]: [`lend`] turns a
//! vector into the slice and the lease, which knows the allocation but
//! never touches it. [`Lease::end`] frees the memory once the borrower
//! hands back the very slice it was lent, and checks that it is that
//! slice: its address and its length.
//!
//! `end` is `unsafe`, because whether a borrower is done with a slice is
//! more than having it back. Every reference made from the slice must be
//! gone. None of them may be an argument of a call still running either,
//! inside a struct or not: Rust lets a function count on what its
//! arguments point to staying there until it returns, and Miri's borrow
//! models call freeing it then undefined. The slices come back as the
//! return values of the calls that take a socket apart, which keeps to
//! this. A lease dropped without its slice leaks the memory, which is
//! safe: the borrower may still have it.

#![allow(unsafe_code)]

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

/// An allocation [`lend`] lent: `capacity` places for values of `T`, of
/// which the first `len` were lent as a slice.
pub struct Lease<T, A: Allocator> {
    /// The allocation's start, with the provenance of the whole of it.
    /// Nothing reads or writes through it until the memory comes back.
    ptr: NonNull<T>,
    len: usize,
    capacity: usize,
    alloc: A,
    /// The values are the lease's to drop, once they come back.
    values: PhantomData<T>,
}

// SAFETY: nothing reaches the memory through a lease while it is lent, so
// a lease is a vector's allocator and its values, and moves to another
// thread as they may.
unsafe impl<T: Send, A: Allocator + Send> Send for Lease<T, A> {}
// SAFETY: a shared lease reaches neither the memory nor its allocator.
unsafe impl<T: Sync, A: Allocator + Sync> Sync for Lease<T, A> {}

/// Lends `values` for as long as the borrower wants: the slice of them,
/// and the lease that takes their memory back. The allocator lives
/// forever, as the slice may.
pub fn lend<T, A: Allocator + 'static>(values: Vec<T, A>) -> (Lease<T, A>, &'static mut [T]) {
    // Slices of a zero-sized type can be made from nothing, so their
    // address and length would not tell whose they are.
    const { assert!(size_of::<T>() != 0, "a lease lends memory, and zero-sized values have none") };
    let mut values = ManuallyDrop::new(values);
    let (len, capacity) = (values.len(), values.capacity());
    // A vector's pointer is never null, even with nothing allocated, and
    // `as_mut_ptr` gives it with the provenance of the whole buffer.
    let ptr = NonNull::new(values.as_mut_ptr()).unwrap_or(NonNull::dangling());
    // SAFETY: the vector is never used or dropped again, so its allocator
    // is moved out of it, once, as `Vec::into_raw_parts_with_alloc` does.
    let alloc = unsafe { core::ptr::read(values.allocator()) };
    // SAFETY: the first `len` values of the buffer are initialized, and
    // nothing else reaches them while they are lent: the vector is gone,
    // and the lease does not touch them.
    let slice = unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), len) };
    (Lease { ptr, len, capacity, alloc, values: PhantomData }, slice)
}

impl<T, A: Allocator> Lease<T, A> {
    /// How many values were lent.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the slice lent is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether `values` is where the slice lent is: its address and its
    /// length.
    pub fn lent(&self, values: *const [T]) -> bool {
        core::ptr::eq(values.cast::<T>(), self.ptr.as_ptr()) && values.len() == self.len
    }

    /// Ends the lease, given `values`, the slice [`lend`] lent: frees the
    /// memory, and drops its values.
    ///
    /// # Errors
    ///
    /// The lease, which still holds the memory, if `values` is not where
    /// the slice lent is ([`Lease::lent`]).
    ///
    /// # Safety
    ///
    /// `values` must come from the `&'static mut` slice `lend` lent, and
    /// neither that slice nor any reference made from it may be used
    /// again. Nor may any of them be an argument, or inside one, of a call
    /// that is still running: the slice must come to the caller as the
    /// return value of a call, as smoltcp gives a socket's buffers back,
    /// and not through the caller's own arguments.
    pub unsafe fn end(self, values: *mut [T]) -> Result<(), Lease<T, A>> {
        if !self.lent(values) {
            return Err(self);
        }
        let Lease { ptr, len, capacity, alloc, .. } = self;
        // SAFETY: `values` came from the slice `lend` made of this
        // allocation, the caller says, and it is where that slice is: so
        // only the lease can still reach the memory, and nothing counts on
        // it staying there. `ptr`, `len`, `capacity` and `alloc` are the
        // vector's own, as `from_raw_parts_in` asks.
        drop(unsafe { Vec::from_raw_parts_in(ptr.as_ptr(), len, capacity, alloc) });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::ptr;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::alloc::Global;

    use super::*;

    #[test]
    fn the_slice_lent_comes_back() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"lent");
        let (lease, slice) = lend(bytes);
        assert_eq!(lease.len(), 4);
        slice[0] = b'b';
        assert_eq!(slice, b"bent");
        assert!(lease.lent(slice));
        // SAFETY: `slice` is the slice lent, made in this test, and is not
        // used again.
        assert!(unsafe { lease.end(ptr::from_mut(slice)) }.is_ok());
    }

    #[test]
    fn another_slice_is_refused() {
        let (lease, slice) = lend(vec![0u8; 8]);
        let (other, other_slice) = lend(vec![0u8; 8]);
        let other_slice = ptr::from_mut(other_slice);
        // SAFETY: a slice that is not the one lent is refused before
        // anything is freed.
        let lease = unsafe { lease.end(other_slice) }.err().unwrap();
        // Part of the slice lent is not the slice, nor is more of it.
        assert!(!lease.lent(ptr::from_ref(&slice[..4])) && !lease.lent(ptr::from_ref(&slice[4..])));
        assert!(!lease.lent(ptr::slice_from_raw_parts(slice.as_ptr(), 9)));
        // SAFETY: each is the slice its lease lent, made in this test, and
        // neither is used again.
        unsafe {
            assert!(lease.end(ptr::from_mut(slice)).is_ok());
            assert!(other.end(other_slice).is_ok());
        }
    }

    #[test]
    fn values_drop_when_the_memory_comes_back() {
        struct Counted(Arc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let mut values = Vec::with_capacity_in(8, Global);
        for _ in 0..3 {
            values.push(Counted(drops.clone()));
        }
        let (lease, slice) = lend(values);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        // SAFETY: `slice` is the slice lent, made in this test, and is not
        // used again.
        assert!(unsafe { lease.end(ptr::from_mut(slice)) }.is_ok());
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn an_empty_vector_lends_an_empty_slice() {
        let (lease, slice) = lend(Vec::<u64>::new());
        assert!(lease.is_empty() && slice.is_empty());
        // SAFETY: `slice` is the slice lent, made in this test, and is not
        // used again.
        assert!(unsafe { lease.end(ptr::from_mut(slice)) }.is_ok());
    }
}
