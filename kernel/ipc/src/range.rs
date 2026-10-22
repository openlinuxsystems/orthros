// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Address ranges a program passes to the kernel.

use radix_abi::address_space::{PAGE_SIZE, USER_START, USER_TOP};

/// Whether the `len` bytes from `address` lie below USER_TOP, without the
/// sum overflowing. An empty range is always fine: nothing is touched.
/// The kernel checks this before copying from or to a program, so a
/// pointer into the kernel's half never reaches a copy.
pub const fn user_bytes(address: u64, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    match address.checked_add(len) {
        Some(end) => end <= USER_TOP,
        None => false,
    }
}

/// The end of `pages` pages from `address`, if `address` is page-aligned,
/// `pages` is not 0 and every page lies between USER_START and USER_TOP.
pub const fn user_pages(address: u64, pages: u64) -> Option<u64> {
    if pages == 0 || !address.is_multiple_of(PAGE_SIZE) || address < USER_START {
        return None;
    }
    let Some(len) = pages.checked_mul(PAGE_SIZE) else { return None };
    let Some(end) = address.checked_add(len) else { return None };
    if end <= USER_TOP { Some(end) } else { None }
}
