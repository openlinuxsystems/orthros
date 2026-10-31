// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! HID reports for the Radix kernel (Phase 11a, ADR 0148): what a
//! keyboard, mouse or tablet says in its reports, as key events and
//! pointer motion.
//!
//! A HID device describes its reports in a report descriptor (*Device
//! Class Definition for Human Interface Devices 1.11*, 6.2.2): items that
//! set a global and a local state, and main items, each of which lays out
//! a field of `count` values of `size` bits in an Input, Output or
//! Feature report, named by usages (*HID Usage Tables 1.5*). A field is
//! variable, each value one usage's, or an array, each value naming the
//! usage that is on.
//!
//! - [`Layout::parse`] reads a report descriptor into the fields of its
//!   Input reports and the lock lights of its Output reports. It checks
//!   every item against the bytes there are, and a descriptor that
//!   describes more than it keeps is refused, not cut short.
//! - [`Decoder`] turns each Input report into what changed: keys going
//!   down and up (radix-input(2)'s key codes, Linux's, [`key`]), with the
//!   modifiers first, and the pointer's movement, wheels, buttons and,
//!   for a tablet, where it points.
//! - [`BOOT_KEYBOARD`] and [`BOOT_MOUSE`] describe the boot protocol's
//!   fixed reports, for a device whose own descriptor cannot be read.
//!
//! The crate knows nothing of USB or of the kernel, so HID over I2C can
//! use it too, and its tests run on the host.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![feature(vec_push_within_capacity)]

extern crate alloc;

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use radix_abi::input::{LED_CAPS_LOCK, LED_NUM_LOCK, LED_SCROLL_LOCK, POSITION_MAX};

mod keys;
#[cfg(test)]
mod tests;

pub use keys::key;

/// A usage: its page in the high 16 bits and its ID in the low 16, as a
/// four-byte Usage item writes it (HID 1.11, 6.2.2.8).
pub type Usage = u32;

/// The usage pages the decoder reads (HID Usage Tables 1.5, chapter 3).
pub mod page {
    /// Generic Desktop: pointers, their axes and wheels, system keys.
    pub const GENERIC_DESKTOP: u16 = 0x01;
    /// Keyboard/Keypad.
    pub const KEYBOARD: u16 = 0x07;
    /// LEDs.
    pub const LED: u16 = 0x08;
    /// Buttons, numbered from 1.
    pub const BUTTON: u16 = 0x09;
    /// Consumer: media and application keys, the horizontal wheel.
    pub const CONSUMER: u16 = 0x0c;
    /// Digitizers: touchpads, touchscreens and pens.
    pub const DIGITIZER: u16 = 0x0d;
}

/// The usages the decoder reads.
pub mod usage {
    use super::Usage;

    /// A pointer.
    pub const POINTER: Usage = 0x0001_0001;
    /// A mouse.
    pub const MOUSE: Usage = 0x0001_0002;
    /// A keyboard.
    pub const KEYBOARD: Usage = 0x0001_0006;
    /// A keypad.
    pub const KEYPAD: Usage = 0x0001_0007;
    /// The X axis: right is more.
    pub const X: Usage = 0x0001_0030;
    /// The Y axis: down is more.
    pub const Y: Usage = 0x0001_0031;
    /// The wheel: away from the user is more.
    pub const WHEEL: Usage = 0x0001_0038;
    /// The system keys: power, sleep and wake.
    pub const SYSTEM_CONTROL: Usage = 0x0001_0080;
    /// What a keyboard reports with too many keys down to say which.
    pub const ERROR_ROLL_OVER: Usage = 0x0007_0001;
    /// The Num Lock light.
    pub const NUM_LOCK: Usage = 0x0008_0001;
    /// The Caps Lock light.
    pub const CAPS_LOCK: Usage = 0x0008_0002;
    /// The Scroll Lock light.
    pub const SCROLL_LOCK: Usage = 0x0008_0003;
    /// The media and application keys.
    pub const CONSUMER_CONTROL: Usage = 0x000c_0001;
    /// The horizontal wheel (AC Pan): right is more.
    pub const AC_PAN: Usage = 0x000c_0238;
    /// A touchscreen.
    pub const TOUCH_SCREEN: Usage = 0x000d_0004;
    /// A touchpad, which reports its fingers when told to and is a mouse
    /// until then.
    pub const TOUCH_PAD: Usage = 0x000d_0005;
}

