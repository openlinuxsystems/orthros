// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Keyboard layouts for the consoles (ADR 0149): keymap(5) files, and what
//! a key types in one.
//!
//! A keymap names, for each key a console types with, what it types alone,
//! with Shift, with AltGr and with Shift and AltGr: a character, a dead key,
//! or nothing. Dukovo's layouts are made from xkeyboard-config's by
//! generate.py and built in ([`LAYOUTS`]); a file of one's own in
//! `/system/settings/keymaps` is read the same way. What a dead key makes
//! of the letter after it comes from Unicode's compositions ([`compose`]).

mod dead;
mod layouts;
#[cfg(test)]
mod tests;

use std::fmt;

pub use dead::Dead;
pub use layouts::LAYOUTS;
use radix_abi::input::*;

/// The keys a keymap gives levels to: their Linux names (radix-input(2)),
/// as keymap files name them, and their codes. `kpdot` is the keypad's
/// decimal key with Num Lock on.
pub const KEYS: [(&str, u16); 52] = [
    ("1", KEY_1),
    ("2", KEY_2),
    ("3", KEY_3),
    ("4", KEY_4),
    ("5", KEY_5),
    ("6", KEY_6),
    ("7", KEY_7),
    ("8", KEY_8),
    ("9", KEY_9),
    ("0", KEY_0),
    ("minus", KEY_MINUS),
    ("equal", KEY_EQUAL),
    ("q", KEY_Q),
    ("w", KEY_W),
    ("e", KEY_E),
    ("r", KEY_R),
    ("t", KEY_T),
    ("y", KEY_Y),
    ("u", KEY_U),
    ("i", KEY_I),
    ("o", KEY_O),
    ("p", KEY_P),
    ("leftbrace", KEY_LEFTBRACE),
    ("rightbrace", KEY_RIGHTBRACE),
    ("a", KEY_A),
    ("s", KEY_S),
    ("d", KEY_D),
    ("f", KEY_F),
    ("g", KEY_G),
    ("h", KEY_H),
    ("j", KEY_J),
    ("k", KEY_K),
    ("l", KEY_L),
    ("semicolon", KEY_SEMICOLON),
    ("apostrophe", KEY_APOSTROPHE),
    ("grave", KEY_GRAVE),
    ("backslash", KEY_BACKSLASH),
    ("z", KEY_Z),
    ("x", KEY_X),
    ("c", KEY_C),
    ("v", KEY_V),
    ("b", KEY_B),
    ("n", KEY_N),
    ("m", KEY_M),
    ("comma", KEY_COMMA),
    ("dot", KEY_DOT),
    ("slash", KEY_SLASH),
    ("space", KEY_SPACE),
    ("102nd", KEY_102ND),
    ("ro", KEY_RO),
    ("yen", KEY_YEN),
    ("kpdot", KEY_KPDOT),
];

/// The levels a key can have: alone, with Shift, with AltGr, and with
/// Shift and AltGr.
pub const LEVELS: usize = 4;

/// The longest keymap file read, in bytes.
pub const FILE_MAX: usize = 64 * 1024;

/// The setting that names the consoles' layout (keymap(1)).
pub const SETTING: &str = "/system/settings/keyboard";

/// Where one's own layouts are, each `NAME.keymap`; one there is used in
/// place of a built-in layout of its name.
pub const OWN_DIR: &str = "/system/settings/keymaps";

/// The layout used when the setting names none.
pub const DEFAULT: &str = "us";

/// What a key types at one level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sym {
    /// Nothing.
    #[default]
    None,
    /// A character.
    Char(char),
    /// A dead key: an accent for the next letter.
    Dead(Dead),
}

/// A keyboard layout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Keymap {
    /// What it is called, as its `name` line says.
    pub name: String,
    /// Right Alt is AltGr, which picks the third and fourth levels; when
    /// it is not, Right Alt is Alt, and those levels are not typed.
    pub altgr: bool,
    /// The levels of each key of [`KEYS`], in its order, and how many of
    /// them its line gave.
    keys: Vec<([Sym; LEVELS], usize)>,
}

