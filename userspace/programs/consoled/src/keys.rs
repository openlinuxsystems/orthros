// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Turning keys into what a terminal sends, as Linux's console does: the
//! characters of the keyboard layout (ADR 0149), with AltGr and dead keys
//! where it has them, control characters with Ctrl, ESC before them with
//! Alt, and the escape sequences of terminfo's `linux` entry for the
//! cursor, editing, function and keypad keys. The keys consoled keeps for
//! itself: Alt+F1 to Alt+F6 (with or without Ctrl) switch consoles (Alt+Left
//! and Alt+Right are the programs', which xterm's way sends, for the
//! shell's folder history: ADR 0262), Shift+Page Up and Shift+Page Down look back,
//! and Ctrl+Alt+Delete is the secure attention key (ADR 0251).

use keymap::{Dead, Keymap, Sym, compose};
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
    /// Look back half a screen (1) or forward (-1).
    Page(isize),
    /// Ctrl+Alt+Delete, the secure attention key: consoled's own prompt
    /// or menu.
    Attention,
    /// A lock key changed: light the keyboard's lights again.
    Lights,
}

/// The layout, and the state of the modifier, lock and dead keys.
#[derive(Clone, Debug)]
pub struct Keyboard {
    keymap: Keymap,
    /// The US layout, for Ctrl with a key whose character has no control
    /// character, as on Linux.
    us: Keymap,
    shift: [bool; 2],
    ctrl: [bool; 2],
    alt: [bool; 2],
    /// Right Alt, in a layout where it is AltGr.
    altgr: bool,
    /// The dead key pressed last, waiting for its letter.
    dead: Option<Dead>,
    /// Caps Lock.
    pub caps: bool,
    /// Num Lock.
    pub num: bool,
    /// Scroll Lock: the console's output is held while it is on.
    pub scroll: bool,
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
    /// A keyboard of `keymap`'s layout with Num Lock on, so the keypad
    /// types digits.
    pub fn new(keymap: Keymap) -> Keyboard {
        Keyboard {
            keymap,
            us: Keymap::us(),
            shift: [false; 2],
            ctrl: [false; 2],
            alt: [false; 2],
            altgr: false,
            dead: None,
            caps: false,
            num: true,
            scroll: false,
        }
    }

