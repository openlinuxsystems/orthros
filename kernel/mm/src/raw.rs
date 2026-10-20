//! The memory manager's only raw memory accesses.
//!
//! The frame allocator and its free lists never touch the memory they
//! manage. The heap must: a slab keeps its free objects in a list threaded
//! through the objects, freed objects carry a poison word, memory is
//! zeroed on request, and per-CPU caches live in frames of their own. Each
//! of those goes through one of the functions below, whose callers state
//! why the address is theirs to use.

#![allow(unsafe_code)]

/// Fills `bytes` bytes at `addr` with zeros.
///
/// # Safety
///
/// `addr..addr + bytes` must be mapped writable and owned by the caller:
/// nothing else may read or write it meanwhile.
pub(crate) unsafe fn zero(addr: usize, bytes: usize) {
    // SAFETY: the caller guarantees the range is writable and exclusively
    // theirs.
    unsafe { core::ptr::write_bytes(addr as *mut u8, 0, bytes) };
}

/// Reads the word at `addr`.
///
/// # Safety
///
/// `addr` must be aligned to 8 bytes, mapped readable, and not written
/// concurrently.
pub(crate) unsafe fn read_word(addr: usize) -> usize {
    // SAFETY: the caller guarantees the word is aligned, readable and not
    // being written.
    unsafe { (addr as *const usize).read() }
}

/// Writes `value` to the word at `addr`.
///
/// # Safety
///
/// `addr` must be aligned to 8 bytes, mapped writable, and owned by the
/// caller: no reference to it may exist elsewhere.
pub(crate) unsafe fn write_word(addr: usize, value: usize) {
    // SAFETY: the caller guarantees the word is aligned, writable and
    // exclusively theirs.
    unsafe { (addr as *mut usize).write(value) };
}

/// Moves `value` to `addr` and returns a reference to it that lives as long
/// as the kernel.
///
/// # Safety
///
/// `addr` must be aligned for `T`, and `size_of::<T>()` bytes from it must
/// be mapped writable, owned by the caller, and never freed or used for
/// anything else afterwards.
pub(crate) unsafe fn place<T>(addr: usize, value: T) -> &'static T {
    let slot = addr as *mut T;
    // SAFETY: the caller guarantees the memory is aligned, writable,
    // exclusively theirs and never reused, so it can hold `value` for the
    // rest of the kernel's life and be shared from now on.
    unsafe {
        slot.write(value);
        &*slot
    }
}

/// Moves `value` to `addr` and returns the only reference to it.
///
/// # Safety
///
/// `addr` must be aligned for `T`, and `size_of::<T>()` bytes from it must
/// be mapped writable and owned by the caller until the reference is given
/// up (the heap turns it back into an address to free it).
pub(crate) unsafe fn place_mut<T>(addr: usize, value: T) -> &'static mut T {
    let slot = addr as *mut T;
    // SAFETY: the caller guarantees the memory is aligned, writable and
    // exclusively theirs for as long as the reference lives.
    unsafe {
        slot.write(value);
        &mut *slot
    }
}