/// Why a keymap file was refused: its line, from 1, and what is wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The line, from 1; 0 for the file as a whole.
    pub line: usize,
    /// What is wrong with it.
    pub what: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.what)
        } else {
            write!(f, "line {}: {}", self.line, self.what)
        }
    }
}

impl std::error::Error for Error {}

/// The index in [`KEYS`] of the key `code`.
fn index(code: u16) -> Option<usize> {
    KEYS.iter().position(|&(_, key)| key == code)
}

/// One level of a key as a keymap file writes it: `-` for nothing,
/// `dead:NAME` for a dead key, `U+XXXX` for a character by its number,
/// or the character itself.
fn sym(token: &str) -> Result<Sym, String> {
    if token == "-" {
        return Ok(Sym::None);
    }
    if let Some(name) = token.strip_prefix("dead:") {
        return Dead::ALL
            .iter()
            .find(|dead| dead.name() == name)
            .map(|&dead| Sym::Dead(dead))
            .ok_or(format!("there is no dead key called {name}"));
    }
    let c = match token.strip_prefix("U+") {
        Some(hex) if (4..=6).contains(&hex.len()) => u32::from_str_radix(hex, 16)
            .ok()
            .and_then(char::from_u32)
            .ok_or(format!("{token} is not a character"))?,
        _ => {
            let mut chars = token.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => c,
                _ => return Err(format!("`{token}` is not one character")),
            }
        }
    };
    if c.is_control() {
        return Err(format!("{token} is a control character"));
    }
    Ok(Sym::Char(c))
}

impl Keymap {
    /// The keymap `text` describes (keymap(5)).
    pub fn parse(text: &str) -> Result<Keymap, Error> {
        if text.len() > FILE_MAX {
            return Err(Error { line: 0, what: format!("it is over {FILE_MAX} bytes") });
        }
        let mut keymap =
            Keymap { keys: vec![([Sym::None; LEVELS], 0); KEYS.len()], ..Keymap::default() };
        let mut seen = [false; KEYS.len()];
        let mut named = false;
        for (at, line) in text.lines().enumerate() {
            let fail = |what: String| Error { line: at + 1, what };
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((setting, value)) = line.split_once('=')
                && matches!(setting.trim(), "name" | "rightalt")
            {
                let (setting, value) = (setting.trim(), value.trim());
                if setting == "name" {
                    if value.is_empty()
                        || value.chars().count() > 64
                        || value.chars().any(char::is_control)
                    {
                        return Err(fail("a name is 1 to 64 characters".into()));
                    }
                    keymap.name = value.into();
                    named = true;
                } else {
                    keymap.altgr = match value {
                        "altgr" => true,
                        "alt" => false,
                        _ => return Err(fail("rightalt is altgr or alt".into())),
                    };
                }
                continue;
            }
            let mut tokens = line.split_whitespace();
            let key = tokens.next().unwrap_or_default();
            let Some(i) = KEYS.iter().position(|&(name, _)| name == key) else {
                return Err(fail(format!("there is no key called {key}")));
            };
            if seen[i] {
                return Err(fail(format!("{key} is given twice")));
            }
            seen[i] = true;
            let (levels, count) = &mut keymap.keys[i];
            for token in tokens {
                if *count == LEVELS {
                    return Err(fail(format!("a key has at most {LEVELS} levels")));
                }
                levels[*count] = sym(token).map_err(fail)?;
                *count += 1;
            }
            if *count == 0 {
                return Err(fail(format!("{key} has no levels")));
            }
        }
        if !named {
            return Err(Error { line: 0, what: "it has no name line".into() });
        }
        Ok(keymap)
    }

    /// The built-in layout `name` ([`LAYOUTS`]).
    pub fn builtin(name: &str) -> Option<Keymap> {
        let &(_, text) = LAYOUTS.iter().find(|&&(layout, _)| layout == name)?;
        Keymap::parse(text).ok()
    }

    /// The US layout, Linux's default.
    pub fn us() -> Keymap {
        Keymap::builtin("us").unwrap_or_default()
    }

