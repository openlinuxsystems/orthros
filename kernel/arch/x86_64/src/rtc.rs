// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The PC's real-time clock (the MC146818-compatible clock in CMOS RAM).
//!
//! [`init`] reads the date and time once at boot and sets the time of day
//! ([`WALL_CLOCK`]) from it; from then on the kernel's monotonic clock
//! keeps it, as timed(8) sets and slews it (ADR 0194). The clock is taken
//! to keep UTC, as on Linux machines that do not dual-boot Windows. The
//! registers are read until two reads in a row agree, outside an update
//! cycle, and decoded from BCD and the 12-hour format when the clock uses
//! them; [`write`] writes them in the same format, with the clock's
//! updates held meanwhile. One lock keeps reads and writes from mixing
//! their register numbers.

use core::sync::atomic::{AtomicU8, Ordering};

use radix_hal::clock::{self, WallClock};
use radix_sync::SpinLock;

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
/// Status B: updates are held, for the registers to be written.
const SET: u8 = 1 << 7;
/// Status B: values are binary, not BCD.
const BINARY: u8 = 1 << 2;
/// Status B: hours run 0 to 23.
const HOURS_24: u8 = 1 << 1;

/// The time of day.
pub static WALL_CLOCK: WallClock = WallClock::new();

/// The CMOS index of the century register, from the FADT (0 if none, for
/// years 2000 to 2099).
static CENTURY: AtomicU8 = AtomicU8::new(0);

/// Taken around each use of the index and data ports.
static CMOS: SpinLock<()> = SpinLock::new(());

fn read(register: u8) -> u8 {
    // SAFETY: selecting a CMOS register and reading it has no effect on
    // memory; only this module touches these ports, under CMOS.
    unsafe {
        io::outb(INDEX, register);
        io::inb(DATA)
    }
}

fn write_register(register: u8, value: u8) {
    // SAFETY: as in `read`; the registers written are the clock's date,
    // time and status B, which hold no memory addresses.
    unsafe {
        io::outb(INDEX, register);
        io::outb(DATA, value);
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

fn to_bcd(value: u8) -> u8 {
    (value / 10) << 4 | value % 10
}

/// The clock's date and time, as seconds since 1970, if it holds a valid
/// one. Call with CMOS held.
fn now_seconds() -> Option<u64> {
    let century = CENTURY.load(Ordering::Relaxed);
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
    clock::unix_seconds(
        full_year,
        u32::from(decode(month)),
        u32::from(decode(day)),
        u32::from(hour),
        u32::from(decode(minutes)),
        u32::from(decode(seconds)),
    )
}

/// Reads the clock and sets the time of day from it; `century` is the
/// CMOS index of its century register from the FADT (0 if none, for
/// years 2000 to 2099).
pub fn init(century: u8) {
    CENTURY.store(century, Ordering::Relaxed);
    let Some(seconds) = seconds() else {
        radix_klog::warn!("Time: the real-time clock holds no valid date");
        return;
    };
    WALL_CLOCK.set(time::now_ns(), seconds * 1_000_000_000);
    let (y, mo, d) = clock::civil_from_days((seconds / 86_400) as i64);
    let s = seconds % 86_400;
    radix_klog::info!(
        "Time: the real-time clock says {y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02} (taken as UTC)",
        s / 3600,
        s / 60 % 60,
        s % 60
    );
}

/// The clock read now, as seconds since 1970, if it holds a valid date.
pub fn seconds() -> Option<u64> {
    let _cmos = CMOS.lock();
    now_seconds()
}

/// Writes `seconds` since 1970 into the clock, in the format its status
/// register says it keeps: BCD or binary, 12 or 24 hours. Years it cannot
/// hold (before 2000 or after 2099 without a century register, or after
/// 9999) are not written. The clock counts on from what is written once
/// its updates are let go.
pub fn write(seconds: u64) -> bool {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let (year, month, day) = clock::civil_from_days(days);
    let century = CENTURY.load(Ordering::Relaxed);
    let fits = if century != 0 { (1970..=9999).contains(&year) } else { (2000..=2099).contains(&year) };
    if !fits {
        radix_klog::warn!("Time: the real-time clock cannot hold the year {year}");
        return false;
    }
    let of_day = seconds % 86_400;
    let (hour, minute, second) = ((of_day / 3600) as u8, (of_day / 60 % 60) as u8, (of_day % 60) as u8);
    let _cmos = CMOS.lock();
    let status = read(STATUS_B);
    let encode = |v: u8| if status & BINARY != 0 { v } else { to_bcd(v) };
    let hours = if status & HOURS_24 != 0 {
        encode(hour)
    } else {
        let twelve = match hour % 12 {
            0 => 12,
            h => h,
        };
        encode(twelve) | if hour >= 12 { 0x80 } else { 0 }
    };
    write_register(STATUS_B, status | SET);
    write_register(SECONDS, encode(second));
    write_register(MINUTES, encode(minute));
    write_register(HOURS, hours);
    write_register(DAY, encode(day as u8));
    write_register(MONTH, encode(month as u8));
    write_register(YEAR, encode((year % 100) as u8));
    if century != 0 {
        write_register(century, encode((year / 100) as u8));
    }
    write_register(STATUS_B, status & !SET);
    true
}
