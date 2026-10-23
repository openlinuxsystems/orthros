// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bytes as text, and back, losing nothing.
//!
//! The shell works on Rust strings, but file names, arguments, the
//! environment and the output of commands are bytes, and need not be
//! UTF-8. Each byte that is not part of valid UTF-8 is held as one
//! character of a range of the last private use plane, U+10FF80 to
//! U+10FFFF (the byte plus U+10FF00), and turns back into that byte on its
//! way out, so `for f in *; do rm -- "$f"; done` works on any name. Those
//! characters are never typed, so text never means them by accident.

use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

/// The character that stands for a byte that is not valid UTF-8.
const BASE: u32 = 0x10_FF00;

/// The character for byte `byte` on its own: itself if ASCII, otherwise
/// the character that stands for it.
pub fn byte_char(byte: u8) -> char {
    if byte < 0x80 {
        char::from(byte)
    } else {
        // 0x10FF80..=0x10FFFF are all valid characters.
        char::from_u32(BASE + u32::from(byte)).unwrap_or(char::REPLACEMENT_CHARACTER)
    }
}

/// The byte a character stands for, if it is one of the stand-ins.
pub fn char_byte(c: char) -> Option<u8> {
    let value = u32::from(c);
    if (BASE + 0x80..=BASE + 0xff).contains(&value) { Some((value - BASE) as u8) } else { None }
}

/// Bytes to text.
pub fn decode(mut bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    loop {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                out.push_str(text);
                return out;
            }
            Err(error) => {
                let (good, rest) = bytes.split_at(error.valid_up_to());
                // SAFETY: from_utf8 checked the first valid_up_to bytes.
                out.push_str(unsafe { std::str::from_utf8_unchecked(good) });
                let bad = error.error_len().unwrap_or(rest.len());
                for &byte in &rest[..bad] {
                    out.push(byte_char(byte));
                }
                bytes = &rest[bad..];
            }
        }
    }
}

/// Text to bytes.
pub fn encode(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    encode_into(text, &mut out);
    out
}

/// Appends the bytes of `text` to `out`.
pub fn encode_into(text: &str, out: &mut Vec<u8>) {
    // Text with no stand-ins (the usual case) is its own bytes.
    if !text.chars().any(|c| char_byte(c).is_some()) {
        out.extend_from_slice(text.as_bytes());
        return;
    }
    let mut buf = [0; 4];
    for c in text.chars() {
        match char_byte(c) {
            Some(byte) => out.push(byte),
            None => out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        }
    }
}

pub fn from_os(os: &OsStr) -> String {
    decode(os.as_bytes())
}

pub fn to_os(text: &str) -> OsString {
    OsString::from_vec(encode(text))
}

/// Text as a C string; a NUL, which no C string can hold, ends it.
pub fn cstring(text: &str) -> CString {
    let mut bytes = encode(text);
    if let Some(nul) = bytes.iter().position(|&b| b == 0) {
        bytes.truncate(nul);
    }
    CString::new(bytes).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_any_bytes() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(encode(&decode(&all)), all);
        let mixed = b"caf\xc3\xa9 \xff\xfe ok \xe2\x82";
        let text = decode(mixed);
        assert!(text.starts_with("café "));
        assert_eq!(encode(&text), mixed);
    }

    #[test]
    fn stand_ins_count_as_one_character() {
        assert_eq!(decode(b"a\xffb").chars().count(), 3);
        assert_eq!(char_byte(byte_char(0xff)), Some(0xff));
        assert_eq!(char_byte('a'), None);
    }
}
