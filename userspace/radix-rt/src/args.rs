// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The program's arguments, from its start-up page: the first is the
//! path it was started from (radix-startup(2)).

use libradix::startup;
use radix_abi::address_space::PAGE_SIZE;
use radix_abi::startup::STARTUP_ADDRESS;

/// The program's arguments, in order.
pub fn args() -> Args {
    let start = startup();
    let (offset, size) = (start.args_offset as u64, start.args_size as u64);
    let bytes: &'static [u8] = if offset.checked_add(size).is_some_and(|end| end <= PAGE_SIZE) {
        // SAFETY: the start-up page is mapped read-only for the program's
        // whole life, and the bytes lie inside it.
        unsafe {
            core::slice::from_raw_parts((STARTUP_ADDRESS + offset) as *const u8, size as usize)
        }
    } else {
        &[]
    };
    Args { rest: bytes }
}

/// An iterator over the arguments. An argument that is not UTF-8 is
/// given as `?`.
#[derive(Clone)]
pub struct Args {
    rest: &'static [u8],
}

impl Iterator for Args {
    type Item = &'static str;

    fn next(&mut self) -> Option<&'static str> {
        if self.rest.is_empty() {
            return None;
        }
        let end = self.rest.iter().position(|&b| b == 0).unwrap_or(self.rest.len());
        let (arg, rest) = self.rest.split_at(end);
        self.rest = rest.get(1..).unwrap_or(&[]);
        Some(core::str::from_utf8(arg).unwrap_or("?"))
    }
}