    /// Types in `keymap` from now; Right Alt and a dead key waiting start
    /// afresh.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
        self.alt[1] = false;
        self.altgr = false;
        self.dead = None;
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
            KEY_RIGHTALT if self.keymap.altgr => self.altgr = down,
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
            // A dead key does not repeat: held, it waits for its letter.
            _ if value == REPEATED
                && matches!(
                    self.keymap.sym(code, self.shift(), self.altgr, self.caps),
                    Sym::Dead(_)
                ) => {}
            _ if down => return self.pressed(code, modes),
            _ => {}
        }
        Action::None
    }

    fn pressed(&mut self, code: u16, modes: Modes) -> Action {
        let (shift, ctrl, alt) = (self.shift(), self.ctrl(), self.alt());
        // Any other key than a layout's lets go of a dead key.
        let dead = self.dead.take();
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
        let keypad = |digit: char, applic_letter: u8, without: Action| {
            if applic {
                Action::Send(vec![0x1b, b'O', applic_letter])
            } else if self.num != shift {
                Action::Send(utf8(digit))
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
            KEY_LEFT if alt => Action::Send(b"\x1b[1;3D".to_vec()),
            KEY_RIGHT if alt => Action::Send(b"\x1b[1;3C".to_vec()),
            KEY_RIGHT => cursor(b'C'),
            KEY_LEFT => cursor(b'D'),
            KEY_HOME => send(b"\x1b[1~"),
            KEY_INSERT => send(b"\x1b[2~"),
            KEY_DELETE if ctrl && alt => Action::Attention,
            KEY_DELETE => send(b"\x1b[3~"),
            KEY_END => send(b"\x1b[4~"),
            KEY_PAGEUP if shift => Action::Page(1),
            KEY_PAGEDOWN if shift => Action::Page(-1),
            KEY_PAGEUP => send(b"\x1b[5~"),
            KEY_PAGEDOWN => send(b"\x1b[6~"),
            KEY_KP0 => keypad('0', b'p', send(b"\x1b[2~")),
            KEY_KP1 => keypad('1', b'q', send(b"\x1b[4~")),
            KEY_KP2 => keypad('2', b'r', cursor(b'B')),
            KEY_KP3 => keypad('3', b's', send(b"\x1b[6~")),
            KEY_KP4 => keypad('4', b't', cursor(b'D')),
            KEY_KP5 => keypad('5', b'u', send(b"\x1b[G")),
            KEY_KP6 => keypad('6', b'v', cursor(b'C')),
            KEY_KP7 => keypad('7', b'w', send(b"\x1b[1~")),
            KEY_KP8 => keypad('8', b'x', cursor(b'A')),
            KEY_KP9 => keypad('9', b'y', send(b"\x1b[5~")),
            KEY_KPDOT if ctrl && alt => Action::Attention,
            KEY_KPDOT => keypad(self.keymap.keypad_decimal(), b'n', send(b"\x1b[3~")),
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
            _ => self.typed(code, dead),
        }
    }

    /// What a key of the layout types, after the dead key `dead` if one
    /// was pressed: a letter it goes on takes its accent, a space or the
    /// dead key again types the accent alone, and anything else types the
    /// accent and then itself, as Linux's console does.
    fn typed(&mut self, code: u16, dead: Option<Dead>) -> Action {
        let (shift, ctrl, alt) = (self.shift(), self.ctrl(), self.alt());
        let c = match self.keymap.sym(code, shift, self.altgr, self.caps) {
            Sym::None => {
                self.dead = dead;
                return Action::None;
            }
            Sym::Dead(pressed) => match dead {
                Some(dead) if dead == pressed => return Action::Send(utf8(dead.spacing())),
                Some(dead) => {
                    self.dead = Some(pressed);
                    return Action::Send(utf8(dead.spacing()));
                }
                None => {
                    self.dead = Some(pressed);
                    return Action::None;
                }
            },
            Sym::Char(c) => c,
        };
        let mut bytes = Vec::with_capacity(8);
        if alt {
            bytes.push(0x1b);
        }
        if ctrl {
            let us = || match self.us.sym(code, shift, false, false) {
                Sym::Char(c) => control(c),
                _ => None,
            };
            if let Some(byte) = control(c).or_else(us) {
                bytes.push(byte);
                return Action::Send(bytes);
            }
        }
        match dead.map(|dead| (dead, compose(dead, c))) {
            Some((_, Some(composed))) => bytes.extend(utf8(composed)),
            Some((dead, None)) => {
                bytes.extend(utf8(dead.spacing()));
                bytes.extend(utf8(c));
            }
            None => bytes.extend(utf8(c)),
        }
        Action::Send(bytes)
    }
}