/// A main item's flags that the decoder reads (HID 1.11, 6.2.2.5).
pub mod flag {
    /// Constant: padding, which holds nothing.
    pub const CONSTANT: u16 = 1 << 0;
    /// Variable, not an array.
    pub const VARIABLE: u16 = 1 << 1;
    /// Relative to the last report, not absolute.
    pub const RELATIVE: u16 = 1 << 2;
}

/// The most Input fields a layout keeps.
pub const MAX_FIELDS: usize = 64;
/// The most usage ranges its Input fields name, together.
pub const MAX_USAGES: usize = 256;
/// The most report IDs it uses.
pub const MAX_REPORTS: usize = 32;
/// The most usage ranges one main item names; those after are dropped.
const ITEM_USAGES: usize = 48;
/// The most lock lights a layout keeps.
const MAX_LIGHTS: usize = 8;
/// The most top-level collections a layout names.
const MAX_APPLICATIONS: usize = 4;
/// How deep Push can go.
const STACK: usize = 4;
/// The most usages of a range the parser looks through for keys and
/// pointer usages.
const RANGE_LOOK: u32 = 1024;

/// Why a report descriptor was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// An item runs past its end.
    Truncated,
    /// It has more Input fields than a layout keeps.
    TooManyFields,
    /// Its Input fields name more usages than a layout keeps.
    TooManyUsages,
    /// A report is longer than 8 KiB.
    TooLong,
    /// It uses report ID 0, which is reserved, or one past 255.
    BadReportId,
    /// Its Push and Pop do not match.
    Stack,
    /// It describes no Input field the decoder can read.
    NoInputs,
    /// There was no memory for its layout.
    NoMemory,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Truncated => "its report descriptor ends inside an item",
            Error::TooManyFields => "its report descriptor has more than 64 input fields",
            Error::TooManyUsages => "its report descriptor names more than 256 usage ranges",
            Error::TooLong => "it has a report longer than 8 KiB, or more than 32 reports",
            Error::BadReportId => "its report descriptor uses a reserved report ID",
            Error::Stack => "its report descriptor's Push and Pop do not match",
            Error::NoInputs => "its report descriptor describes no input",
            Error::NoMemory => "there was no memory for its report layout",
        })
    }
}

/// An Input field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Field {
    /// The report it is in: its ID, or 0 when the device numbers none.
    pub report: u8,
    /// Where it starts, in bits from the start of the report's data
    /// (after the ID).
    pub offset: u32,
    /// Bits a value, 1 to 32.
    pub size: u8,
    /// Values.
    pub count: u16,
    /// Its main item's flags.
    pub flags: u16,
    /// The least and the most a value may be; an array's values outside
    /// this name no usage.
    pub logical_min: i32,
    /// The most.
    pub logical_max: i32,
    /// Its usages: `usage_count` ranges from `first_usage` in the
    /// layout's.
    first_usage: u16,
    usage_count: u16,
    /// Whether one of its usages is a key.
    keys: bool,
    /// Whether one of its usages is a pointer's axis, wheel or button.
    pointer: bool,
}

/// A range of usages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Range {
    first: Usage,
    last: Usage,
}

impl Range {
    fn len(self) -> u32 {
        self.last - self.first + 1
    }
}

/// A lock light: the bit of an Output report that lights it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Light {
    report: u8,
    /// In bits from the start of the report's data.
    offset: u32,
    /// LED_* bit of radix-input(2).
    led: u32,
}

/// How long the Input and Output reports of a report ID are, in bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Report {
    id: u8,
    input: u16,
    output: u16,
}

/// What a report descriptor says of the reports the decoder reads, in
/// memory from `A`.
#[derive(Debug)]
pub struct Layout<A: Allocator> {
    fields: Vec<Field, A>,
    usages: Vec<Range, A>,
    reports: Vec<Report, A>,
    lights: Vec<Light, A>,
    /// Its top-level collections' usages.
    applications: [Usage; MAX_APPLICATIONS],
    application_count: usize,
    /// Whether its reports start with their ID.
    numbered: bool,
}

