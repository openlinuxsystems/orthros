// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Reading the kernel's framebuffer console from a QEMU screenshot.
//!
//! The reference laptop has no serial port, so what the kernel draws on the
//! screen is a feature the boot tests check, not a side effect. A screenshot
//! (PPM, from QMP's `screendump`) is cut into the console's character cells,
//! and each cell is matched against the glyphs of the same built-in font the
//! kernel used, which gives back the text exactly; no OCR is involved. The
//! kernel logs the console's geometry on the serial port, which says where
//! the cells are.

use std::collections::HashMap;

use radix_fbcon::Font;

/// A decoded screenshot.
pub struct Image {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// RGB triples, row by row.
    pixels: Vec<[u8; 3]>,
}

impl Image {
    /// Parses a binary PPM (P6) file with 8-bit samples.
    pub fn parse_ppm(bytes: &[u8]) -> Result<Image, String> {
        let mut fields = Vec::new();
        let mut at = 0;
        // Magic, width, height and maximum value, separated by whitespace
        // and comments, then one whitespace byte before the samples.
        while fields.len() < 4 {
            while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'#') {
                if bytes[at] == b'#' {
                    while at < bytes.len() && bytes[at] != b'\n' {
                        at += 1;
                    }
                } else {
                    at += 1;
                }
            }
            let start = at;
            while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            if start == at {
                return Err("truncated PPM header".into());
            }
            fields.push(String::from_utf8_lossy(&bytes[start..at]).into_owned());
        }
        at += 1;
        if fields[0] != "P6" || fields[3] != "255" {
            return Err(format!("not an 8-bit binary PPM ({} {})", fields[0], fields[3]));
        }
        let number = |s: &str| s.parse::<usize>().map_err(|_| format!("bad PPM size `{s}`"));
        let (width, height) = (number(&fields[1])?, number(&fields[2])?);
        let data = bytes.get(at..at + width * height * 3).ok_or("truncated PPM data")?;
        let pixels = data.as_chunks::<3>().0.to_vec();
        Ok(Image { width, height, pixels })
    }

    fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        self.pixels[y * self.width + x]
    }
}

/// Where the console's cells are, as the kernel logged it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Columns and rows.
    pub cols: usize,
    /// Rows.
    pub rows: usize,
    /// Font name, such as `8x16`.
    pub font: (usize, usize),
    /// Pixel position of the top-left cell.
    pub origin: (usize, usize),
}

impl Geometry {
    /// Finds the kernel's line
    /// `Framebuffer: console of 160x50 cells with font 8x16 at (0, 0)`.
    pub fn from_log(log: &str) -> Option<Geometry> {
        let line = log.lines().find_map(|l| l.split("Framebuffer: console of ").nth(1))?;
        let (grid, rest) = line.split_once(" cells with font ")?;
        let (font, rest) = rest.split_once(" at (")?;
        let (x, y) = rest.trim_end_matches(')').split_once(", ")?;
        let pair = |s: &str| -> Option<(usize, usize)> {
            let (a, b) = s.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        };
        let (cols, rows) = pair(grid)?;
        Some(Geometry {
            cols,
            rows,
            font: pair(font)?,
            origin: (x.trim().parse().ok()?, y.trim().parse().ok()?),
        })
    }
}

/// Reads the console's text from a screenshot: one string per row, with
/// trailing blanks removed. A cell that matches no glyph reads as `?`.
pub fn read_text(image: &Image, geometry: Geometry) -> Result<Vec<String>, String> {
    let (fw, fh) = geometry.font;
    let name = format!("{fw}x{fh}");
    let font = radix_fbcon::builtin_font(&name).ok_or(format!("no built-in font {name}"))?;
    let (ox, oy) = geometry.origin;
    if ox + geometry.cols * fw > image.width || oy + geometry.rows * fh > image.height {
        return Err(format!(
            "the console ({}x{} cells of {name} at {ox},{oy}) does not fit a {}x{} screenshot",
            geometry.cols, geometry.rows, image.width, image.height
        ));
    }
    let glyphs = glyph_table(&font);
    let mut rows = Vec::with_capacity(geometry.rows);
    for row in 0..geometry.rows {
        let mut text = String::with_capacity(geometry.cols);
        for col in 0..geometry.cols {
            let bits = cell_bits(image, ox + col * fw, oy + row * fh, fw, fh);
            text.push(*glyphs.get(&bits).unwrap_or(&'?'));
        }
        rows.push(text.trim_end().to_string());
    }
    Ok(rows)
}