    /// What the key `code` types with Shift, AltGr and Caps Lock as given.
    /// A level its line does not give is the one without AltGr, and then
    /// the first: so Shift and AltGr leave the space bar a space. Caps
    /// Lock works as Shift on a key whose levels are a lowercase letter and
    /// its capital, as on Linux.
    pub fn sym(&self, code: u16, shift: bool, altgr: bool, caps: bool) -> Sym {
        let Some((levels, count)) = index(code).and_then(|i| self.keys.get(i)) else {
            return Sym::None;
        };
        let base = if altgr && self.altgr { 2 } else { 0 };
        let mut level = base + usize::from(shift);
        if caps
            && let (Sym::Char(lower), Sym::Char(upper)) = (levels[base], levels[base + 1])
            && lower.is_lowercase()
            && single_upper(lower) == Some(upper)
        {
            level ^= 1;
        }
        let level = [level, level & 1, 0].into_iter().find(|&level| level < *count);
        level.map_or(Sym::None, |level| levels[level])
    }

    /// What the keypad's decimal key types with Num Lock on: `.` unless
    /// the layout says otherwise.
    pub fn keypad_decimal(&self) -> char {
        match self.sym(KEY_KPDOT, false, false, false) {
            Sym::Char(c) => c,
            _ => '.',
        }
    }
}

/// The capital of `c`, if it is one character.
fn single_upper(c: char) -> Option<char> {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) => Some(u),
        _ => None,
    }
}

/// What the dead key `dead` makes of the letter `c` after it, if Unicode
/// composes them; a space after a dead key types its accent alone
/// ([`Dead::spacing`]).
pub fn compose(dead: Dead, c: char) -> Option<char> {
    if c == ' ' {
        return Some(dead.spacing());
    }
    let letters = dead.letters();
    letters.binary_search_by_key(&c, |&(letter, _)| letter).ok().map(|i| letters[i].1)
}

/// Whether `name` can name a layout: 1 to 32 lowercase letters, digits and
/// `-`, so that it is a file's name and nothing more.
pub fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The layout the setting `text` names: its `layout = NAME` line, or
/// [`DEFAULT`] when it has none.
pub fn layout_setting(text: &str) -> Result<String, Error> {
    let mut layout = DEFAULT.to_string();
    for (at, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fail = |what: &str| Error { line: at + 1, what: what.into() };
        let Some((setting, value)) = line.split_once('=') else {
            return Err(fail("a setting is `layout = NAME`"));
        };
        if setting.trim() != "layout" {
            return Err(fail("the only setting is layout"));
        }
        let value = value.trim();
        if !valid_name(value) {
            return Err(fail("a layout's name is 1 to 32 lowercase letters, digits and -"));
        }
        layout = value.into();
    }
    Ok(layout)
}

/// The text of the setting that names `layout`.
pub fn setting_text(layout: &str) -> String {
    format!(
        "# The keyboard layout the consoles type in: a name `keymap --list` shows\n\
         # (keymap(1)).\n\
         layout = {layout}\n"
    )
}

/// The layout the setting names: [`DEFAULT`] if there is no setting.
pub fn chosen() -> Result<String, String> {
    match std::fs::read_to_string(SETTING) {
        Ok(text) => layout_setting(&text).map_err(|err| format!("{SETTING}: {err}")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(DEFAULT.into()),
        Err(err) => Err(format!("{SETTING}: {err}")),
    }
}

/// The layout `name`: one's own file in [`OWN_DIR`] if there is one,
/// else the built-in one.
pub fn load(name: &str) -> Result<Keymap, String> {
    if !valid_name(name) {
        return Err(format!("`{name}` cannot name a layout"));
    }
    let path = format!("{OWN_DIR}/{name}.keymap");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes).map_err(|_| format!("{path}: not UTF-8"))?;
            Keymap::parse(&text).map_err(|err| format!("{path}: {err}"))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Keymap::builtin(name).ok_or(format!("there is no layout called {name}"))
        }
        Err(err) => Err(format!("{path}: {err}")),
    }
}

/// One's own layouts in [`OWN_DIR`], by name, sorted.
pub fn own() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(OWN_DIR)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let name = name.strip_suffix(".keymap")?;
            valid_name(name).then(|| name.to_string())
        })
        .collect();
    names.sort();
    names
}