/// Pushes `value` onto `v`, if it has fewer than `max`; `full` if not.
fn push<T, A: Allocator>(
    v: &mut Vec<T, A>,
    value: T,
    max: usize,
    full: Error,
) -> Result<(), Error> {
    if v.len() >= max {
        return Err(full);
    }
    v.try_reserve(1).map_err(|_| Error::NoMemory)?;
    v.push_within_capacity(value).map(|_| ()).map_err(|_| Error::NoMemory)
}

/// The global items' state (HID 1.11, 6.2.2.7).
#[derive(Clone, Copy, Debug, Default)]
struct Globals {
    page: u16,
    logical_min: i32,
    logical_max: i32,
    size: u32,
    count: u32,
    report: u8,
}

/// A usage range of a local item, and whether its page is still to be
/// settled: a one- or two-byte Usage takes the Usage Page in effect, which
/// a descriptor may change after it, before the main item.
#[derive(Clone, Copy, Debug, Default)]
struct Local {
    range: Range,
    short: bool,
}

/// The local items' state (HID 1.11, 6.2.2.8), which each main item ends.
struct Locals {
    usages: [Local; ITEM_USAGES],
    count: usize,
    /// A Usage Minimum, waiting for its Usage Maximum.
    minimum: Option<(Usage, bool)>,
}

impl Locals {
    const fn new() -> Locals {
        Locals {
            usages: [Local { range: Range { first: 0, last: 0 }, short: false }; ITEM_USAGES],
            count: 0,
            minimum: None,
        }
    }

    fn push(&mut self, first: Usage, last: Usage, short: bool) {
        if first <= last && first >> 16 == last >> 16 && self.count < ITEM_USAGES {
            self.usages[self.count] = Local { range: Range { first, last }, short };
            self.count += 1;
        }
    }

    /// Gives the usages written without a page the one in effect at the
    /// main item, back from the last, up to one already on it, as Linux
    /// does: so a Usage Page item between the usages and the main item
    /// applies to those after the last on the old page.
    fn settle(&mut self, page: u16) {
        let page = u32::from(page) << 16;
        for local in self.usages[..self.count].iter_mut().rev().filter(|l| l.short) {
            if local.range.first & 0xffff_0000 == page {
                break;
            }
            local.range.first = page | (local.range.first & 0xffff);
            local.range.last = page | (local.range.last & 0xffff);
        }
    }

    /// Its `index`th usage, counting through the ranges.
    fn usage(&self, mut index: u32) -> Option<Usage> {
        for local in &self.usages[..self.count] {
            if index < local.range.len() {
                return Some(local.range.first + index);
            }
            index -= local.range.len();
        }
        None
    }

    fn ranges(&self) -> impl Iterator<Item = Range> + '_ {
        self.usages[..self.count].iter().map(|l| l.range)
    }
}

impl<A: Allocator + Clone> Layout<A> {
    /// Reads the report descriptor `bytes`, keeping its layout in memory
    /// from `alloc`.
    pub fn parse(bytes: &[u8], alloc: A) -> Result<Layout<A>, Error> {
        let mut layout = Layout {
            fields: Vec::new_in(alloc.clone()),
            usages: Vec::new_in(alloc.clone()),
            reports: Vec::new_in(alloc.clone()),
            lights: Vec::new_in(alloc),
            applications: [0; MAX_APPLICATIONS],
            application_count: 0,
            numbered: false,
        };
        layout.read(bytes)?;
        Ok(layout)
    }

