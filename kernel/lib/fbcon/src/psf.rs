//! PC Screen Font (PSF) parsing, versions 1 and 2.
//!
//! PSF is the bitmap font format of the Linux console. Each glyph is a
//! bitmap with one bit per pixel, rows padded to whole bytes, most
//! significant bit leftmost. An optional Unicode table maps code points to
//! glyphs. Reference: the `psf` description in the kbd project
//! (<https://www.win.tue.nl/~aeb/linux/kbd/font-formats-1.html>).

use core::fmt;

/// Why a font could not be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Neither PSF 1 nor PSF 2 magic.
    BadMagic,
    /// The header promises more data than there is.
    Truncated,
    /// Zero-sized glyphs, or a glyph size that does not match width and
    /// height.
    BadGeometry,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::BadMagic => "not a PSF font",
            Error::Truncated => "font file is truncated",
            Error::BadGeometry => "font has inconsistent glyph dimensions",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Table {
    None,
    /// PSF 1: little-endian u16 code points, 0xFFFE starts a sequence,
    /// 0xFFFF ends a glyph's entry.
    Ucs2,
    /// PSF 2: UTF-8, 0xFE starts a sequence, 0xFF ends a glyph's entry.
    Utf8,
}

/// A parsed bitmap font.
#[derive(Clone, Copy)]
pub struct Font<'a> {
    /// Glyph width in pixels.
    pub width: usize,
    /// Glyph height in pixels.
    pub height: usize,
    bytes_per_row: usize,
    glyph_size: usize,
    count: usize,
    glyphs: &'a [u8],
    table: Table,
    table_bytes: &'a [u8],
    /// Glyph index for code points below 256, `u16::MAX` where none.
    latin1: [u16; 256],
    fallback: usize,
}

impl fmt::Debug for Font<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Font")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("glyphs", &self.count)
            .finish()
    }
}

