// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Turning keys into what a terminal sends, as Linux's console does with
//! its default (US) key map: characters, control characters with Ctrl,
//! ESC before them with Alt, and the escape sequences of terminfo's
//! `linux` entry for the cursor, editing, function and keypad keys. The
//! keys consoled keeps for itself: Alt+F1 to Alt+F6 (with or without Ctrl)
//! and Alt+Left and Alt+Right switch consoles, Shift+Page Up and Shift+Page
//! Down look back, and Ctrl+Alt+Delete restarts the machine.

use radix_abi::input::*;

use crate::term::Modes;

/// What a key does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing.
    None,
    /// Send these bytes to the console's program.
    Send(Vec<u8>),
    /// Show console `n` (from 0).
    Switch(usize),
    /// Show the next console (1) or the previous one (-1).
    SwitchBy(isize),
    /// Look back half a screen (1) or forward (-1).
    Page(isize),
    /// Restart the machine.
    Reboot,
    /// A lock key changed: light the keyboard's lights again.
    Lights,
}

/// The state of the modifier and lock keys.
#[derive(Clone, Copy, Debug, Default)]
pub struct Keyboard {
    shift: [bool; 2],
    ctrl: [bool; 2],
    alt: [bool; 2],
    /// Caps Lock.
    pub caps: bool,
    /// Num Lock.
    pub num: bool,
    /// Scroll Lock: the console's output is held while it is on.
    pub scroll: bool,
}

/// The character the key `code` types in the US layout, and with Shift.
fn us(code: u16) -> Option<(char, char)> {
    let pair = match code {
        KEY_1 => ('1', '!'),
        KEY_2 => ('2', '@'),
        KEY_3 => ('3', '#'),
        KEY_4 => ('4', '$'),
        KEY_5 => ('5', '%'),
        KEY_6 => ('6', '^'),
        KEY_7 => ('7', '&'),
        KEY_8 => ('8', '*'),
        KEY_9 => ('9', '('),
        KEY_0 => ('0', ')'),
        KEY_MINUS => ('-', '_'),
        KEY_EQUAL => ('=', '+'),
        KEY_LEFTBRACE => ('[', '{'),
        KEY_RIGHTBRACE => (']', '}'),
        KEY_SEMICOLON => (';', ':'),
        KEY_APOSTROPHE => ('\'', '"'),
        KEY_GRAVE => ('`', '~'),
        KEY_BACKSLASH | KEY_102ND => ('\\', '|'),
        KEY_COMMA => (',', '<'),
        KEY_DOT => ('.', '>'),
        KEY_SLASH => ('/', '?'),
        KEY_SPACE => (' ', ' '),
        _ => {
            let letter = letter(code)?;
            (letter, letter.to_ascii_uppercase())
        }
    };
    Some(pair)
}

/// The letter of the key `code`, if it is one.
fn letter(code: u16) -> Option<char> {
    const ROWS: [(u16, &str); 3] =
        [(KEY_Q, "qwertyuiop"), (KEY_A, "asdfghjkl"), (KEY_Z, "zxcvbnm")];
    ROWS.iter().find_map(|&(first, letters)| {
        let at = usize::from(code.checked_sub(first)?);
        letters.as_bytes().get(at).map(|&b| char::from(b))
    })
}

/// The control character Ctrl and `c` type, as on a VT100.
fn control(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '@' | ' ' | '2' => 0,
        '[' | '{' | '3' => 0x1b,
        '\\' | '|' | '4' => 0x1c,
        ']' | '}' | '5' => 0x1d,
        '^' | '~' | '6' => 0x1e,
        '_' | '-' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Linux's sequence for function key `n` (1 to 12).
fn function(n: u16) -> &'static [u8] {
    const KEYS: [&[u8]; 12] = [
        b"\x1b[[A",
        b"\x1b[[B",
        b"\x1b[[C",
        b"\x1b[[D",
        b"\x1b[[E",
        b"\x1b[17~",
        b"\x1b[18~",
        b"\x1b[19~",
        b"\x1b[20~",
        b"\x1b[21~",
        b"\x1b[23~",
        b"\x1b[24~",
    ];
    KEYS[usize::from(n - 1)]
}

/// The function key number of `code`, 1 to 12.
fn function_number(code: u16) -> Option<u16> {
    match code {
        KEY_F1..=KEY_F10 => Some(code - KEY_F1 + 1),
        KEY_F11 => Some(11),
        KEY_F12 => Some(12),
        _ => None,
    }
}

impl Keyboard {
    /// A keyboard with Num Lock on, so the keypad types digits.
    pub fn new() -> Keyboard {
        Keyboard { num: true, ..Keyboard::default() }
    }

    fn shift(&self) -> bool {
        self.shift[0] || self.shift[1]
    }

    fn ctrl(&self) -> bool {
        self.ctrl[0] || self.ctrl[1]
    }

    fn alt(&self) -> bool {
        self.alt[0] || self.alt[1]
    }