    fn read(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let mut globals = Globals::default();
        let mut stack = [Globals::default(); STACK];
        let mut pushed = 0;
        let mut locals = Locals::new();
        let mut depth = 0usize;
        let mut at = 0;
        while at < bytes.len() {
            let prefix = bytes[at];
            if prefix == 0xfe {
                // A long item (6.2.2.3): no tag of one is defined, so it
                // is skipped.
                let size = usize::from(*bytes.get(at + 1).ok_or(Error::Truncated)?);
                at += 3 + size;
                if at > bytes.len() {
                    return Err(Error::Truncated);
                }
                continue;
            }
            let size = [0, 1, 2, 4][usize::from(prefix & 3)];
            let data = bytes.get(at + 1..at + 1 + size).ok_or(Error::Truncated)?;
            at += 1 + size;
            let mut word = [0u8; 4];
            word[..size].copy_from_slice(data);
            let unsigned = u32::from_le_bytes(word);
            let signed = match size {
                1 => i32::from(unsigned as u8 as i8),
                2 => i32::from(unsigned as u16 as i16),
                _ => unsigned as i32,
            };
            let tag = prefix >> 4;
            match (prefix >> 2) & 3 {
                // Main.
                0 => {
                    locals.settle(globals.page);
                    match tag {
                        0x8 => self.input(&globals, &locals, unsigned as u16)?,
                        0x9 => self.output(&globals, &locals, unsigned as u16)?,
                        0xa => {
                            if depth == 0
                                && self.application_count < MAX_APPLICATIONS
                                && let Some(usage) = locals.usage(0)
                            {
                                self.applications[self.application_count] = usage;
                                self.application_count += 1;
                            }
                            depth += 1;
                        }
                        0xc => depth = depth.saturating_sub(1),
                        // Feature, and the reserved tags.
                        _ => {}
                    }
                    locals = Locals::new();
                }
                // Global.
                1 => match tag {
                    0x0 => globals.page = unsigned as u16,
                    0x1 => globals.logical_min = signed,
                    // A maximum is read unsigned when the minimum is not
                    // negative, as many descriptors write 255 in one byte.
                    0x2 => {
                        globals.logical_max =
                            if globals.logical_min < 0 { signed } else { unsigned as i32 };
                    }
                    0x7 => globals.size = unsigned,
                    0x8 => {
                        globals.report = u8::try_from(unsigned)
                            .ok()
                            .filter(|&id| id != 0)
                            .ok_or(Error::BadReportId)?;
                        self.numbered = true;
                    }
                    0x9 => globals.count = unsigned,
                    0xa => {
                        *stack.get_mut(pushed).ok_or(Error::Stack)? = globals;
                        pushed += 1;
                    }
                    0xb => {
                        pushed = pushed.checked_sub(1).ok_or(Error::Stack)?;
                        globals = stack[pushed];
                    }
                    // Physical extents and units: nothing the decoder uses.
                    _ => {}
                },
                // Local.
                2 => {
                    let full = |id: u32| {
                        if size == 4 { id } else { u32::from(globals.page) << 16 | (id & 0xffff) }
                    };
                    match tag {
                        0x0 => locals.push(full(unsigned), full(unsigned), size < 4),
                        0x1 => locals.minimum = Some((full(unsigned), size < 4)),
                        0x2 => {
                            let (first, short) = locals
                                .minimum
                                .take()
                                .unwrap_or((u32::from(globals.page) << 16, size < 4));
                            locals.push(first, full(unsigned), short && size < 4);
                        }
                        // Designators, strings and delimiters.
                        _ => {}
                    }
                }
                // Reserved.
                _ => {}
            }
        }
        if self.fields.is_empty() {
            return Err(Error::NoInputs);
        }
        Ok(())
    }

    /// The lengths of report `id`'s reports, kept from its first item.
    fn report(&mut self, id: u8) -> Result<&mut Report, Error> {
        let at = match self.reports.iter().position(|r| r.id == id) {
            Some(at) => at,
            None => {
                let report = Report { id, ..Report::default() };
                push(&mut self.reports, report, MAX_REPORTS, Error::TooLong)?;
                self.reports.len() - 1
            }
        };
        Ok(&mut self.reports[at])
    }

    /// An Input item.
    fn input(&mut self, globals: &Globals, locals: &Locals, flags: u16) -> Result<(), Error> {
        let bits = bits(globals)?;
        let report = self.report(globals.report)?;
        let offset = u32::from(report.input);
        report.input = u16::try_from(offset + bits).map_err(|_| Error::TooLong)?;
        if flags & flag::CONSTANT != 0
            || !(1..=32).contains(&globals.size)
            || globals.count == 0
            || locals.count == 0
        {
            return Ok(());
        }
        let first_usage = self.usages.len();
        let (mut keys, mut pointer) = (false, false);
        for range in locals.ranges() {
            push(&mut self.usages, range, MAX_USAGES, Error::TooManyUsages)?;
            for usage in (range.first..=range.last).take(RANGE_LOOK as usize) {
                keys |= key(usage).is_some();
                pointer |= matches!(usage, usage::X | usage::Y | usage::WHEEL | usage::AC_PAN)
                    || usage >> 16 == u32::from(page::BUTTON);
            }
        }
        let field = Field {
            report: globals.report,
            offset,
            size: globals.size as u8,
            count: globals.count as u16,
            flags,
            logical_min: globals.logical_min,
            logical_max: globals.logical_max,
            first_usage: first_usage as u16,
            usage_count: locals.count as u16,
            keys,
            pointer,
        };
        push(&mut self.fields, field, MAX_FIELDS, Error::TooManyFields)
    }

