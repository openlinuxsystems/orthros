// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::alloc::Global;

use proptest::prelude::*;
use radix_abi::input::*;

use super::*;

fn layout(bytes: &[u8]) -> Layout<Global> {
    Layout::parse(bytes, Global).expect("the descriptor parses")
}

fn parse(bytes: &[u8]) -> Result<Layout<Global>, Error> {
    Layout::parse(bytes, Global)
}

fn feed(decoder: &mut Decoder, layout: &Layout<Global>, report: &[u8]) -> Vec<Input> {
    let mut out = Vec::new();
    decoder.report(layout, report, &mut |input| out.push(input));
    out
}

fn down(code: u16) -> Input {
    Input::Key { code, down: true }
}

fn up(code: u16) -> Input {
    Input::Key { code, down: false }
}

/// QEMU's usb-tablet: three buttons, X and Y from 0 to 0x7fff, a wheel.
const TABLET: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f,
    0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x35, 0x00,
    0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0, 0xc0,
];

/// A keyboard with its media keys, as wireless receivers have them:
/// report 1 the boot keyboard's keys, report 2 a consumer control's 16-bit
/// array, report 3 the system keys as bits.
const COMPOSITE: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x85, 0x01, // keyboard, report 1
    0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81,
    0x02, // modifiers
    0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x26, 0xff, 0x00, 0x19, 0x00, 0x2a, 0xff, 0x00, 0x81,
    0x00, // six keys
    0x05, 0x08, 0x19, 0x01, 0x29, 0x03, 0x95, 0x03, 0x75, 0x01, 0x91, 0x02, // lights
    0x95, 0x05, 0x91, 0x01, 0xc0, // padding, end
    0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x02, // consumer control, report 2
    0x15, 0x00, 0x26, 0x9c, 0x02, 0x19, 0x00, 0x2a, 0x9c, 0x02, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00,
    0xc0, // one 16-bit usage
    0x05, 0x01, 0x09, 0x80, 0xa1, 0x01, 0x85, 0x03, // system control, report 3
    0x19, 0x81, 0x29, 0x83, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x03, 0x81, 0x02, 0x95, 0x05,
    0x81, 0x01, 0xc0,
];

#[test]
fn the_boot_keyboard_parses_with_its_lights() {
    let layout = layout(BOOT_KEYBOARD);
    assert!(layout.is_keyboard() && layout.has_keys() && layout.has_lights());
    assert!(!layout.is_pointer() && !layout.numbered());
    assert_eq!(layout.input_bytes(), 8);
    assert_eq!(layout.applications(), &[usage::KEYBOARD]);
    assert_eq!(layout.fields().len(), 2);
    let mut out = [0xaa; 4];
    assert_eq!(layout.lights_report(LED_CAPS_LOCK, &mut out), Some((0, 1)));
    assert_eq!(out[0], 0b010);
    assert_eq!(layout.lights_report(LED_NUM_LOCK | LED_SCROLL_LOCK, &mut out), Some((0, 1)));
    assert_eq!(out[0], 0b101);
    assert_eq!(layout.lights_report(0, &mut out), Some((0, 1)));
    assert_eq!(out[0], 0);
    assert_eq!(layout.lights_report(0, &mut []), None);
}

#[test]
fn keys_go_down_and_up_with_the_modifiers_first() {
    let layout = layout(BOOT_KEYBOARD);
    let mut decoder = Decoder::new();
    // Shift and A in one report: Shift first, though its code is higher.
    assert_eq!(
        feed(&mut decoder, &layout, &[0x02, 0, 0x04, 0, 0, 0, 0, 0]),
        [down(KEY_LEFTSHIFT), down(KEY_A)]
    );
    // B too, A still down: only B.
    assert_eq!(feed(&mut decoder, &layout, &[0x02, 0, 0x05, 0x04, 0, 0, 0, 0]), [down(KEY_B)]);
    // The same report again: nothing.
    assert!(feed(&mut decoder, &layout, &[0x02, 0, 0x05, 0x04, 0, 0, 0, 0]).is_empty());
    // A up and C down at once: the release first.
    assert_eq!(
        feed(&mut decoder, &layout, &[0x02, 0, 0x05, 0x06, 0, 0, 0, 0]),
        [up(KEY_A), down(KEY_C)]
    );
    // All up: Shift first, then the keys, in the order of their codes.
    assert_eq!(
        feed(&mut decoder, &layout, &[0, 0, 0, 0, 0, 0, 0, 0]),
        [up(KEY_LEFTSHIFT), up(KEY_C), up(KEY_B)]
    );
    // The right GUI and Enter.
    assert_eq!(
        feed(&mut decoder, &layout, &[0x80, 0, 0x28, 0, 0, 0, 0, 0]),
        [down(KEY_RIGHTMETA), down(KEY_ENTER)]
    );
}