/// `c` in UTF-8.
fn utf8(c: char) -> Vec<u8> {
    c.encode_utf8(&mut [0; 4]).as_bytes().to_vec()
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
        let mut keyboard = Keyboard::new(Keymap::us());
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
        let mut keyboard = Keyboard::new(Keymap::us());
        keyboard.key(KEY_LEFTCTRL, PRESSED, PLAIN);
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_C])[0]), [3]);
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_LEFTBRACE])[0]), [0x1b]);
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_SPACE])[0]), [0]);
        keyboard.key(KEY_LEFTCTRL, RELEASED, PLAIN);
        keyboard.key(KEY_LEFTALT, PRESSED, PLAIN);
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_B])[0]), b"\x1bb");
        assert_eq!(typed(&mut keyboard, &[KEY_F2])[0], Action::Switch(1));
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_LEFT])[0]), b"\x1b[1;3D");
        keyboard.key(KEY_LEFTCTRL, PRESSED, PLAIN);
        assert_eq!(typed(&mut keyboard, &[KEY_DELETE])[0], Action::Attention);
        assert_eq!(typed(&mut keyboard, &[KEY_F3])[0], Action::Switch(2));
    }

    #[test]
    fn editing_and_function_keys() {
        let mut keyboard = Keyboard::new(Keymap::us());
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

    fn layout(name: &str) -> Keyboard {
        Keyboard::new(Keymap::builtin(name).unwrap())
    }

    /// What pressing `code` with `held` down sends.
    fn with(keyboard: &mut Keyboard, held: &[u16], code: u16) -> Action {
        for &key in held {
            keyboard.key(key, PRESSED, PLAIN);
        }
        let action = keyboard.key(code, PRESSED, PLAIN);
        for &key in held {
            keyboard.key(key, RELEASED, PLAIN);
        }
        action
    }

    #[test]
    fn a_layout_with_altgr_and_dead_keys() {
        let mut de = layout("de");
        assert_eq!(bytes(&typed(&mut de, &[KEY_Y])[0]), b"z");
        assert_eq!(bytes(&with(&mut de, &[KEY_LEFTSHIFT], KEY_SEMICOLON)), "Ö".as_bytes());
        assert_eq!(bytes(&with(&mut de, &[KEY_RIGHTALT], KEY_Q)), b"@");
        assert_eq!(bytes(&with(&mut de, &[KEY_RIGHTALT], KEY_8)), b"[");
        // Dead keys: a letter takes the accent; a space, or the dead key
        // again, types it alone; another key types both.
        assert_eq!(typed(&mut de, &[KEY_EQUAL])[0], Action::None);
        assert_eq!(bytes(&typed(&mut de, &[KEY_E])[0]), "é".as_bytes());
        assert_eq!(bytes(&typed(&mut de, &[KEY_EQUAL, KEY_SPACE])[1]), "´".as_bytes());
        assert_eq!(bytes(&typed(&mut de, &[KEY_GRAVE, KEY_GRAVE])[1]), b"^");
        assert_eq!(bytes(&typed(&mut de, &[KEY_GRAVE, KEY_X])[1]), "^x".as_bytes());
        assert_eq!(bytes(&with(&mut de, &[KEY_LEFTSHIFT], KEY_EQUAL)), b"");
        assert_eq!(bytes(&with(&mut de, &[KEY_LEFTSHIFT], KEY_A)), "À".as_bytes());
        // A dead key then another: the first's accent, the second waits.
        assert_eq!(bytes(&typed(&mut de, &[KEY_EQUAL, KEY_GRAVE])[1]), "´".as_bytes());
        assert_eq!(bytes(&typed(&mut de, &[KEY_O])[0]), "ô".as_bytes());
        // Another key lets go of it; a held one does not repeat.
        assert_eq!(typed(&mut de, &[KEY_EQUAL, KEY_LEFT, KEY_E])[2], Action::Send(b"e".to_vec()));
        de.key(KEY_EQUAL, PRESSED, PLAIN);
        assert_eq!(de.key(KEY_EQUAL, REPEATED, PLAIN), Action::None);
        assert_eq!(bytes(&typed(&mut de, &[KEY_U])[0]), "ú".as_bytes());
        // Ctrl with a letter of the layout, and with one that has no
        // control character: the key's US one.
        assert_eq!(bytes(&with(&mut de, &[KEY_LEFTCTRL], KEY_Y)), [0x1a]);
        assert_eq!(bytes(&with(&mut de, &[KEY_LEFTCTRL], KEY_LEFTBRACE)), [0x1b]);
        assert_eq!(bytes(&with(&mut de, &[KEY_LEFTALT], KEY_SEMICOLON)), "\x1bö".as_bytes());
        assert_eq!(bytes(&typed(&mut de, &[KEY_KPDOT])[0]), b",");
    }

    #[test]
    fn right_alt_is_alt_where_the_layout_has_no_altgr() {
        let mut us = Keyboard::new(Keymap::us());
        assert_eq!(bytes(&with(&mut us, &[KEY_RIGHTALT], KEY_B)), b"\x1bb");
        assert_eq!(with(&mut us, &[KEY_RIGHTALT], KEY_F2), Action::Switch(1));
        // Switching layouts with Right Alt held lets go of it.
        let mut keyboard = Keyboard::new(Keymap::us());
        keyboard.key(KEY_RIGHTALT, PRESSED, PLAIN);
        keyboard.set_keymap(Keymap::builtin("de").unwrap());
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_B])[0]), b"b");
        assert_eq!(keyboard.keymap.name, "German");
    }

    #[test]
    fn the_keypad_follows_num_lock() {
        let mut keyboard = Keyboard::new(Keymap::us());
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_KP7])[0]), b"7");
        keyboard.key(KEY_NUMLOCK, PRESSED, PLAIN);
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_KP7])[0]), b"\x1b[1~");
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_KP8])[0]), b"\x1b[A");
        assert_eq!(bytes(&typed(&mut keyboard, &[KEY_KPPLUS])[0]), b"+");
    }
}