    /// An Output item: keeps the lock lights among its one-bit values.
    fn output(&mut self, globals: &Globals, locals: &Locals, flags: u16) -> Result<(), Error> {
        let bits = bits(globals)?;
        let report = self.report(globals.report)?;
        let offset = u32::from(report.output);
        report.output = u16::try_from(offset + bits).map_err(|_| Error::TooLong)?;
        if flags & (flag::CONSTANT | flag::VARIABLE) != flag::VARIABLE || globals.size != 1 {
            return Ok(());
        }
        for i in 0..globals.count.min(32) {
            let led = match locals.usage(i) {
                Some(usage::NUM_LOCK) => LED_NUM_LOCK,
                Some(usage::CAPS_LOCK) => LED_CAPS_LOCK,
                Some(usage::SCROLL_LOCK) => LED_SCROLL_LOCK,
                _ => continue,
            };
            let light = Light { report: globals.report, offset: offset + i, led };
            // Lights past the eighth are not lit.
            let _ = push(&mut self.lights, light, MAX_LIGHTS, Error::TooManyFields);
        }
        Ok(())
    }

    /// Its Input fields.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// The usage of value `index` of `field`: a variable field's `index`th
    /// value, or what an array's value `index` (less its minimum) names.
    pub fn usage(&self, field: &Field, mut index: u32) -> Option<Usage> {
        let ranges = self.ranges(field);
        for range in ranges {
            if index < range.len() {
                return Some(range.first + index);
            }
            index -= range.len();
        }
        None
    }

    /// The usage ranges of `field`.
    fn ranges(&self, field: &Field) -> &[Range] {
        let first = usize::from(field.first_usage);
        self.usages.get(first..first + usize::from(field.usage_count)).unwrap_or(&[])
    }

    /// The last usage of `field`, which a variable field's values past its
    /// usages take (6.2.2.8).
    fn last_usage(&self, field: &Field) -> Option<Usage> {
        self.ranges(field).last().map(|r| r.last)
    }

    /// Whether its reports start with their ID.
    pub fn numbered(&self) -> bool {
        self.numbered
    }

    /// Whether it reports keys of a keyboard, not only media or system
    /// keys.
    pub fn is_keyboard(&self) -> bool {
        self.fields().iter().any(|f| {
            f.keys && self.ranges(f).iter().any(|r| r.first >> 16 == u32::from(page::KEYBOARD))
        })
    }

    /// Whether it reports keys of any kind.
    pub fn has_keys(&self) -> bool {
        self.fields().iter().any(|f| f.keys)
    }

    /// Whether it reports a pointer.
    pub fn is_pointer(&self) -> bool {
        self.fields().iter().any(|f| f.pointer)
    }

    /// Whether its pointer says where it points, as a tablet's does, not
    /// how far it moved.
    pub fn is_absolute(&self) -> bool {
        self.fields().iter().any(|f| {
            f.pointer
                && f.flags & flag::RELATIVE == 0
                && (0..u32::from(f.count))
                    .any(|i| matches!(self.usage(f, i), Some(usage::X | usage::Y)))
        })
    }

    /// Whether it has lock lights.
    pub fn has_lights(&self) -> bool {
        !self.lights.is_empty()
    }

    /// Its top-level collections' usages.
    pub fn applications(&self) -> &[Usage] {
        &self.applications[..self.application_count]
    }

    /// The most bytes an Input report of it takes, with its ID.
    pub fn input_bytes(&self) -> usize {
        let bits = self.reports.iter().map(|r| r.input).max().unwrap_or(0);
        usize::from(bits).div_ceil(8) + usize::from(self.numbered)
    }