/// The lit pixels of every printable ASCII glyph, row by row.
fn glyph_table(font: &Font<'_>) -> HashMap<Vec<u32>, char> {
    let mut table = HashMap::new();
    for c in (' '..='~').rev() {
        let glyph = font.glyph(c);
        let bits: Vec<u32> = (0..font.height).map(|y| glyph.row_bits(y)).collect();
        table.insert(bits, c);
    }
    table
}

/// The pixels of one cell that differ from its background, which is the
/// colour most of its pixels have (glyphs light up far fewer than half).
fn cell_bits(image: &Image, x0: usize, y0: usize, width: usize, height: usize) -> Vec<u32> {
    let mut counts: HashMap<[u8; 3], usize> = HashMap::new();
    for y in y0..y0 + height {
        for x in x0..x0 + width {
            *counts.entry(image.pixel(x, y)).or_default() += 1;
        }
    }
    let background = counts.into_iter().max_by_key(|&(_, n)| n).map(|(c, _)| c).unwrap_or_default();
    (y0..y0 + height)
        .map(|y| {
            (0..width).fold(0u32, |bits, gx| {
                (bits << 1) | u32::from(image.pixel(x0 + gx, y) != background)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Draws `lines` with the 8x16 font, white on blue, into a PPM.
    fn render(lines: &[&str], width: usize, height: usize) -> Vec<u8> {
        let font = radix_fbcon::builtin_font("8x16").unwrap();
        let mut pixels = vec![[0u8, 0, 128]; width * height];
        for (row, line) in lines.iter().enumerate() {
            for (col, c) in line.chars().enumerate() {
                let glyph = font.glyph(c);
                for y in 0..16 {
                    for x in 0..8 {
                        if glyph.pixel(x, y) {
                            pixels[(4 + row * 16 + y) * width + 2 + col * 8 + x] = [255, 255, 255];
                        }
                    }
                }
            }
        }
        let mut out = format!("P6\n# test\n{width} {height}\n255\n").into_bytes();
        out.extend(pixels.iter().flatten());
        out
    }

    #[test]
    fn reads_back_drawn_text() {
        let ppm =
            render(&["Radix panic: oops", "  #0  0xffffffff80100000  radix::main+0x10"], 400, 40);
        let image = Image::parse_ppm(&ppm).unwrap();
        let geometry = Geometry { cols: 49, rows: 2, font: (8, 16), origin: (2, 4) };
        let rows = read_text(&image, geometry).unwrap();
        assert_eq!(rows, ["Radix panic: oops", "  #0  0xffffffff80100000  radix::main+0x10"]);
    }

    #[test]
    fn finds_the_geometry_in_the_log() {
        let log = "[    0.1 cpu0] Framebuffer: console of 160x50 cells with font 8x16 at (0, 0)\n";
        let geometry = Geometry::from_log(log).unwrap();
        assert_eq!(geometry, Geometry { cols: 160, rows: 50, font: (8, 16), origin: (0, 0) });
        assert!(Geometry::from_log("nothing here").is_none());
    }

    #[test]
    fn rejects_other_images() {
        assert!(Image::parse_ppm(b"P5\n1 1\n255\n\0").is_err());
        assert!(Image::parse_ppm(b"P6\n2 2\n255\n\0\0\0").is_err());
    }
}
