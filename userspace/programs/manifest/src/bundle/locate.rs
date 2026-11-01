// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Where a value is in a manifest's text, so that an error found after
//! reading it says its line: the key of an object's member, or the start
//! of an array's element, by the path of keys and indexes that leads to
//! it. The text has already been read as JSON.

/// The offset in `text` of what `path` leads to: the key of the last
/// member it names, or the element of the last index; none if the text
/// has no such place.
pub(crate) fn locate(text: &str, path: &[Step<'_>]) -> Option<usize> {
    let mut scan = Scan { bytes: text.as_bytes(), at: 0 };
    scan.space();
    scan.find(path)
}

/// A step of a path: an object's member by its key, or an array's element
/// by its index.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

struct Scan<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    fn space(&mut self) {
        while self.bytes.get(self.at).is_some_and(|b| b" \t\r\n".contains(b)) {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    /// Reads a string at the cursor, and gives whether it is `want`
    /// once unescaped (escapes compare as written, which a key with none
    /// matches).
    fn string_is(&mut self, want: &str) -> bool {
        self.at += 1;
        let start = self.at;
        while let Some(b) = self.peek() {
            match b {
                b'\\' => self.at += 2,
                b'"' => break,
                _ => self.at += 1,
            }
        }
        let same = self.bytes.get(start..self.at) == Some(want.as_bytes());
        self.at += 1;
        same
    }

    /// Skips the value at the cursor.
    fn skip(&mut self) {
        match self.peek() {
            Some(b'"') => {
                self.string_is("");
            }
            Some(open @ (b'{' | b'[')) => {
                let close = if open == b'{' { b'}' } else { b']' };
                self.at += 1;
                self.space();
                if self.peek() == Some(close) {
                    self.at += 1;
                    return;
                }
                loop {
                    if open == b'{' {
                        self.string_is("");
                        self.space();
                        self.at += 1; // :
                        self.space();
                    }
                    self.skip();
                    self.space();
                    match self.peek() {
                        Some(b',') => {
                            self.at += 1;
                            self.space();
                        }
                        _ => {
                            self.at += 1;
                            return;
                        }
                    }
                }
            }
            _ => {
                while self.peek().is_some_and(|b| !b",}] \t\r\n".contains(&b)) {
                    self.at += 1;
                }
            }
        }
    }

    /// Follows `path` from the value at the cursor.
    fn find(&mut self, path: &[Step<'_>]) -> Option<usize> {
        let (step, rest) = path.split_first()?;
        match (self.peek()?, step) {
            (b'{', Step::Key(key)) => {
                self.at += 1;
                loop {
                    self.space();
                    if self.peek()? != b'"' {
                        return None;
                    }
                    let at = self.at;
                    let hit = self.string_is(key);
                    self.space();
                    self.at += 1; // :
                    self.space();
                    if hit {
                        return if rest.is_empty() { Some(at) } else { self.find(rest) };
                    }
                    self.skip();
                    self.space();
                    if self.peek()? != b',' {
                        return None;
                    }
                    self.at += 1;
                }
            }
            (b'[', Step::Index(index)) => {
                self.at += 1;
                for _ in 0..*index {
                    self.space();
                    self.skip();
                    self.space();
                    if self.peek()? != b',' {
                        return None;
                    }
                    self.at += 1;
                }
                self.space();
                if matches!(self.peek()?, b']') {
                    return None;
                }
                if rest.is_empty() { Some(self.at) } else { self.find(rest) }
            }
            _ => None,
        }
    }
}