    /// Writes into `out` the Output report that lights the lights of
    /// `leds` (LED_* bits of radix-input(2)) and puts the others out.
    /// Returns its report ID and length with the ID, or None if it has no
    /// lights or `out` is too short.
    pub fn lights_report(&self, leds: u32, out: &mut [u8]) -> Option<(u8, usize)> {
        let report = self.lights.first()?.report;
        let skip = usize::from(self.numbered);
        let bits = self.reports.iter().find(|r| r.id == report).map_or(0, |r| r.output);
        let len = usize::from(bits).div_ceil(8) + skip;
        let out = out.get_mut(..len)?;
        out.fill(0);
        if self.numbered {
            out[0] = report;
        }
        for light in self.lights.iter().filter(|l| l.report == report) {
            if leds & light.led != 0 {
                let bit = light.offset as usize;
                out[skip + bit / 8] |= 1 << (bit % 8);
            }
        }
        Some((report, len))
    }
}

/// The bits of a main item of `globals`, checked.
fn bits(globals: &Globals) -> Result<u32, Error> {
    globals.size.checked_mul(globals.count).filter(|&b| b <= 0xffff).ok_or(Error::TooLong)
}

/// The value of `size` bits at bit `at` of `data`, sign-extended if
/// `signed`; None past its end, for a short report.
fn value(data: &[u8], at: u32, size: u8, signed: bool) -> Option<i32> {
    let (at, size) = (at as usize, u32::from(size));
    let end = at + size as usize;
    if end > data.len() * 8 || size == 0 {
        return None;
    }
    let mut bits = 0u64;
    for (k, &byte) in data[at / 8..end.div_ceil(8)].iter().enumerate() {
        bits |= u64::from(byte) << (8 * k);
    }
    bits = (bits >> (at % 8)) & ((1u64 << size) - 1);
    if signed && size < 64 && bits & (1 << (size - 1)) != 0 {
        bits |= !0u64 << size;
    }
    Some(bits as i32)
}

/// `value` of an absolute axis from `min` to `max`, from 0 to
/// POSITION_MAX.
fn scale(value: i32, min: i32, max: i32) -> u32 {
    if max <= min {
        return 0;
    }
    let from = i64::from(value.clamp(min, max)) - i64::from(min);
    (from * i64::from(POSITION_MAX) / (i64::from(max) - i64::from(min))) as u32
}

/// What a pointer did in one report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pointer {
    /// How far it moved: right and down are more.
    pub dx: i32,
    /// Down.
    pub dy: i32,
    /// The wheel's detents: away from the user is more.
    pub wheel: i32,
    /// The horizontal wheel's: right is more.
    pub pan: i32,
    /// Whether it says where it points, in `x` and `y`.
    pub absolute: bool,
    /// Where, from 0 to POSITION_MAX across: left to right.
    pub x: u32,
    /// Top to bottom.
    pub y: u32,
    /// The buttons down: bit `n` for button `n + 1`.
    pub buttons: u32,
}

/// What changed, in the order it is to be told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// A key went down or up.
    Key {
        /// Its KEY_* code.
        code: u16,
        /// Whether it went down.
        down: bool,
    },
    /// The pointer moved, or a button or wheel changed.
    Pointer(Pointer),
}

/// The report IDs whose keys a decoder follows.
const KEY_REPORTS: usize = 8;

/// Turns a device's Input reports into what changed.
#[derive(Clone, Debug)]
pub struct Decoder {
    /// The keys down, a bit per KEY_* code, of each report ID in
    /// `reports`, the first `used`.
    down: [[u64; 4]; KEY_REPORTS],
    reports: [u8; KEY_REPORTS],
    used: usize,
    buttons: u32,
    position: (u32, u32),
}

impl Default for Decoder {
    fn default() -> Decoder {
        Decoder::new()
    }
}

/// Calls `each` with every KEY_* code whose bit is set in `bits`.
fn each_key(bits: [u64; 4], each: &mut dyn FnMut(u16)) {
    for (word, &w) in bits.iter().enumerate() {
        let mut w = w;
        while w != 0 {
            let bit = w.trailing_zeros();
            w &= w - 1;
            each((word * 64) as u16 + bit as u16);
        }
    }
}

fn set_key(bits: &mut [u64; 4], code: u16) {
    bits[usize::from(code / 64)] |= 1 << (code % 64);
}