    /// The input(2) LED_* bits for the lock keys that are on.
    pub fn lights(&self) -> u32 {
        let mut bits = 0;
        if self.caps {
            bits |= LED_CAPS_LOCK;
        }
        if self.num {
            bits |= LED_NUM_LOCK;
        }
        if self.scroll {
            bits |= LED_SCROLL_LOCK;
        }
        bits
    }

    /// What the key `code` going `value` (input(2)'s RELEASED, PRESSED or
    /// REPEATED) does, for a console in `modes`.
    pub fn key(&mut self, code: u16, value: u16, modes: Modes) -> Action {
        let down = value != RELEASED;
        match code {
            KEY_LEFTSHIFT => self.shift[0] = down,
            KEY_RIGHTSHIFT => self.shift[1] = down,
            KEY_LEFTCTRL => self.ctrl[0] = down,
            KEY_RIGHTCTRL => self.ctrl[1] = down,
            KEY_LEFTALT => self.alt[0] = down,
            KEY_RIGHTALT => self.alt[1] = down,
            KEY_CAPSLOCK | KEY_NUMLOCK | KEY_SCROLLLOCK if value == PRESSED => {
                let lock = match code {
                    KEY_CAPSLOCK => &mut self.caps,
                    KEY_NUMLOCK => &mut self.num,
                    _ => &mut self.scroll,
                };
                *lock = !*lock;
                return Action::Lights;
            }
            _ if down => return self.pressed(code, modes),
            _ => {}
        }
        Action::None
    }

    fn pressed(&self, code: u16, modes: Modes) -> Action {
        let (shift, ctrl, alt) = (self.shift(), self.ctrl(), self.alt());
        if let Some(n) = function_number(code) {
            if alt {
                return Action::Switch(usize::from(n - 1));
            }
            return send(function(n));
        }
        let cursor = |letter: u8| {
            let lead: &[u8] = if modes.app_cursor { b"\x1bO" } else { b"\x1b[" };
            Action::Send([lead, &[letter]].concat())
        };
        let applic = modes.app_keypad && !shift;
        // The keypad: digits with Num Lock, the editing keys without.
        let keypad = |digit: u8, applic_letter: u8, without: Action| {
            if applic {
                Action::Send(vec![0x1b, b'O', applic_letter])
            } else if self.num != shift {
                Action::Send(vec![digit])
            } else {
                without
            }
        };
        match code {
            KEY_ESC => send(b"\x1b"),
            KEY_ENTER | KEY_KPENTER => {
                if code == KEY_KPENTER && applic {
                    send(b"\x1bOM")
                } else if modes.newline {
                    send(b"\r\n")
                } else {
                    send(b"\r")
                }
            }
            KEY_BACKSPACE => match (ctrl, alt) {
                (true, _) => send(b"\x08"),
                (_, true) => send(b"\x1b\x7f"),
                _ => send(b"\x7f"),
            },
            KEY_TAB if shift => send(b"\x1b[Z"),
            KEY_TAB => send(b"\t"),
            KEY_UP => cursor(b'A'),
            KEY_DOWN => cursor(b'B'),
            KEY_LEFT if alt => Action::SwitchBy(-1),
            KEY_RIGHT if alt => Action::SwitchBy(1),
            KEY_RIGHT => cursor(b'C'),
            KEY_LEFT => cursor(b'D'),
            KEY_HOME => send(b"\x1b[1~"),
            KEY_INSERT => send(b"\x1b[2~"),
            KEY_DELETE if ctrl && alt => Action::Reboot,
            KEY_DELETE => send(b"\x1b[3~"),
            KEY_END => send(b"\x1b[4~"),
            KEY_PAGEUP if shift => Action::Page(1),
            KEY_PAGEDOWN if shift => Action::Page(-1),
            KEY_PAGEUP => send(b"\x1b[5~"),
            KEY_PAGEDOWN => send(b"\x1b[6~"),
            KEY_KP0 => keypad(b'0', b'p', send(b"\x1b[2~")),
            KEY_KP1 => keypad(b'1', b'q', send(b"\x1b[4~")),
            KEY_KP2 => keypad(b'2', b'r', cursor(b'B')),
            KEY_KP3 => keypad(b'3', b's', send(b"\x1b[6~")),
            KEY_KP4 => keypad(b'4', b't', cursor(b'D')),
            KEY_KP5 => keypad(b'5', b'u', send(b"\x1b[G")),
            KEY_KP6 => keypad(b'6', b'v', cursor(b'C')),
            KEY_KP7 => keypad(b'7', b'w', send(b"\x1b[1~")),
            KEY_KP8 => keypad(b'8', b'x', cursor(b'A')),
            KEY_KP9 => keypad(b'9', b'y', send(b"\x1b[5~")),
            KEY_KPDOT if ctrl && alt => Action::Reboot,
            KEY_KPDOT => keypad(b'.', b'n', send(b"\x1b[3~")),
            KEY_KPSLASH if applic => send(b"\x1bOo"),
            KEY_KPASTERISK if applic => send(b"\x1bOj"),
            KEY_KPMINUS if applic => send(b"\x1bOm"),
            KEY_KPPLUS if applic => send(b"\x1bOl"),
            KEY_KPSLASH => send(b"/"),
            KEY_KPASTERISK => send(b"*"),
            KEY_KPMINUS => send(b"-"),
            KEY_KPPLUS => send(b"+"),
            KEY_KPEQUAL => send(b"="),
            KEY_KPCOMMA => send(b","),
            _ => {
                let Some((plain, shifted)) = us(code) else { return Action::None };
                let upper = shift != (self.caps && letter(code).is_some());
                let c = if upper { shifted } else { plain };
                let mut bytes = Vec::with_capacity(2);
                if alt {
                    bytes.push(0x1b);
                }
                match ctrl.then(|| control(c)).flatten() {
                    Some(byte) => bytes.push(byte),
                    None => bytes.push(c as u8),
                }
                Action::Send(bytes)
            }
        }
    }
}

