// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Byte vectors filled at the speed of a string store. The kernel's
//! `alloc` has no `resize` or `extend_from_slice`, which cannot fail (ADR
//! 0017), and pushing a byte at a time costs several instructions a byte:
//! a 4 KiB buffer took some 28,000 of them in `exec`.

#![allow(unsafe_code)]

use alloc::collections::TryReserveError;
use alloc::vec::Vec;
use core::alloc::Allocator;
use core::mem::MaybeUninit;

/// A vector of `len` zero bytes from `alloc`.
pub fn zeroed<A: Allocator>(len: usize, alloc: A) -> Result<Vec<u8, A>, TryReserveError> {
    let mut bytes = Vec::new_in(alloc);
    bytes.try_reserve_exact(len)?;
    bytes.spare_capacity_mut()[..len].fill(MaybeUninit::new(0));
    // SAFETY: there was room for `len` bytes, and each was just written.
    unsafe { bytes.set_len(len) };
    Ok(bytes)
}

/// Appends `more` to `bytes`, growing it if need be.
pub fn extend<A: Allocator>(bytes: &mut Vec<u8, A>, more: &[u8]) -> Result<(), TryReserveError> {
    bytes.try_reserve(more.len())?;
    let len = bytes.len();
    for (slot, &byte) in bytes.spare_capacity_mut().iter_mut().zip(more) {
        slot.write(byte);
    }
    // SAFETY: there was room for `more`, and each of its bytes was just
    // written after the `len` there were.
    unsafe { bytes.set_len(len + more.len()) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use super::*;

    #[test]
    fn zeroed_is_all_zeros() {
        for len in [0, 1, 7, 4096, 65537] {
            let bytes = zeroed(len, Global).unwrap();
            assert_eq!(bytes.len(), len);
            assert!(bytes.iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn extend_appends() {
        let mut bytes = zeroed(3, Global).unwrap();
        extend(&mut bytes, b"abc").unwrap();
        extend(&mut bytes, b"").unwrap();
        let long: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        extend(&mut bytes, &long).unwrap();
        assert_eq!(&bytes[..6], b"\0\0\0abc");
        assert_eq!(&bytes[6..], &long[..]);
    }

    #[test]
    fn too_much_is_an_error() {
        assert!(zeroed(usize::MAX, Global).is_err());
    }
}