fn and(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
    core::array::from_fn(|i| a[i] & b[i])
}

fn and_not(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
    core::array::from_fn(|i| a[i] & !b[i])
}

impl Decoder {
    /// A decoder with no key or button down.
    pub const fn new() -> Decoder {
        Decoder {
            down: [[0; 4]; KEY_REPORTS],
            reports: [0; KEY_REPORTS],
            used: 0,
            buttons: 0,
            position: (0, 0),
        }
    }

    /// Where report `id`'s keys are kept, if a place is left for them.
    fn slot(&mut self, id: u8) -> Option<usize> {
        if let Some(at) = self.reports[..self.used].iter().position(|&r| r == id) {
            return Some(at);
        }
        let at = self.used;
        if at == KEY_REPORTS {
            return None;
        }
        self.reports[at] = id;
        self.used += 1;
        Some(at)
    }

    /// Reads the Input report `report` of a device described by `layout`,
    /// telling `out` what changed: the modifiers that went down or up,
    /// then the other keys that went up, then those that went down, then
    /// the pointer.
    pub fn report<A: Allocator + Clone>(
        &mut self,
        layout: &Layout<A>,
        report: &[u8],
        out: &mut dyn FnMut(Input),
    ) {
        let (id, data) = if layout.numbered {
            match report.split_first() {
                Some((&id, rest)) => (id, rest),
                None => return,
            }
        } else {
            (0, report)
        };
        let mut now = [0u64; 4];
        let (mut has_keys, mut rolled_over) = (false, false);
        let mut pointer = Pointer::default();
        let (mut has_pointer, mut has_buttons) = (false, false);
        let (mut x, mut y) = (None, None);
        for field in layout.fields().iter().filter(|f| f.report == id) {
            has_keys |= field.keys;
            has_pointer |= field.pointer;
            let signed = field.logical_min < 0;
            for i in 0..u32::from(field.count) {
                let at = field.offset + i * u32::from(field.size);
                let Some(value) = value(data, at, field.size, signed) else { break };
                if field.flags & flag::VARIABLE == 0 {
                    // An array: the value names the usage that is on.
                    if value < field.logical_min || value > field.logical_max {
                        continue;
                    }
                    let index = value.wrapping_sub(field.logical_min) as u32;
                    match layout.usage(field, index) {
                        Some(usage::ERROR_ROLL_OVER) => rolled_over = true,
                        Some(usage) => {
                            if let Some(code) = key(usage) {
                                set_key(&mut now, code);
                            }
                        }
                        None => {}
                    }
                    continue;
                }
                let Some(usage) = layout.usage(field, i).or_else(|| layout.last_usage(field))
                else {
                    break;
                };
                let relative = field.flags & flag::RELATIVE != 0;
                match usage {
                    usage::X if relative => pointer.dx = pointer.dx.wrapping_add(value),
                    usage::Y if relative => pointer.dy = pointer.dy.wrapping_add(value),
                    usage::X => x = Some(scale(value, field.logical_min, field.logical_max)),
                    usage::Y => y = Some(scale(value, field.logical_min, field.logical_max)),
                    usage::WHEEL => pointer.wheel = pointer.wheel.wrapping_add(value),
                    usage::AC_PAN => pointer.pan = pointer.pan.wrapping_add(value),
                    _ if usage >> 16 == u32::from(page::BUTTON) => {
                        has_buttons = true;
                        let n = usage & 0xffff;
                        if (1..=32).contains(&n) && value != 0 {
                            pointer.buttons |= 1 << (n - 1);
                        }
                    }
                    _ => {
                        if value != 0
                            && let Some(code) = key(usage)
                        {
                            set_key(&mut now, code);
                        }
                    }
                }
            }
        }
        if has_keys && let Some(slot) = self.slot(id) {
            let old = self.down[slot];
            if rolled_over {
                // Too many keys down to say which: the others stay as they
                // were.
                now = core::array::from_fn(|i| {
                    now[i] & keys::MODIFIERS[i] | old[i] & !keys::MODIFIERS[i]
                });
            }
            let changed: [u64; 4] = core::array::from_fn(|i| old[i] ^ now[i]);
            each_key(and(changed, keys::MODIFIERS), &mut |code| {
                out(Input::Key { code, down: now[usize::from(code / 64)] & 1 << (code % 64) != 0 });
            });
            let others = and_not(changed, keys::MODIFIERS);
            each_key(and(others, old), &mut |code| out(Input::Key { code, down: false }));
            each_key(and(others, now), &mut |code| out(Input::Key { code, down: true }));
            self.down[slot] = now;
        }
        if has_pointer {
            if !has_buttons {
                pointer.buttons = self.buttons;
            }
            let moved = (x.is_some() || y.is_some()) && {
                let position = (x.unwrap_or(self.position.0), y.unwrap_or(self.position.1));
                let moved = position != self.position;
                self.position = position;
                moved
            };
            pointer.absolute = x.is_some() || y.is_some();
            (pointer.x, pointer.y) = if pointer.absolute { self.position } else { (0, 0) };
            if pointer.dx != 0
                || pointer.dy != 0
                || pointer.wheel != 0
                || pointer.pan != 0
                || pointer.buttons != self.buttons
                || moved
            {
                self.buttons = pointer.buttons;
                out(Input::Pointer(pointer));
            }
        }
    }