#[test]
fn roll_over_keeps_the_keys_and_follows_the_modifiers() {
    let layout = layout(BOOT_KEYBOARD);
    let mut decoder = Decoder::new();
    feed(&mut decoder, &layout, &[0, 0, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09]);
    // Too many: the six stay down, Ctrl goes down.
    assert_eq!(feed(&mut decoder, &layout, &[0x01, 0, 1, 1, 1, 1, 1, 1]), [down(KEY_LEFTCTRL)]);
    // Back to A alone: the others go up, in the order of their codes.
    assert_eq!(
        feed(&mut decoder, &layout, &[0x01, 0, 0x04, 0, 0, 0, 0, 0]),
        [up(KEY_E), up(KEY_D), up(KEY_F), up(KEY_C), up(KEY_B)]
    );
}

#[test]
fn every_key_is_let_go_of_when_the_device_goes() {
    let layout = layout(BOOT_KEYBOARD);
    let mut decoder = Decoder::new();
    feed(&mut decoder, &layout, &[0x04, 0, 0x2c, 0, 0, 0, 0, 0]);
    let mut out = Vec::new();
    decoder.release_all(&mut |input| out.push(input));
    assert_eq!(out, [up(KEY_SPACE), up(KEY_LEFTALT)]);
    out.clear();
    decoder.release_all(&mut |input| out.push(input));
    assert!(out.is_empty());
}

#[test]
fn the_boot_mouse_moves_and_clicks() {
    let layout = layout(BOOT_MOUSE);
    assert!(layout.is_pointer() && !layout.has_keys() && !layout.is_absolute());
    assert_eq!(layout.input_bytes(), 3);
    let mut decoder = Decoder::new();
    let moved = feed(&mut decoder, &layout, &[0x01, 5, 0xfb]);
    assert_eq!(
        moved,
        [Input::Pointer(Pointer { dx: 5, dy: -5, buttons: BUTTON_LEFT, ..Pointer::default() })]
    );
    // The button still down, nothing moved: nothing to tell.
    assert!(feed(&mut decoder, &layout, &[0x01, 0, 0]).is_empty());
    assert_eq!(
        feed(&mut decoder, &layout, &[0x04, 0, 0]),
        [Input::Pointer(Pointer { buttons: BUTTON_MIDDLE, ..Pointer::default() })]
    );
    let mut out = Vec::new();
    decoder.release_all(&mut |input| out.push(input));
    assert_eq!(out, [Input::Pointer(Pointer::default())]);
    // A short report: what is there is read.
    assert_eq!(
        feed(&mut decoder, &layout, &[0x02, 3]),
        [Input::Pointer(Pointer { dx: 3, buttons: BUTTON_RIGHT, ..Pointer::default() })]
    );
}

#[test]
fn a_tablet_says_where_it_points() {
    let layout = layout(TABLET);
    assert!(layout.is_pointer() && layout.is_absolute());
    assert_eq!(layout.input_bytes(), 6);
    let mut decoder = Decoder::new();
    let at = |x: u32, y: u32, buttons: u32, wheel: i32| {
        Input::Pointer(Pointer { absolute: true, x, y, buttons, wheel, ..Pointer::default() })
    };
    assert_eq!(feed(&mut decoder, &layout, &[0, 0xff, 0x7f, 0, 0, 0]), [at(POSITION_MAX, 0, 0, 0)]);
    assert_eq!(
        feed(&mut decoder, &layout, &[0, 0x00, 0x40, 0xff, 0x7f, 0]),
        [at(32768, POSITION_MAX, 0, 0)]
    );
    // The same place: nothing; then a click there, and the wheel turned
    // toward the user.
    assert!(feed(&mut decoder, &layout, &[0, 0x00, 0x40, 0xff, 0x7f, 0]).is_empty());
    assert_eq!(
        feed(&mut decoder, &layout, &[1, 0x00, 0x40, 0xff, 0x7f, 0xfe]),
        [at(32768, POSITION_MAX, BUTTON_LEFT, -2)]
    );
}

