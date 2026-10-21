// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Checks that count their failures, and print each.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use libradix::abi::Status;
use libradix::abi::errors::Kernel;
use libradix::println;

static FAILURES: AtomicU64 = AtomicU64::new(0);

/// How many checks failed so far.
pub fn failures() -> u64 {
    FAILURES.load(Ordering::Relaxed)
}

/// Checks that `ok` holds.
pub fn that(ok: bool, what: fmt::Arguments<'_>) -> bool {
    if !ok {
        FAILURES.fetch_add(1, Ordering::Relaxed);
        println!("FAIL: {what}");
    }
    ok
}

/// The name of a status, for messages.
pub struct Shown(pub Status);

impl fmt::Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match Kernel::from_status(self.0) {
            Some(kind) => write!(f, "{} ({})", kind.name(), self.0.detail()),
            None if self.0.is_ok() => f.write_str("ok"),
            None => write!(f, "{:#x}", self.0.0),
        }
    }
}

/// Checks that `result` succeeded, and gives its value.
pub fn ok<T>(result: Result<T, Status>, what: fmt::Arguments<'_>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(status) => {
            that(false, format_args!("{what}: {}", Shown(status)));
            None
        }
    }
}

/// Checks that `result` failed with `want`.
pub fn fails<T>(result: Result<T, Status>, want: Kernel, what: fmt::Arguments<'_>) -> bool {
    match result {
        Ok(_) => that(false, format_args!("{what}: succeeded, not {}", want.name())),
        Err(status) => that(
            Kernel::from_status(status) == Some(want),
            format_args!("{what}: {}, not {}", Shown(status), want.name()),
        ),
    }
}

/// Checks a condition, with a message in `format!` style.
#[macro_export]
macro_rules! check {
    ($ok:expr, $($what:tt)*) => {
        $crate::checks::that($ok, format_args!($($what)*))
    };
}

/// Checks that a result succeeded, giving its value as an `Option`.
#[macro_export]
macro_rules! ok {
    ($result:expr, $($what:tt)*) => {
        $crate::checks::ok($result, format_args!($($what)*))
    };
}

/// Checks that a result failed with a given error.
#[macro_export]
macro_rules! fails {
    ($result:expr, $want:ident, $($what:tt)*) => {
        $crate::checks::fails(
            $result,
            libradix::abi::errors::Kernel::$want,
            format_args!($($what)*),
        )
    };
}
