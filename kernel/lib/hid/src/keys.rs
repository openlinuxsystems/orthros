// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Which key each usage is: the usages of the Keyboard/Keypad page, the
//! Consumer page's media and application keys, and the Generic Desktop
//! page's system keys (*HID Usage Tables 1.5*, chapters 10, 15 and 4), as
//! radix-input(2)'s key codes, which are Linux's. A usage the table does
//! not name is no key.

use radix_abi::input::*;

use crate::{Usage, page};

/// The Keyboard/Keypad page's usages from 0x00 to 0xE7 (chapter 10), by
/// their names there; 0 where a usage is no key here: the error usages,
/// the locking keys, and the rarer keys Linux leaves unmapped too.
const KEYBOARD: [u16; 0xe8] = {
    let mut keys = [0u16; 0xe8];
    // Letters, from 0x04: a to z.
    let letters = [
        KEY_A, KEY_B, KEY_C, KEY_D, KEY_E, KEY_F, KEY_G, KEY_H, KEY_I, KEY_J, KEY_K, KEY_L, KEY_M,
        KEY_N, KEY_O, KEY_P, KEY_Q, KEY_R, KEY_S, KEY_T, KEY_U, KEY_V, KEY_W, KEY_X, KEY_Y, KEY_Z,
    ];
    let mut i = 0;
    while i < letters.len() {
        keys[0x04 + i] = letters[i];
        i += 1;
    }
    // Digits, from 0x1E: 1 to 9, then 0; their key codes are in the same
    // order.
    let mut i = 0;
    while i < 10 {
        keys[0x1e + i] = KEY_1 + i as u16;
        i += 1;
    }
    keys[0x28] = KEY_ENTER;
    keys[0x29] = KEY_ESC;
    keys[0x2a] = KEY_BACKSPACE;
    keys[0x2b] = KEY_TAB;
    keys[0x2c] = KEY_SPACE;
    keys[0x2d] = KEY_MINUS;
    keys[0x2e] = KEY_EQUAL;
    keys[0x2f] = KEY_LEFTBRACE;
    keys[0x30] = KEY_RIGHTBRACE;
    keys[0x31] = KEY_BACKSLASH;
    // Non-US # and ~: where the backslash key is on ISO keyboards.
    keys[0x32] = KEY_BACKSLASH;
    keys[0x33] = KEY_SEMICOLON;
    keys[0x34] = KEY_APOSTROPHE;
    keys[0x35] = KEY_GRAVE;
    keys[0x36] = KEY_COMMA;
    keys[0x37] = KEY_DOT;
    keys[0x38] = KEY_SLASH;
    keys[0x39] = KEY_CAPSLOCK;
    // F1 to F10, then F11 and F12, whose codes are apart.
    let mut i = 0;
    while i < 10 {
        keys[0x3a + i] = KEY_F1 + i as u16;
        i += 1;
    }
    keys[0x44] = KEY_F11;
    keys[0x45] = KEY_F12;
    keys[0x46] = KEY_SYSRQ;
    keys[0x47] = KEY_SCROLLLOCK;
    keys[0x48] = KEY_PAUSE;
    keys[0x49] = KEY_INSERT;
    keys[0x4a] = KEY_HOME;
    keys[0x4b] = KEY_PAGEUP;
    keys[0x4c] = KEY_DELETE;
    keys[0x4d] = KEY_END;
    keys[0x4e] = KEY_PAGEDOWN;
    keys[0x4f] = KEY_RIGHT;
    keys[0x50] = KEY_LEFT;
    keys[0x51] = KEY_DOWN;
    keys[0x52] = KEY_UP;
    keys[0x53] = KEY_NUMLOCK;
    keys[0x54] = KEY_KPSLASH;
    keys[0x55] = KEY_KPASTERISK;
    keys[0x56] = KEY_KPMINUS;
    keys[0x57] = KEY_KPPLUS;
    keys[0x58] = KEY_KPENTER;
    // The keypad's 1 to 9, then 0 and its dot.
    let keypad = [KEY_KP1, KEY_KP2, KEY_KP3, KEY_KP4, KEY_KP5, KEY_KP6, KEY_KP7, KEY_KP8, KEY_KP9];
    let mut i = 0;
    while i < keypad.len() {
        keys[0x59 + i] = keypad[i];
        i += 1;
    }
    keys[0x62] = KEY_KP0;
    keys[0x63] = KEY_KPDOT;
    // Non-US \ and |: the key beside the left Shift on ISO keyboards.
    keys[0x64] = KEY_102ND;
    // Application: the menu key.
    keys[0x65] = KEY_COMPOSE;
    keys[0x66] = KEY_POWER;
    keys[0x67] = KEY_KPEQUAL;
    // F13 to F24.
    let mut i = 0;
    while i < 12 {
        keys[0x68 + i] = KEY_F13 + i as u16;
        i += 1;
    }
    keys[0x74] = KEY_OPEN;
    keys[0x75] = KEY_HELP;
    keys[0x76] = KEY_PROPS;
    keys[0x77] = KEY_FRONT;
    keys[0x78] = KEY_STOP;
    keys[0x79] = KEY_AGAIN;
    keys[0x7a] = KEY_UNDO;
    keys[0x7b] = KEY_CUT;
    keys[0x7c] = KEY_COPY;
    keys[0x7d] = KEY_PASTE;
    keys[0x7e] = KEY_FIND;
    keys[0x7f] = KEY_MUTE;
    keys[0x80] = KEY_VOLUMEUP;
    keys[0x81] = KEY_VOLUMEDOWN;
    keys[0x85] = KEY_KPCOMMA;
    // International1 to International6: the Japanese and Brazilian keys.
    keys[0x87] = KEY_RO;
    keys[0x88] = KEY_KATAKANAHIRAGANA;
    keys[0x89] = KEY_YEN;
    keys[0x8a] = KEY_HENKAN;
    keys[0x8b] = KEY_MUHENKAN;
    keys[0x8c] = KEY_KPJPCOMMA;
    // LANG1 to LANG5: the Korean and Japanese input keys.
    keys[0x90] = KEY_HANGEUL;
    keys[0x91] = KEY_HANJA;
    keys[0x92] = KEY_KATAKANA;
    keys[0x93] = KEY_HIRAGANA;
    keys[0x94] = KEY_ZENKAKUHANKAKU;
    // The modifiers, left then right: Control, Shift, Alt and GUI.
    keys[0xe0] = KEY_LEFTCTRL;
    keys[0xe1] = KEY_LEFTSHIFT;
    keys[0xe2] = KEY_LEFTALT;
    keys[0xe3] = KEY_LEFTMETA;
    keys[0xe4] = KEY_RIGHTCTRL;
    keys[0xe5] = KEY_RIGHTSHIFT;
    keys[0xe6] = KEY_RIGHTALT;
    keys[0xe7] = KEY_RIGHTMETA;
    keys
};