/// A touchpad's, as HID over I2C ones have it (Microsoft's Precision
/// Touchpad collections): report 1 a mouse's two buttons and relative X
/// and Y, which it reports until told to report its fingers; report 3
/// the touchpad's two fingers, each with its tip, confidence, contact ID
/// and absolute X and Y, then the scan time, the contact count and its
/// button; report 4 its input mode, a feature.
const TOUCHPAD: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xa1, 0x00, // mouse, report 1
    0x05, 0x09, 0x19, 0x01, 0x29, 0x02, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x02, 0x81, 0x02,
    0x95, 0x06, 0x81, 0x01, // two buttons, padding
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
    0xc0, 0xc0, // relative X and Y
    0x05, 0x0d, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x03, // touch pad, report 3
    0x09, 0x22, 0xa1, 0x02, 0x09, 0x42, 0x09, 0x47, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x02,
    0x81, 0x02, 0x95, 0x06, 0x81, 0x03, 0x09, 0x51, 0x25, 0x0f, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x26, 0xff, 0x0f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02,
    0xc0, // a finger
    0x05, 0x0d, 0x09, 0x22, 0xa1, 0x02, 0x09, 0x42, 0x09, 0x47, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
    0x95, 0x02, 0x81, 0x02, 0x95, 0x06, 0x81, 0x03, 0x09, 0x51, 0x25, 0x0f, 0x75, 0x08, 0x95, 0x01,
    0x81, 0x02, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x26, 0xff, 0x0f, 0x75, 0x10, 0x95, 0x02, 0x81,
    0x02, 0xc0, // another
    0x05, 0x0d, 0x09, 0x56, 0x27, 0xff, 0xff, 0x00, 0x00, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02, 0x09,
    0x54, 0x25, 0x02, 0x75, 0x08, 0x81, 0x02, // scan time, contact count
    0x05, 0x09, 0x09, 0x01, 0x25, 0x01, 0x75, 0x01, 0x81, 0x02, 0x95, 0x07, 0x81, 0x03,
    0xc0, // its button, padding
    0x05, 0x0d, 0x09, 0x0e, 0xa1, 0x01, 0x85, 0x04, 0x09, 0x52, 0x25, 0x0a, 0x75, 0x08, 0x95, 0x01,
    0xb1, 0x02, 0xc0, // configuration, report 4: input mode
];

#[test]
fn numbered_reports_keep_their_own_keys() {
    let layout = layout(COMPOSITE);
    assert!(layout.numbered() && layout.is_keyboard() && layout.has_lights());
    assert_eq!(
        layout.applications(),
        &[usage::KEYBOARD, usage::CONSUMER_CONTROL, usage::SYSTEM_CONTROL]
    );
    assert_eq!(layout.input_bytes(), 8);
    let mut decoder = Decoder::new();
    assert_eq!(feed(&mut decoder, &layout, &[1, 0, 0x04, 0, 0, 0, 0, 0]), [down(KEY_A)]);
    // Mute, then the volume, in report 2: A stays down.
    assert_eq!(feed(&mut decoder, &layout, &[2, 0xe2, 0x00]), [down(KEY_MUTE)]);
    assert_eq!(feed(&mut decoder, &layout, &[2, 0xe9, 0x00]), [up(KEY_MUTE), down(KEY_VOLUMEUP)]);
    assert_eq!(feed(&mut decoder, &layout, &[2, 0, 0]), [up(KEY_VOLUMEUP)]);
    // An application key past 0xff.
    assert_eq!(feed(&mut decoder, &layout, &[2, 0x92, 0x01]), [down(KEY_CALC)]);
    // Sleep, in report 3.
    assert_eq!(feed(&mut decoder, &layout, &[3, 0b010]), [down(KEY_SLEEP)]);
    assert_eq!(feed(&mut decoder, &layout, &[1, 0, 0, 0, 0, 0, 0, 0]), [up(KEY_A)]);
    // A report ID it does not have, and an empty report: nothing.
    assert!(feed(&mut decoder, &layout, &[9, 1, 2, 3]).is_empty());
    assert!(feed(&mut decoder, &layout, &[]).is_empty());
    let mut out = [0u8; 8];
    assert_eq!(layout.lights_report(LED_CAPS_LOCK, &mut out), Some((1, 2)));
    assert_eq!(out[..2], [1, 0b010]);
}