fn send(bytes: &[u8]) -> Action {
    Action::Send(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Modes = Modes {
        autowrap: true,
        insert: false,
        origin: false,
        cursor_visible: true,
        app_cursor: false,
        app_keypad: false,
        newline: false,
        reverse_screen: false,
    };

    fn typed(keyboard: &mut Keyboard, keys: &[u16]) -> Vec<Action> {
        keys.iter().map(|&code| keyboard.key(code, PRESSED, PLAIN)).collect()
    }

    fn bytes(action: &Action) -> &[u8] {
        match action {
            Action::Send(bytes) => bytes,
            _ => &[],
        }
    }

    #[test]
    fn letters_shift_and_caps_lock() {
        let mut keyboard = Keyboard::new();
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_H])[0]), b"h");
        keyboard.key(KEY_LEFTSHIFT, PRESSED, PLAIN);
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_H])[0]), b"H");
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_1])[0]), b"!");
        keyboard.key(KEY_LEFTSHIFT, RELEASED, PLAIN);
        assert_eq!(keyboard.key(KEY_CAPSLOCK, PRESSED, PLAIN), Action::Lights);
        assert_eq!(keyboard.lights(), LED_CAPS_LOCK | LED_NUM_LOCK);
        // Caps Lock changes letters only.
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_M])[0]), b"M");
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_SLASH])[0]), b"/");
        // A held lock key does not toggle again as it repeats.
        assert_eq!(keyboard.key(KEY_CAPSLOCK, REPEATED, PLAIN), Action::None);
        assert!(keyboard.caps);
    }

    #[test]
    fn control_and_alt() {
        let mut keyboard = Keyboard::new();
        keyboard.key(KEY_LEFTCTRL, PRESSED, PLAIN);
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_C])[0]), [3]);
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_LEFTBRACE])[0]), [0x1b]);
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_SPACE])[0]), [0]);
        keyboard.key(KEY_LEFTCTRL, RELEASED, PLAIN);
        keyboard.key(KEY_LEFTALT, PRESSED, PLAIN);
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_B])[0]), b"\x1bb");
        assert_eq!(typed(&mut keyboard, &[KEY_F2])[0], Action::Switch(1));
        assert_eq!(typed(&mut keyboard, &[KEY_LEFT])[0], Action::SwitchBy(-1));
        keyboard.key(KEY_LEFTCTRL, PRESSED, PLAIN);
        assert_eq!(typed(&mut keyboard, &[KEY_DELETE])[0], Action::Reboot);
        assert_eq!(typed(&mut keyboard, &[KEY_F3])[0], Action::Switch(2));
    }

    #[test]
    fn editing_and_function_keys() {
        let mut keyboard = Keyboard::new();
        let keys = [KEY_UP, KEY_HOME, KEY_DELETE, KEY_F1, KEY_F12, KEY_BACKSPACE, KEY_ENTER];
        let sent: Vec<Vec<u8>> =
            typed(&mut keyboard, &keys).iter().map(|a| bytes(a).to_vec()).collect();
        let expected: Vec<&[u8]> =
            vec![b"\x1b[A", b"\x1b[1~", b"\x1b[3~", b"\x1b[[A", b"\x1b[24~", b"\x7f", b"\r"];
        assert_eq!(sent, expected);
        let app = Modes { app_cursor: true, app_keypad: true, ..PLAIN };
        assert_eq!(bytes(&keyboard.key(KEY_UP, PRESSED, app)), b"\x1bOA");
        assert_eq!(bytes(&keyboard.key(KEY_KP5, PRESSED, app)), b"\x1bOu");
        keyboard.key(KEY_LEFTSHIFT, PRESSED, PLAIN);
        assert_eq!(keyboard.key(KEY_PAGEUP, PRESSED, PLAIN), Action::Page(1));
        assert_eq!(bytes(&keyboard.key(KEY_TAB, PRESSED, PLAIN)), b"\x1b[Z");
    }

    #[test]
    fn the_keypad_follows_num_lock() {
        let mut keyboard = Keyboard::new();
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_KP7])[0]), b"7");
        keyboard.key(KEY_NUMLOCK, PRESSED, PLAIN);
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_KP7])[0]), b"\x1b[1~");
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_KP8])[0]), b"\x1b[A");
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_KPPLUS])[0]), b"+");
    }
}
