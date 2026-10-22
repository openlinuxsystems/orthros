// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Printing to the kernel log, through the kernel log capability every
//! program starts with (radix-kernel-log(2)). A line longer than the
//! kernel takes at once goes in pieces.

use core::fmt::{self, Write};

use radix_abi::kernel_log::WRITE_MAX;
use radix_abi::startup::SLOT_LOG;

use crate::SYS;

/// A line being put together.
pub struct Line {
    bytes: [u8; WRITE_MAX as usize],
    len: usize,
}

impl Line {
    /// An empty line.
    pub const fn new() -> Line {
        Line { bytes: [0; WRITE_MAX as usize], len: 0 }
    }

    /// Writes what it holds to the log, and empties it.
    pub fn flush(&mut self) {
        if self.len > 0 {
            let _ = radix_abi::syscalls::debug_log(&SYS, SLOT_LOG, &self.bytes[..self.len]);
            self.len = 0;
        }
    }
}

impl Default for Line {
    fn default() -> Line {
        Line::new()
    }
}

impl Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for &byte in text.as_bytes() {
            if self.len == self.bytes.len() {
                self.flush();
            }
            self.bytes[self.len] = byte;
            self.len += 1;
        }
        Ok(())
    }
}

/// Writes one line to the log.
pub fn print_line(args: fmt::Arguments<'_>) {
    let mut line = Line::new();
    let _ = line.write_fmt(args);
    line.flush();
}

/// Writes a formatted line to the kernel log.
#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {
        $crate::print::print_line(format_args!($($arg)*))
    };
}