#[test]
fn a_touchpad_is_a_mouse_until_told_otherwise() {
    let layout = layout(TOUCHPAD);
    assert_eq!(layout.applications(), [usage::MOUSE, usage::TOUCH_PAD, 0x000d_000e]);
    assert!(layout.is_pointer() && !layout.has_keys());
    assert_eq!(layout.input_bytes(), 1 + 2 * 6 + 4);
    let mut decoder = Decoder::new();
    // Its mouse's report moves and clicks as a mouse's does.
    assert_eq!(
        feed(&mut decoder, &layout, &[1, 0x01, 5, 0xfb]),
        [Input::Pointer(Pointer { dx: 5, dy: -5, buttons: BUTTON_LEFT, ..Pointer::default() })]
    );
    assert_eq!(feed(&mut decoder, &layout, &[1, 0x00, 0, 0]), [Input::Pointer(Pointer::default())]);
}

#[test]
fn a_page_written_late_applies_to_the_usages_before_it() {
    // Usage (X), Usage (Y) written while the Button page is in effect,
    // then Usage Page (Generic Desktop) before the Input item: they are X
    // and Y, as Linux reads them.
    let late = [
        0x05, 0x09, 0x09, 0x30, 0x09, 0x31, 0x05, 0x01, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95,
        0x02, 0x81, 0x06,
    ];
    let layout = layout(&late);
    let field = layout.fields()[0];
    assert_eq!(layout.usage(&field, 0), Some(usage::X));
    assert_eq!(layout.usage(&field, 1), Some(usage::Y));
    // A four-byte usage keeps its own page.
    let long = [
        0x05, 0x01, 0x0b, 0xe2, 0x00, 0x0c, 0x00, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01,
        0x81, 0x02,
    ];
    let layout = self::layout(&long);
    assert_eq!(layout.usage(&layout.fields()[0], 0), Some(0x000c_00e2));
    let mut decoder = Decoder::new();
    assert_eq!(feed(&mut decoder, &layout, &[1]), [down(KEY_MUTE)]);
}

#[test]
fn values_past_the_usages_take_the_last() {
    // Five buttons named by one usage range of three: buttons 4 and 5 are
    // button 3's usage, so they press the middle button.
    let short = [
        0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x05, 0x81,
        0x02, 0x75, 0x03, 0x95, 0x01, 0x81, 0x01,
    ];
    let layout = layout(&short);
    let mut decoder = Decoder::new();
    assert_eq!(
        feed(&mut decoder, &layout, &[0b10000]),
        [Input::Pointer(Pointer { buttons: BUTTON_MIDDLE, ..Pointer::default() })]
    );
}

#[test]
fn broken_descriptors_are_refused() {
    // An item cut short, and a long item past the end.
    assert_eq!(parse(&[0x05, 0x01, 0x26, 0xff]).err(), Some(Error::Truncated));
    assert_eq!(parse(&[0xfe, 0x04, 0x00, 0x01]).err(), Some(Error::Truncated));
    // Report ID 0.
    assert_eq!(parse(&[0x85, 0x00]).err(), Some(Error::BadReportId));
    // Pop without Push, and Push past its stack.
    assert_eq!(parse(&[0xb4]).err(), Some(Error::Stack));
    assert_eq!(parse(&[0xa4; 5]).err(), Some(Error::Stack));
    // A report past 8 KiB.
    assert_eq!(parse(&[0x75, 0x20, 0x96, 0x00, 0x08, 0x81, 0x01]).err(), Some(Error::TooLong));
    // Nothing to read: only padding, and only an output.
    assert_eq!(parse(&[0x75, 0x08, 0x95, 0x01, 0x81, 0x01]).err(), Some(Error::NoInputs));
    assert_eq!(parse(&[]).err(), Some(Error::NoInputs));
    // Too many fields.
    let mut many = vec![0x05, 0x09, 0x75, 0x01, 0x95, 0x01, 0x15, 0x00, 0x25, 0x01];
    for _ in 0..=MAX_FIELDS {
        many.extend_from_slice(&[0x09, 0x01, 0x81, 0x02]);
    }
    assert_eq!(parse(&many).err(), Some(Error::TooManyFields));
    // A long item is skipped, and Push and Pop give back what was pushed.
    let skipped = [
        0xfe, 0x02, 0x10, 0xaa, 0xbb, 0x05, 0x09, 0xa4, 0x05, 0x01, 0xb4, 0x09, 0x01, 0x15, 0x00,
        0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02,
    ];
    let layout = parse(&skipped).expect("parses");
    assert_eq!(layout.usage(&layout.fields()[0], 0), Some(0x0009_0001));
}