/// The key a Consumer page usage is (chapter 15): media, volume and
/// application keys.
fn consumer(id: u16) -> Option<u16> {
    let key = match id {
        0x030 => KEY_POWER,
        0x032 | 0x034 => KEY_SLEEP,
        0x06f => KEY_BRIGHTNESSUP,
        0x070 => KEY_BRIGHTNESSDOWN,
        0x0b0 => KEY_PLAYCD,
        0x0b1 => KEY_PAUSECD,
        0x0b2 => KEY_RECORD,
        0x0b3 => KEY_FASTFORWARD,
        0x0b4 => KEY_REWIND,
        0x0b5 => KEY_NEXTSONG,
        0x0b6 => KEY_PREVIOUSSONG,
        0x0b7 => KEY_STOPCD,
        0x0b8 => KEY_EJECTCD,
        0x0cd => KEY_PLAYPAUSE,
        0x0e2 => KEY_MUTE,
        0x0e9 => KEY_VOLUMEUP,
        0x0ea => KEY_VOLUMEDOWN,
        // AL Consumer Control Configuration: the media player.
        0x183 => KEY_CONFIG,
        0x18a => KEY_MAIL,
        0x192 => KEY_CALC,
        // AL Local Machine Browser: the file manager.
        0x194 => KEY_FILE,
        0x196 => KEY_WWW,
        // AL Terminal Lock/Screensaver.
        0x19e => KEY_SCREENLOCK,
        0x221 => KEY_SEARCH,
        0x223 => KEY_HOMEPAGE,
        0x224 => KEY_BACK,
        0x225 => KEY_FORWARD,
        0x226 => KEY_STOP,
        0x227 => KEY_REFRESH,
        0x22a => KEY_BOOKMARKS,
        _ => return None,
    };
    Some(key)
}

/// The key `usage` is, if it is one.
pub fn key(usage: Usage) -> Option<u16> {
    let id = usage as u16;
    let code = match (usage >> 16) as u16 {
        page::KEYBOARD => KEYBOARD.get(usize::from(id)).copied().unwrap_or(0),
        page::CONSUMER => consumer(id)?,
        // System Power Down, System Sleep and System Wake Up (chapter 4).
        page::GENERIC_DESKTOP => match id {
            0x81 => KEY_POWER,
            0x82 => KEY_SLEEP,
            0x83 => KEY_WAKEUP,
            _ => return None,
        },
        _ => return None,
    };
    (code != 0).then_some(code)
}

/// The modifier keys, a bit per key code, as [`crate::Decoder`] keeps keys:
/// the changes to these go first in what a report changed.
pub(crate) const MODIFIERS: [u64; 4] = {
    let mut bits = [0u64; 4];
    let codes = [
        KEY_LEFTCTRL,
        KEY_LEFTSHIFT,
        KEY_LEFTALT,
        KEY_LEFTMETA,
        KEY_RIGHTCTRL,
        KEY_RIGHTSHIFT,
        KEY_RIGHTALT,
        KEY_RIGHTMETA,
    ];
    let mut i = 0;
    while i < codes.len() {
        bits[codes[i] as usize / 64] |= 1 << (codes[i] % 64);
        i += 1;
    }
    bits
};
