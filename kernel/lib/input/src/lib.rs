// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Keyboards for the Radix kernel (Phase 9, ADR 0097): what a PS/2
//! keyboard sends, as the key events of radix-input(2).
//!
//! The i8042 controller translates what the keyboard sends into scan code
//! set 1, as PC firmware leaves it, and [`Set1`] turns those bytes into
//! key codes (Linux's, `radix_abi::input`):
//!
//! - a byte below 0x80 presses a key, and the same byte with bit 7 set
//!   releases it;
//! - 0xE0 before one names an extended key: the arrows and the block above
//!   them, the right Ctrl and Alt, the keypad's Enter and slash, the
//!   Windows and menu keys, and the media keys;
//! - Print Screen, and the extended keys while Shift or Num Lock is on,
//!   come with fake presses and releases of a Shift (E0 2A, E0 AA, E0 36,
//!   E0 B6), which are dropped;
//! - Pause sends E1 1D 45 E1 9D C5 and nothing when it goes up: the first
//!   three bytes press it and the last three release it.
//!
//! A key pressed again while it is down is a repeat, as the keyboard sends
//! it again while it is held. A release of a key that is not down, as the
//! 0xAA a keyboard sends after it resets, is dropped, and so are the bytes
//! that are not keys at all (0x00 and 0xFF, the keyboard's errors).
//!
//! The crate knows nothing of the kernel, so its tests run on the host.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use radix_abi::input::*;

/// A key going down, repeating or going up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    /// A KEY_* code.
    pub code: u16,
    /// RELEASED, PRESSED or REPEATED.
    pub value: u16,
}

/// What came before the byte being decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prefix {
    /// Nothing: the byte starts a key.
    None,
    /// 0xE0: the byte is an extended key's.
    Extended,
    /// Pause's sequence, with this many of its bytes seen.
    Pause(u8),
}

/// The bytes of Pause's sequence after its first 0xE1.
const PAUSE: [u8; 5] = [0x1d, 0x45, 0xe1, 0x9d, 0xc5];

/// Turns scan code set 1 into key events.
#[derive(Clone, Debug)]
pub struct Set1 {
    prefix: Prefix,
    /// The keys down, a bit per code.
    down: [u64; 4],
}

impl Default for Set1 {
    fn default() -> Set1 {
        Set1::new()
    }
}

impl Set1 {
    /// A decoder with no key down.
    pub const fn new() -> Set1 {
        Set1 { prefix: Prefix::None, down: [0; 4] }
    }

    /// Forgets the keys down and any sequence begun: after the keyboard
    /// has reset, or its bytes were lost.
    pub fn reset(&mut self) {
        *self = Set1::new();
    }

    /// Whether key `code` is down.
    pub fn is_down(&self, code: u16) -> bool {
        let code = usize::from(code);
        code < 256 && self.down[code / 64] & (1 << (code % 64)) != 0
    }

    /// Takes the next byte from the keyboard; returns the key event it
    /// ends, if any.
    pub fn feed(&mut self, byte: u8) -> Option<Key> {
        match (self.prefix, byte) {
            (Prefix::Pause(seen), _) => {
                if PAUSE.get(usize::from(seen)) != Some(&byte) {
                    // Not Pause after all: start again from this byte.
                    self.prefix = Prefix::None;
                    return self.feed(byte);
                }
                let seen = seen + 1;
                self.prefix = if usize::from(seen) == PAUSE.len() {
                    Prefix::None
                } else {
                    Prefix::Pause(seen)
                };
                match seen {
                    2 => self.change(KEY_PAUSE, true),
                    5 => self.change(KEY_PAUSE, false),
                    _ => None,
                }
            }
            (_, 0xe0) => {
                self.prefix = Prefix::Extended;
                None
            }
            (_, 0xe1) => {
                self.prefix = Prefix::Pause(0);
                None
            }
            // Errors: the keyboard's buffer ran over, or a key it could
            // not tell.
            (_, 0x00 | 0xff) => {
                self.prefix = Prefix::None;
                None
            }
            (prefix, byte) => {
                self.prefix = Prefix::None;
                let make = byte & 0x7f;
                let code = if prefix == Prefix::Extended { extended(make) } else { plain(make) };
                match code {
                    0 => None,
                    code => self.change(code, byte & 0x80 == 0),
                }
            }
        }
    }

    /// Key `code` went down (or repeats) if `pressed`, else up.
    fn change(&mut self, code: u16, pressed: bool) -> Option<Key> {
        let (word, bit) = (usize::from(code) / 64, 1u64 << (code % 64));
        let was = self.down[word] & bit != 0;
        if pressed {
            self.down[word] |= bit;
            Some(Key { code, value: if was { REPEATED } else { PRESSED } })
        } else {
            self.down[word] &= !bit;
            was.then_some(Key { code, value: RELEASED })
        }
    }
}

/// The key a set 1 make code (below 0x80) without a prefix names, or 0.
/// Up to 0x58 the codes are Linux's own numbers, which were taken from
/// this set.
pub const fn plain(make: u8) -> u16 {
    match make {
        0x01..=0x53 | 0x56..=0x58 => make as u16,
        // Alt+Print Screen sends 0x54.
        0x54 => KEY_SYSRQ,
        0x59 => KEY_KPEQUAL,
        0x64..=0x6e => KEY_F13 + (make - 0x64) as u16,
        0x70 => KEY_KATAKANAHIRAGANA,
        0x73 => KEY_RO,
        0x76 => KEY_F24,
        0x79 => KEY_HENKAN,
        0x7b => KEY_MUHENKAN,
        0x7d => KEY_YEN,
        0x7e => KEY_KPCOMMA,
        _ => 0,
    }
}