#[test]
fn keys_have_linux_codes() {
    assert_eq!(key(0x0007_0004), Some(KEY_A));
    assert_eq!(key(0x0007_001d), Some(KEY_Z));
    assert_eq!(key(0x0007_001e), Some(KEY_1));
    assert_eq!(key(0x0007_0027), Some(KEY_0));
    assert_eq!(key(0x0007_0028), Some(KEY_ENTER));
    assert_eq!(key(0x0007_0032), Some(KEY_BACKSLASH));
    assert_eq!(key(0x0007_0045), Some(KEY_F12));
    assert_eq!(key(0x0007_0059), Some(KEY_KP1));
    assert_eq!(key(0x0007_0062), Some(KEY_KP0));
    assert_eq!(key(0x0007_0064), Some(KEY_102ND));
    assert_eq!(key(0x0007_0073), Some(KEY_F24));
    assert_eq!(key(0x0007_00e5), Some(KEY_RIGHTSHIFT));
    assert_eq!(key(0x000c_00cd), Some(KEY_PLAYPAUSE));
    assert_eq!(key(0x0001_0081), Some(KEY_POWER));
    // Not keys: the error usages, the locking keys, past the table, other
    // pages.
    for usage in [0x0007_0000, 0x0007_0001, 0x0007_0082, 0x0007_00e8, 0x0009_0001, 0x000c_0001] {
        assert_eq!(key(usage), None, "{usage:#x}");
    }
    // Every letter and digit is a different key.
    let codes: std::collections::BTreeSet<_> =
        (0x04..=0x27).filter_map(|id| key(0x0007_0000 | id)).collect();
    assert_eq!(codes.len(), 36);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// Whatever a device sends as its descriptor, parsing it ends, and a
    /// layout it gives decodes whatever reports come.
    #[test]
    fn any_descriptor_and_reports_are_safe(
        descriptor in proptest::collection::vec(any::<u8>(), 0..200),
        reports in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..16), 0..8),
    ) {
        if let Ok(layout) = parse(&descriptor) {
            let mut decoder = Decoder::new();
            for report in &reports {
                decoder.report(&layout, report, &mut |_| {});
            }
            let _ = layout.input_bytes();
            let _ = layout.lights_report(7, &mut [0u8; 64]);
        }
    }

    /// Whatever a keyboard sends, a key goes down only while up and up
    /// only while down, and once the device goes none is left down.
    #[test]
    fn keys_go_down_and_up_in_turn(
        reports in proptest::collection::vec(
            (any::<u8>(), proptest::collection::vec(0u8..0x70, 6)), 1..40),
    ) {
        let layout = layout(BOOT_KEYBOARD);
        let mut decoder = Decoder::new();
        let mut held = std::collections::BTreeSet::new();
        let mut check = |input: Input| match input {
            Input::Key { code, down: true } => assert!(held.insert(code), "{code} down twice"),
            Input::Key { code, down: false } => assert!(held.remove(&code), "{code} up twice"),
            Input::Pointer(_) => panic!("a keyboard has no pointer"),
        };
        for (modifiers, keys) in &reports {
            let mut report = vec![*modifiers, 0];
            report.extend_from_slice(keys);
            decoder.report(&layout, &report, &mut check);
        }
        decoder.release_all(&mut check);
        prop_assert!(held.is_empty());
    }
}
