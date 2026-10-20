//! A fixed-capacity string for formatting without a heap.

use core::fmt;

/// A string of at most `N` bytes. Writes past the end are cut at a
/// character boundary and the overflow is dropped.
#[derive(Clone)]
pub struct TextBuf<const N: usize> {
    bytes: [u8; N],
    len: usize,
    truncated: bool,
}

impl<const N: usize> TextBuf<N> {
    /// An empty buffer.
    pub const fn new() -> Self {
        Self { bytes: [0; N], len: 0, truncated: false }
    }

    /// The text so far.
    pub fn as_str(&self) -> &str {
        // Only whole characters from `&str`s are ever copied in.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    /// Appends as much of `s` as fits.
    pub fn push_str(&mut self, s: &str) {
        let room = N - self.len;
        let take = floor_char_boundary(s, room);
        if take < s.len() {
            self.truncated = true;
        }
        self.bytes[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
    }

    /// Whether anything was dropped for lack of room.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Empties the buffer.
    pub fn clear(&mut self) {
        self.len = 0;
        self.truncated = false;
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> Default for TextBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> fmt::Write for TextBuf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_str(s);
        Ok(())
    }
}

impl<const N: usize> fmt::Display for TextBuf<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const N: usize> fmt::Debug for TextBuf<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// The largest index `<= max` that is a character boundary of `s`.
pub(crate) fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    (0..=max).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write;

    #[test]
    fn truncates_at_character_boundaries() {
        let mut buf = TextBuf::<5>::new();
        let accented = "éé";
        write!(buf, "ab{accented}").unwrap();
        assert_eq!(buf.as_str(), "abé");
        assert!(buf.truncated());
        buf.clear();
        buf.push_str("xyz");
        assert_eq!(buf.as_str(), "xyz");
        assert!(!buf.truncated());
    }
}