/// The key a set 1 make code after 0xE0 names, or 0 (the fake Shifts
/// among them).
pub const fn extended(make: u8) -> u16 {
    match make {
        0x10 => KEY_PREVIOUSSONG,
        0x19 => KEY_NEXTSONG,
        0x1c => KEY_KPENTER,
        0x1d => KEY_RIGHTCTRL,
        0x20 => KEY_MUTE,
        0x21 => KEY_CALC,
        0x22 => KEY_PLAYPAUSE,
        0x24 => KEY_STOPCD,
        0x2e => KEY_VOLUMEDOWN,
        0x30 => KEY_VOLUMEUP,
        0x32 => KEY_HOMEPAGE,
        0x35 => KEY_KPSLASH,
        0x37 => KEY_SYSRQ,
        0x38 => KEY_RIGHTALT,
        // Ctrl+Pause (Break).
        0x46 => KEY_PAUSE,
        0x47 => KEY_HOME,
        0x48 => KEY_UP,
        0x49 => KEY_PAGEUP,
        0x4b => KEY_LEFT,
        0x4d => KEY_RIGHT,
        0x4f => KEY_END,
        0x50 => KEY_DOWN,
        0x51 => KEY_PAGEDOWN,
        0x52 => KEY_INSERT,
        0x53 => KEY_DELETE,
        0x5b => KEY_LEFTMETA,
        0x5c => KEY_RIGHTMETA,
        0x5d => KEY_COMPOSE,
        0x5e => KEY_POWER,
        0x5f => KEY_SLEEP,
        0x63 => KEY_WAKEUP,
        0x65 => KEY_SEARCH,
        0x66 => KEY_BOOKMARKS,
        0x67 => KEY_REFRESH,
        0x68 => KEY_STOP,
        0x69 => KEY_FORWARD,
        0x6a => KEY_BACK,
        0x6b => KEY_COMPUTER,
        0x6c => KEY_MAIL,
        0x6d => KEY_MEDIA,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<(u16, u16)> {
        let mut set = Set1::new();
        bytes.iter().filter_map(|&b| set.feed(b)).map(|k| (k.code, k.value)).collect()
    }

    #[test]
    fn plain_keys_press_and_release() {
        // a, then Shift+1.
        assert_eq!(
            keys(&[0x1e, 0x9e, 0x2a, 0x02, 0x82, 0xaa]),
            [
                (KEY_A, PRESSED),
                (KEY_A, RELEASED),
                (KEY_LEFTSHIFT, PRESSED),
                (KEY_1, PRESSED),
                (KEY_1, RELEASED),
                (KEY_LEFTSHIFT, RELEASED)
            ]
        );
        assert_eq!(
            keys(&[0x57, 0xd7, 0x58]),
            [(KEY_F11, PRESSED), (KEY_F11, RELEASED), (KEY_F12, PRESSED)]
        );
    }

    #[test]
    fn held_keys_repeat() {
        assert_eq!(
            keys(&[0x1c, 0x1c, 0x1c, 0x9c]),
            [
                (KEY_ENTER, PRESSED),
                (KEY_ENTER, REPEATED),
                (KEY_ENTER, REPEATED),
                (KEY_ENTER, RELEASED)
            ]
        );
    }

    #[test]
    fn extended_keys_and_fake_shifts() {
        // Up with Num Lock on: a fake Shift around it.
        assert_eq!(
            keys(&[0xe0, 0x2a, 0xe0, 0x48, 0xe0, 0xc8, 0xe0, 0xaa]),
            [(KEY_UP, PRESSED), (KEY_UP, RELEASED)]
        );
        // The right Ctrl and the left one are different keys.
        assert_eq!(
            keys(&[0xe0, 0x1d, 0x1d, 0xe0, 0x9d, 0x9d]),
            [
                (KEY_RIGHTCTRL, PRESSED),
                (KEY_LEFTCTRL, PRESSED),
                (KEY_RIGHTCTRL, RELEASED),
                (KEY_LEFTCTRL, RELEASED)
            ]
        );
        // Print Screen.
        assert_eq!(
            keys(&[0xe0, 0x2a, 0xe0, 0x37, 0xe0, 0xb7, 0xe0, 0xaa]),
            [(KEY_SYSRQ, PRESSED), (KEY_SYSRQ, RELEASED)]
        );
    }

    #[test]
    fn pause_presses_and_releases() {
        assert_eq!(
            keys(&[0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5]),
            [(KEY_PAUSE, PRESSED), (KEY_PAUSE, RELEASED)]
        );
        // A broken sequence goes on as plain bytes: 0x1e is a.
        assert_eq!(keys(&[0xe1, 0x1e, 0x9e]), [(KEY_A, PRESSED), (KEY_A, RELEASED)]);
    }

    #[test]
    fn stray_bytes_are_dropped() {
        // 0xAA (a reset keyboard's "passed") releases no key that is down;
        // errors and unknown codes give nothing.
        assert_eq!(keys(&[0xaa, 0x00, 0xff, 0x5a, 0xe0, 0x01]), []);
        let mut set = Set1::new();
        set.feed(0x1e);
        assert!(set.is_down(KEY_A));
        set.reset();
        assert!(!set.is_down(KEY_A));
        assert_eq!(set.feed(0x9e), None);
    }

    #[test]
    fn every_code_fits_the_bitmap() {
        for make in 0..0x80 {
            assert!(plain(make) < 256 && extended(make) < 256);
        }
    }
}
