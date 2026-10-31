// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What HID devices report, wherever they sit: USB's HID interfaces
//! (ADR 0148) and HID over I2C's devices (ADR 0235), whose reports
//! `radix_hid` decodes. Keys go to `/devices/kbd`, the pointer to
//! `/devices/mouse`.

use radix_hid::{Input, Layout, usage};
use radix_mm::Kmalloc;

use crate::fs::{keyboard, mouse};

/// Tells what a report changed: keys to `/devices/kbd`, the pointer to
/// `/devices/mouse`.
pub fn tell(input: Input) {
    match input {
        Input::Key { code, down } => keyboard::usb_key(code, down),
        Input::Pointer(p) => mouse::motion(mouse::Motion {
            dx: p.dx,
            dy: p.dy,
            wheel: p.wheel,
            pan: p.pan,
            position: p.absolute.then_some((p.x, p.y)),
            buttons: p.buttons,
        }),
    }
}

/// What a device laid out as `layout` is, and where it is read, for the
/// log; and whether anything it reports is read.
pub fn what(layout: &Layout<Kmalloc>) -> (&'static str, bool) {
    let touchpad = layout.applications().contains(&usage::TOUCH_PAD);
    let what = if layout.is_keyboard() {
        "a keyboard, read into /devices/kbd"
    } else if touchpad && layout.is_pointer() {
        // Until it is told to report its fingers, it is its mouse.
        "a touchpad, read as a mouse into /devices/mouse"
    } else if layout.is_pointer() && layout.is_absolute() {
        "a tablet, read into /devices/mouse"
    } else if layout.is_pointer() {
        "a mouse, read into /devices/mouse"
    } else if layout.has_keys() {
        "media keys, read into /devices/kbd"
    } else {
        return ("nothing it reports is read", false);
    };
    (what, true)
}