    /// Lets go of every key and button down, telling `out`: the device
    /// is gone.
    pub fn release_all(&mut self, out: &mut dyn FnMut(Input)) {
        for slot in 0..self.used {
            let down = core::mem::take(&mut self.down[slot]);
            each_key(and_not(down, keys::MODIFIERS), &mut |code| {
                out(Input::Key { code, down: false });
            });
            each_key(and(down, keys::MODIFIERS), &mut |code| out(Input::Key { code, down: false }));
        }
        if self.buttons != 0 {
            self.buttons = 0;
            out(Input::Pointer(Pointer::default()));
        }
    }
}

/// The boot protocol's keyboard reports (HID 1.11, appendix B.1): the
/// modifiers' eight bits, a byte kept for the maker, and six keys down;
/// and the five lights of its Output report.
pub const BOOT_KEYBOARD: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xa1, 0x01, // Collection (Application)
    0x05, 0x07, //   Usage Page (Keyboard/Keypad)
    0x19, 0xe0, //   Usage Minimum (Left Control)
    0x29, 0xe7, //   Usage Maximum (Right GUI)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x01, //   Logical Maximum (1)
    0x75, 0x01, //   Report Size (1)
    0x95, 0x08, //   Report Count (8)
    0x81, 0x02, //   Input (Data, Variable, Absolute): the modifiers
    0x95, 0x01, //   Report Count (1)
    0x75, 0x08, //   Report Size (8)
    0x81, 0x01, //   Input (Constant): the byte kept
    0x95, 0x05, //   Report Count (5)
    0x75, 0x01, //   Report Size (1)
    0x05, 0x08, //   Usage Page (LEDs)
    0x19, 0x01, //   Usage Minimum (Num Lock)
    0x29, 0x05, //   Usage Maximum (Kana)
    0x91, 0x02, //   Output (Data, Variable, Absolute): the lights
    0x95, 0x01, //   Report Count (1)
    0x75, 0x03, //   Report Size (3)
    0x91, 0x01, //   Output (Constant)
    0x95, 0x06, //   Report Count (6)
    0x75, 0x08, //   Report Size (8)
    0x15, 0x00, //   Logical Minimum (0)
    0x26, 0xff, 0x00, // Logical Maximum (255)
    0x05, 0x07, //   Usage Page (Keyboard/Keypad)
    0x19, 0x00, //   Usage Minimum (0)
    0x2a, 0xff, 0x00, // Usage Maximum (255)
    0x81, 0x00, //   Input (Data, Array): the keys down
    0xc0, // End Collection
];

/// The boot protocol's mouse reports (HID 1.11, appendix B.2): three
/// buttons, then how far it moved, a byte each way.
pub const BOOT_MOUSE: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xa1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xa1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (1)
    0x29, 0x03, //     Usage Maximum (3)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x03, //     Report Count (3)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data, Variable, Absolute): the buttons
    0x95, 0x01, //     Report Count (1)
    0x75, 0x05, //     Report Size (5)
    0x81, 0x01, //     Input (Constant)
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7f, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x06, //     Input (Data, Variable, Relative): how far
    0xc0, //   End Collection
    0xc0, // End Collection
];