impl<'a> Font<'a> {
    /// Parses a PSF 1 or PSF 2 font.
    pub fn parse(bytes: &'a [u8]) -> Result<Font<'a>, Error> {
        let mut font = if bytes.starts_with(&[0x36, 0x04]) {
            Self::parse_psf1(bytes)?
        } else if bytes.starts_with(&[0x72, 0xb5, 0x4a, 0x86]) {
            Self::parse_psf2(bytes)?
        } else {
            return Err(Error::BadMagic);
        };
        font.build_latin1();
        font.fallback = font.lookup('?').or_else(|| font.lookup(' ')).unwrap_or(0);
        Ok(font)
    }

    fn parse_psf1(bytes: &'a [u8]) -> Result<Font<'a>, Error> {
        let mode = *bytes.get(2).ok_or(Error::Truncated)?;
        let height = usize::from(*bytes.get(3).ok_or(Error::Truncated)?);
        let count = if mode & 0x01 != 0 { 512 } else { 256 };
        if height == 0 {
            return Err(Error::BadGeometry);
        }
        let end = 4 + count * height;
        let glyphs = bytes.get(4..end).ok_or(Error::Truncated)?;
        let has_table = mode & 0x06 != 0;
        Ok(Font {
            width: 8,
            height,
            bytes_per_row: 1,
            glyph_size: height,
            count,
            glyphs,
            table: if has_table { Table::Ucs2 } else { Table::None },
            table_bytes: if has_table { &bytes[end..] } else { &[] },
            latin1: [u16::MAX; 256],
            fallback: 0,
        })
    }

    fn parse_psf2(bytes: &'a [u8]) -> Result<Font<'a>, Error> {
        let word = |i: usize| -> Result<usize, Error> {
            let raw = bytes.get(4 * i..4 * i + 4).ok_or(Error::Truncated)?;
            Ok(u32::from_le_bytes(raw.try_into().map_err(|_| Error::Truncated)?) as usize)
        };
        let (header_size, flags, count, glyph_size, height, width) =
            (word(2)?, word(3)?, word(4)?, word(5)?, word(6)?, word(7)?);
        let bytes_per_row = width.div_ceil(8);
        if width == 0 || height == 0 || count == 0 || glyph_size != bytes_per_row * height {
            return Err(Error::BadGeometry);
        }
        let end = count
            .checked_mul(glyph_size)
            .and_then(|size| size.checked_add(header_size))
            .ok_or(Error::Truncated)?;
        let glyphs = bytes.get(header_size..end).ok_or(Error::Truncated)?;
        let has_table = flags & 1 != 0;
        Ok(Font {
            width,
            height,
            bytes_per_row,
            glyph_size,
            count,
            glyphs,
            table: if has_table { Table::Utf8 } else { Table::None },
            table_bytes: if has_table { &bytes[end..] } else { &[] },
            latin1: [u16::MAX; 256],
            fallback: 0,
        })
    }

    /// Calls `f(glyph, code point)` for every single-code-point mapping in
    /// the Unicode table, stopping when `f` returns `true`.
    fn scan_table(&self, mut f: impl FnMut(usize, u32) -> bool) {
        match self.table {
            Table::None => {}
            Table::Ucs2 => {
                let mut glyph = 0;
                let mut in_sequence = false;
                for pair in self.table_bytes.as_chunks::<2>().0 {
                    match u16::from_le_bytes(*pair) {
                        0xffff => {
                            glyph += 1;
                            in_sequence = false;
                        }
                        0xfffe => in_sequence = true,
                        code if !in_sequence => {
                            if f(glyph, u32::from(code)) {
                                return;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Table::Utf8 => {
                let mut glyph = 0;
                let mut in_sequence = false;
                let mut rest = self.table_bytes;
                while let Some(&byte) = rest.first() {
                    match byte {
                        0xff => {
                            glyph += 1;
                            in_sequence = false;
                            rest = &rest[1..];
                        }
                        0xfe => {
                            in_sequence = true;
                            rest = &rest[1..];
                        }
                        _ => {
                            let len = match byte {
                                0x00..=0x7f => 1,
                                0xc0..=0xdf => 2,
                                0xe0..=0xef => 3,
                                _ => 4,
                            };
                            let Some(encoded) = rest.get(..len) else { return };
                            rest = &rest[len..];
                            let code = core::str::from_utf8(encoded)
                                .ok()
                                .and_then(|s| s.chars().next())
                                .map(u32::from);
                            if let (false, Some(code)) = (in_sequence, code)
                                && f(glyph, code)
                            {
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    fn build_latin1(&mut self) {
        if self.table == Table::None {
            for (code, slot) in self.latin1.iter_mut().enumerate().take(self.count) {
                *slot = code as u16;
            }
            return;
        }
        let mut latin1 = [u16::MAX; 256];
        self.scan_table(|glyph, code| {
            if let Some(slot) = latin1.get_mut(code as usize)
                && *slot == u16::MAX
            {
                *slot = glyph as u16;
            }
            false
        });
        self.latin1 = latin1;
    }

    /// The glyph index for `c`, if the font has one.
    pub fn lookup(&self, c: char) -> Option<usize> {
        let code = u32::from(c);
        if code < 256 {
            let index = self.latin1[code as usize];
            return (index != u16::MAX).then_some(usize::from(index));
        }
        let mut found = None;
        self.scan_table(|glyph, mapped| {
            if mapped == code {
                found = Some(glyph);
            }
            found.is_some()
        });
        found.filter(|&glyph| glyph < self.count)
    }

    /// The bitmap of the glyph for `c`, or of `?` when the font lacks it.
    pub fn glyph(&self, c: char) -> Glyph<'a> {
        let index = self.lookup(c).unwrap_or(self.fallback);
        let start = index * self.glyph_size;
        Glyph {
            bytes: &self.glyphs[start..start + self.glyph_size],
            bytes_per_row: self.bytes_per_row,
            width: self.width,
        }
    }
}

/// One glyph's bitmap.
#[derive(Clone, Copy, Debug)]
pub struct Glyph<'a> {
    bytes: &'a [u8],
    bytes_per_row: usize,
    width: usize,
}

impl Glyph<'_> {
    /// Whether the pixel at (`x`, `y`) is set.
    pub fn pixel(&self, x: usize, y: usize) -> bool {
        let byte = self.bytes[y * self.bytes_per_row + x / 8];
        byte & (0x80 >> (x % 8)) != 0
    }

    /// The bits of row `y`, leftmost pixel in bit `width - 1`.
    pub fn row_bits(&self, y: usize) -> u32 {
        let row = &self.bytes[y * self.bytes_per_row..(y + 1) * self.bytes_per_row];
        let packed = row.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
        (packed >> (self.bytes_per_row * 8 - self.width)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psf2(width: u32, height: u32, glyphs: &[&[u8]], table: Option<&[&str]>) -> Vec<u8> {
        let size = width.div_ceil(8) * height;
        let mut bytes = Vec::new();
        for word in [0x864a_b572, 0, 32, u32::from(table.is_some())] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        for word in [glyphs.len() as u32, size, height, width] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        for glyph in glyphs {
            assert_eq!(glyph.len(), size as usize);
            bytes.extend_from_slice(glyph);
        }
        if let Some(table) = table {
            for entry in table {
                bytes.extend_from_slice(entry.as_bytes());
                bytes.push(0xff);
            }
        }
        bytes
    }

    #[test]
    fn parses_psf2_with_unicode_table() {
        let a = [0x80, 0x40];
        let q = [0x01, 0x02];
        let bytes = psf2(8, 2, &[&a, &q], Some(&["Aä", "?é"]));
        let font = Font::parse(&bytes).unwrap();
        assert_eq!((font.width, font.height), (8, 2));
        assert_eq!(font.lookup('A'), Some(0));
        assert_eq!(font.lookup('ä'), Some(0));
        assert_eq!(font.lookup('é'), Some(1));
        assert_eq!(font.lookup('Z'), None);
        let glyph = font.glyph('Z');
        assert!(glyph.pixel(7, 0) && glyph.pixel(6, 1) && !glyph.pixel(0, 0));
        assert_eq!(font.glyph('A').row_bits(0), 0x80);
    }

    #[test]
    fn wide_glyph_rows() {
        let glyph = [0b1000_0000, 0b0001_0000];
        let bytes = psf2(12, 1, &[&glyph], None);
        let font = Font::parse(&bytes).unwrap();
        let g = font.glyph('\0');
        assert!(g.pixel(0, 0) && g.pixel(11, 0) && !g.pixel(10, 0));
        assert_eq!(g.row_bits(0), 0b1000_0000_0001);
    }

    #[test]
    fn rejects_bad_fonts() {
        assert_eq!(Font::parse(b"nope").unwrap_err(), Error::BadMagic);
        let mut bytes = psf2(8, 2, &[&[0, 0]], None);
        bytes.truncate(33);
        assert_eq!(Font::parse(&bytes).unwrap_err(), Error::Truncated);
        let mut bytes = psf2(8, 2, &[&[0, 0]], None);
        bytes[20..24].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(Font::parse(&bytes).unwrap_err(), Error::BadGeometry);
    }

    #[test]
    fn parses_psf1() {
        let mut bytes = vec![0x36, 0x04, 0x02, 1];
        bytes.extend((0..256).map(|i| i as u8));
        // Glyph 0x41 maps to 'A' and U+0391; others map to nothing.
        for glyph in 0..256u16 {
            if glyph == 0x41 {
                bytes.extend_from_slice(&0x41u16.to_le_bytes());
                bytes.extend_from_slice(&0x391u16.to_le_bytes());
            }
            bytes.extend_from_slice(&0xffffu16.to_le_bytes());
        }
        let font = Font::parse(&bytes).unwrap();
        assert_eq!((font.width, font.height), (8, 1));
        assert_eq!(font.lookup('A'), Some(0x41));
        assert_eq!(font.lookup('\u{391}'), Some(0x41));
        assert_eq!(font.lookup('B'), None);
    }
}
