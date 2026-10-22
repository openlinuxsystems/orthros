// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The PC's real-time clock (the MC146818-compatible clock in CMOS RAM).
//!
//! [`init`] reads the date and time once at boot; from then on the wall
//! clock is that time plus the kernel's monotonic clock. The clock is taken
//! to keep UTC, as on Linux machines that do not dual-boot Windows; time
//! synchronisation (Phase 12) corrects it. The registers are read until two
//! reads in a row agree, outside an update cycle, and decoded from BCD and
//! the 12-hour format when the clock uses them.

use core::sync::atomic::{AtomicU64, Ordering};

use radix_hal::clock;

use crate::{io, time};

const INDEX: u16 = 0x70;
const DATA: u16 = 0x71;
const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0a;
const STATUS_B: u8 = 0x0b;
/// Status A: an update cycle is under way.
const UPDATING: u8 = 1 << 7;
/// Status B: values are binary, not BCD.
const BINARY: u8 = 1 << 2;
/// Status B: hours run 0 to 23.
const HOURS_24: u8 = 1 << 1;

/// Nanoseconds since 1970 at the kernel clock's 0, or 0 if unknown.
static OFFSET_NS: AtomicU64 = AtomicU64::new(0);

fn read(register: u8) -> u8 {
    // SAFETY: selecting a CMOS register and reading it has no effect on
    // memory; only this module touches these ports, during boot.
    unsafe {
        io::outb(INDEX, register);
        io::inb(DATA)
    }
}

fn raw(century: u8) -> [u8; 7] {
    let mut spins = 0;
    while read(STATUS_A) & UPDATING != 0 && spins < 1_000_000 {
        spins += 1;
    }
    let century = if century != 0 { read(century) } else { 0 };
    [read(SECONDS), read(MINUTES), read(HOURS), read(DAY), read(MONTH), read(YEAR), century]
}

fn bcd(value: u8) -> u8 {
    (value >> 4) * 10 + (value & 0xf)
}

/// Reads the clock; `century` is the CMOS index of its century register
/// from the FADT (0 if none, for years 2000 to 2099).
pub fn init(century: u8) {
    let mut values = raw(century);
    for _ in 0..8 {
        let again = raw(century);
        if again == values {
            break;
        }
        values = again;
    }
    let status = read(STATUS_B);
    let [seconds, minutes, hours, day, month, year, century_value] = values;
    let decode = |v: u8| if status & BINARY != 0 { v } else { bcd(v) };
    let pm = status & HOURS_24 == 0 && hours & 0x80 != 0;
    let mut hour = decode(hours & 0x7f);
    if status & HOURS_24 == 0 {
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    let full_year = if century != 0 && century_value != 0 {
        i64::from(decode(century_value)) * 100 + i64::from(decode(year))
    } else {
        2000 + i64::from(decode(year))
    };
    let Some(seconds) = clock::unix_seconds(
        full_year,
        u32::from(decode(month)),
        u32::from(decode(day)),
        u32::from(hour),
        u32::from(decode(minutes)),
        u32::from(decode(seconds)),
    ) else {
        radix_klog::warn!("Time: the real-time clock holds no valid date");
        return;
    };
    let since_boot = time::now_ns();
    OFFSET_NS.store((seconds * 1_000_000_000).saturating_sub(since_boot).max(1), Ordering::Relaxed);
    let (y, mo, d) = clock::civil_from_days((seconds / 86_400) as i64);
    let s = seconds % 86_400;
    radix_klog::info!(
        "Time: the real-time clock says {y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02} (taken as UTC)",
        s / 3600,
        s / 60 % 60,
        s % 60
    );
}

/// Nanoseconds since 1970, if the clock was read.
pub fn wall_clock_ns() -> Option<u64> {
    match OFFSET_NS.load(Ordering::Relaxed) {
        0 => None,
        offset => Some(offset + time::now_ns()),
    }
}
