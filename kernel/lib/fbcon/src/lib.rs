// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Framebuffer text console for the Radix kernel.
//!
//! A machine without a serial port, such as the reference laptop, shows the
//! kernel's output only on its screen. Under UEFI there is no text mode, so
//! the kernel draws text itself into the linear framebuffer the boot loader
//! set up. [`Console`] keeps a grid of character cells, draws them with a
//! [PSF](psf) bitmap font, and on each [`Console::flush`] redraws only the
//! cells that changed since the last one. Scrolling moves the grid in RAM;
//! the framebuffer is never read, because reads from video memory are slow.
//!
//! The built-in fonts are Spleen by Frederic Cambus (BSD-2-Clause), in three
//! sizes; [`choose_font`] picks one for the screen. The grid's storage is
//! passed in, so the crate needs no heap and its tests run on the host.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod psf;

pub use psf::{Font, Glyph};

use radix_hal::Framebuffer;
use radix_klog::Style;

/// The built-in fonts, smallest first, as (name, PSF file).
pub const FONTS: [(&str, &[u8]); 3] = [
    ("8x16", include_bytes!("../fonts/spleen-8x16.psfu")),
    ("12x24", include_bytes!("../fonts/spleen-12x24.psfu")),
    ("16x32", include_bytes!("../fonts/spleen-16x32.psfu")),
];

/// The built-in font called `name`, such as `12x24`.
pub fn builtin_font(name: &str) -> Option<Font<'static>> {
    FONTS.iter().find(|(font, _)| *font == name).and_then(|(_, bytes)| Font::parse(bytes).ok())
}

/// Columns and rows the console should have at least, if a font allows.
const MIN_GRID: (usize, usize) = (100, 40);

/// Picks the largest built-in font that still gives a grid of at least 100
/// columns by 40 rows on a `width` x `height` screen, or the smallest font
/// if none does. Returns the font's name.
pub fn choose_font(width: usize, height: usize) -> &'static str {
    FONTS
        .iter()
        .rev()
        .filter_map(|&(name, bytes)| Some((name, Font::parse(bytes).ok()?)))
        .find(|(_, font)| width / font.width >= MIN_GRID.0 && height / font.height >= MIN_GRID.1)
        .map_or(FONTS[0].0, |(name, _)| name)
}

/// Cells of storage [`Console::new`] needs for a full grid: two per
/// character position, one for the text and one for what is on screen.
pub fn storage_cells(width: usize, height: usize, font: &Font<'_>) -> usize {
    2 * (width / font.width) * (height / font.height)
}

/// Number of distinct [`Style`]s.
const STYLES: usize = 6;

fn style_index(style: Style) -> u32 {
    match style {
        Style::Plain => 0,
        Style::Error => 1,
        Style::Warning => 2,
        Style::Debug => 3,
        Style::Emphasis => 4,
        Style::Banner => 5,
    }
}

/// A colour as (r, g, b).
type Rgb = (u8, u8, u8);

/// Foreground and background colour of each style.
const PALETTE: [(Rgb, Rgb); STYLES] = [
    ((0xc8, 0xc8, 0xc8), (0, 0, 0)),
    ((0xff, 0x60, 0x60), (0, 0, 0)),
    ((0xff, 0xd0, 0x40), (0, 0, 0)),
    ((0x88, 0x88, 0x88), (0, 0, 0)),
    ((0xff, 0xff, 0xff), (0, 0, 0)),
    ((0xff, 0xff, 0xff), (0xa0, 0x10, 0x10)),
];

/// Widest glyph the renderer supports, in pixels.
const MAX_GLYPH_WIDTH: usize = 32;

/// A cell holds a character in its low 21 bits and a style index above
/// them. Zero is a blank in the plain style, which is how cells start.
fn encode_cell(c: char, style: u32) -> u32 {
    if c == ' ' && PALETTE[style as usize].1 == PALETTE[0].1 {
        0
    } else {
        u32::from(c) | (style << 24)
    }
}

fn decode_cell(cell: u32) -> (char, usize) {
    let c = char::from_u32(cell & 0x1f_ffff).filter(|&c| c != '\0').unwrap_or(' ');
    (c, ((cell >> 24) as usize).min(STYLES - 1))
}

/// A text console drawn into a linear framebuffer.
pub struct Console {
    fb: Framebuffer,
    font: Font<'static>,
    cols: usize,
    rows: usize,
    /// Pixel position of the grid's top-left corner.
    origin: (usize, usize),
    /// The text, `cols * rows` cells, row by row.
    cells: &'static mut [u32],
    /// What the screen shows, in the same layout.
    drawn: &'static mut [u32],
    col: usize,
    row: usize,
    /// The cursor is past the last column; the next character wraps.
    wrap_pending: bool,
    /// Encoded pixel values: (foreground, background) per style.
    colors: [(u32, u32); STYLES],
    cells_drawn: u64,
}

impl core::fmt::Debug for Console {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Console")
            .field("framebuffer", &self.fb)
            .field("font", &self.font)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .finish()
    }
}

impl Console {
    /// Takes over `fb`, clears it and returns an empty console. The grid
    /// is centred on the screen. If `storage` holds fewer than
    /// [`storage_cells`] cells, the grid gets fewer rows. Returns `None`
    /// for framebuffers this renderer cannot draw on (fewer than 16
    /// columns or 4 rows, or an unusual pixel size).
    pub fn new(fb: Framebuffer, font: Font<'static>, storage: &'static mut [u32]) -> Option<Self> {
        let bpp = usize::from(fb.format.bytes_per_pixel);
        if !(2..=4).contains(&bpp) || font.width > MAX_GLYPH_WIDTH || fb.pitch < fb.width * bpp {
            return None;
        }
        let height = fb.height.min(fb.memory.len() / fb.pitch.max(1));
        let cols = fb.width / font.width;
        let rows = (height / font.height).min(storage.len() / 2 / cols.max(1));
        if cols < 16 || rows < 4 {
            return None;
        }
        let origin = ((fb.width - cols * font.width) / 2, (height - rows * font.height) / 2);
        let colors = PALETTE.map(|((fr, fg, fb_), (br, bg, bb))| {
            (fb.format.encode(fr, fg, fb_), fb.format.encode(br, bg, bb))
        });
        let (cells, rest) = storage.split_at_mut(cols * rows);
        let (drawn, _) = rest.split_at_mut(cols * rows);
        let mut console = Console {
            fb,
            font,
            cols,
            rows,
            origin,
            cells,
            drawn,
            col: 0,
            row: 0,
            wrap_pending: false,
            colors,
            cells_drawn: 0,
        };
        console.clear();
        Some(console)
    }

    /// Columns in the grid.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Rows in the grid.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Glyph size in pixels, as (width, height).
    pub fn font_size(&self) -> (usize, usize) {
        (self.font.width, self.font.height)
    }

    /// Pixel position of the grid's top-left corner.
    pub fn origin(&self) -> (usize, usize) {
        self.origin
    }

    /// The framebuffer the console draws on.
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.fb
    }

    /// The framebuffer's pixels, for a program that draws on the screen
    /// itself (consoled, through `/dev/fb0`).
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        self.fb.memory
    }

    /// Blanks the screen and moves the cursor to the top-left corner.
    pub fn clear(&mut self) {
        self.blank();
        self.cells.fill(0);
        self.drawn.fill(0);
        self.col = 0;
        self.row = 0;
        self.wrap_pending = false;
    }

    /// Draws the whole screen again from the grid, for when something else
    /// has drawn on it: consoled, before it gave the screen back.
    pub fn repaint(&mut self) {
        self.blank();
        // No cell holds this value, so every one is drawn.
        self.drawn.fill(u32::MAX);
        self.flush();
    }

    /// Paints every pixel in the plain background.
    fn blank(&mut self) {
        let bpp = usize::from(self.fb.format.bytes_per_pixel);
        let background = self.colors[0].1.to_le_bytes();
        let row_bytes = self.fb.width * bpp;
        let height = self.fb.height.min(self.fb.memory.len() / self.fb.pitch);
        for y in 0..height {
            let row = &mut self.fb.memory[y * self.fb.pitch..y * self.fb.pitch + row_bytes];
            if background == [0; 4] {
                row.fill(0);
            } else {
                for pixel in row.chunks_exact_mut(bpp) {
                    pixel.copy_from_slice(&background[..bpp]);
                }
            }
        }
    }

    /// The cursor position, as (column, row).
    pub fn position(&self) -> (usize, usize) {
        (self.col, self.row)
    }

    /// Moves the cursor, clamped to the grid.
    pub fn set_position(&mut self, col: usize, row: usize) {
        self.col = col.min(self.cols - 1);
        self.row = row.min(self.rows - 1);
        self.wrap_pending = false;
    }

    /// Paints row `row` blank in `style`'s background, for a title band.
    pub fn fill_row(&mut self, row: usize, style: Style) {
        let blank = encode_cell(' ', style_index(style));
        if let Some(cells) = self.cells.get_mut(row * self.cols..(row + 1) * self.cols) {
            cells.fill(blank);
        }
    }

    /// Writes text at the cursor. Lines wrap at the right edge and the
    /// grid scrolls at the bottom. `\n` starts a new line, `\r` returns to
    /// its start and `\t` moves to the next multiple of eight columns, as
    /// on a terminal; other control characters are skipped. Nothing reaches the screen
    /// until [`Console::flush`].
    pub fn write(&mut self, text: &str, style: Style) {
        // Keep going sensibly even if a previous writer was interrupted
        // half-way (the panic path may take the console over).
        self.col = self.col.min(self.cols - 1);
        self.row = self.row.min(self.rows - 1);
        let style = style_index(style);
        for c in text.chars() {
            match c {
                '\n' => self.newline(),
                '\r' => {
                    self.col = 0;
                    self.wrap_pending = false;
                }
                '\t' if !self.wrap_pending => {
                    let next = (self.col / 8 + 1) * 8;
                    if next < self.cols {
                        self.col = next;
                    } else {
                        self.col = self.cols - 1;
                        self.wrap_pending = true;
                    }
                }
                c if c.is_control() => {}
                c => self.put(c, style),
            }
        }
    }

    fn put(&mut self, c: char, style: u32) {
        if self.wrap_pending {
            self.newline();
        }
        self.cells[self.row * self.cols + self.col] = encode_cell(c, style);
        if self.col + 1 == self.cols {
            self.wrap_pending = true;
        } else {
            self.col += 1;
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        self.wrap_pending = false;
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            self.cells.copy_within(self.cols.., 0);
            let last = (self.rows - 1) * self.cols;
            self.cells[last..].fill(0);
        }
    }

    /// Draws every cell that changed since the last flush.
    pub fn flush(&mut self) {
        for index in 0..self.cells.len() {
            if self.cells[index] != self.drawn[index] {
                self.draw_cell(index);
                self.drawn[index] = self.cells[index];
            }
        }
    }

    /// Cells drawn since the console was created, for tests and statistics.
    pub fn cells_drawn(&self) -> u64 {
        self.cells_drawn
    }

    fn draw_cell(&mut self, index: usize) {
        self.cells_drawn += 1;
        let (c, style) = decode_cell(self.cells[index]);
        let (fg, bg) = self.colors[style];
        let (fg, bg) = (fg.to_le_bytes(), bg.to_le_bytes());
        let glyph = self.font.glyph(c);
        let bpp = usize::from(self.fb.format.bytes_per_pixel);
        let (width, height) = (self.font.width, self.font.height);
        let x = self.origin.0 + (index % self.cols) * width;
        let y = self.origin.1 + (index / self.cols) * height;
        let mut line = [0u8; MAX_GLYPH_WIDTH * 4];
        for gy in 0..height {
            let bits = glyph.row_bits(gy);
            for gx in 0..width {
                let lit = bits & (1 << (width - 1 - gx)) != 0;
                let color = if lit { &fg } else { &bg };
                line[gx * bpp..(gx + 1) * bpp].copy_from_slice(&color[..bpp]);
            }
            let start = (y + gy) * self.fb.pitch + x * bpp;
            if let Some(target) = self.fb.memory.get_mut(start..start + width * bpp) {
                target.copy_from_slice(&line[..width * bpp]);
            }
        }
    }

    /// Calls `f` with the text of row `row`, trailing blanks removed.
    pub fn row_text(&self, row: usize, f: &mut dyn FnMut(char)) {
        let Some(cells) = self.cells.get(row * self.cols..(row + 1) * self.cols) else { return };
        let used = cells.iter().rposition(|&cell| decode_cell(cell).0 != ' ').map_or(0, |i| i + 1);
        cells[..used].iter().for_each(|&cell| f(decode_cell(cell).0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radix_hal::PixelFormat;

    fn framebuffer(width: usize, height: usize) -> Framebuffer {
        let pitch = width * 4 + 8;
        Framebuffer {
            memory: Box::leak(vec![0x55u8; pitch * height].into_boxed_slice()),
            phys_addr: 0xfd00_0000,
            width,
            height,
            pitch,
            format: PixelFormat::XRGB8888,
        }
    }

    fn storage(cells: usize) -> &'static mut [u32] {
        Box::leak(vec![0u32; cells].into_boxed_slice())
    }

    fn console(width: usize, height: usize) -> Console {
        let font = builtin_font("8x16").unwrap();
        let cells = storage_cells(width, height, &font);
        Console::new(framebuffer(width, height), font, storage(cells)).unwrap()
    }

    fn text(console: &Console, row: usize) -> String {
        let mut out = String::new();
        console.row_text(row, &mut |c| out.push(c));
        out
    }

    fn pixel(console: &Console, x: usize, y: usize) -> u32 {
        let fb = console.framebuffer();
        let at = y * fb.pitch + x * 4;
        u32::from_le_bytes(fb.memory[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn builtin_fonts_parse_and_cover_ascii() {
        for (name, _) in FONTS {
            let font = builtin_font(name).unwrap();
            assert_eq!(format!("{}x{}", font.width, font.height), name);
            for c in ' '..='~' {
                assert!(font.lookup(c).is_some(), "{name} lacks {c:?}");
            }
            assert!(font.lookup('│').is_some(), "{name} lacks box drawing");
        }
    }

    #[test]
    fn font_choice_follows_screen_size() {
        assert_eq!(choose_font(800, 600), "8x16");
        assert_eq!(choose_font(1366, 768), "8x16");
        assert_eq!(choose_font(1920, 1080), "12x24");
        assert_eq!(choose_font(3840, 2160), "16x32");
    }

    #[test]
    fn clears_and_centres_the_grid() {
        let console = console(804, 600);
        assert_eq!((console.cols(), console.rows()), (100, 37));
        assert_eq!(console.origin(), (2, 4));
        assert_eq!(pixel(&console, 0, 0), 0);
        assert_eq!(pixel(&console, 803, 599), 0);
    }

    #[test]
    fn draws_glyphs_in_style_colours() {
        let mut console = console(640, 480);
        console.write("A", Style::Error);
        console.flush();
        let font = builtin_font("8x16").unwrap();
        let glyph = font.glyph('A');
        let red = PixelFormat::XRGB8888.encode(0xff, 0x60, 0x60);
        for y in 0..16 {
            for x in 0..8 {
                let expected = if glyph.pixel(x, y) { red } else { 0 };
                assert_eq!(pixel(&console, x, y), expected, "pixel ({x}, {y})");
            }
        }
    }

    #[test]
    fn wraps_scrolls_and_redraws_only_changes() {
        let mut console = console(16 * 8, 4 * 16);
        console.write("0123456789abcdefXY\n", Style::Plain);
        assert_eq!(text(&console, 0), "0123456789abcdef");
        assert_eq!(text(&console, 1), "XY");
        assert_eq!(console.position(), (0, 2));
        console.flush();
        assert_eq!(console.cells_drawn(), 18);
        console.flush();
        assert_eq!(console.cells_drawn(), 18, "nothing changed");

        console.write("two\nthree\nfour", Style::Plain);
        assert_eq!(text(&console, 0), "XY");
        assert_eq!(text(&console, 3), "four");
        console.write("\rFO\tx", Style::Plain);
        assert_eq!(text(&console, 3), "FOur    x");
    }

    #[test]
    fn exact_width_lines_do_not_leave_blank_lines() {
        let mut console = console(16 * 8, 4 * 16);
        console.write("0123456789abcdef\nnext", Style::Plain);
        assert_eq!(text(&console, 1), "next");
    }

    #[test]
    fn banner_rows_keep_their_background() {
        let mut console = console(16 * 8, 4 * 16);
        console.fill_row(0, Style::Banner);
        console.set_position(1, 0);
        console.write("PANIC", Style::Banner);
        console.flush();
        let band = PixelFormat::XRGB8888.encode(0xa0, 0x10, 0x10);
        assert_eq!(pixel(&console, 0, 0), band);
        assert_eq!(pixel(&console, 127, 15), band);
        assert_eq!(pixel(&console, 0, 16), 0);
    }

    #[test]
    fn repaints_over_what_others_drew() {
        let mut console = console(16 * 8, 4 * 16);
        console.write("A", Style::Plain);
        console.flush();
        let drawn = console.cells_drawn();
        console.pixels_mut().fill(0x77);
        console.repaint();
        assert_eq!(console.cells_drawn(), drawn + 64, "every cell again");
        assert_ne!(pixel(&console, 0, 0) & 0xff, 0x77);
        assert_eq!(pixel(&console, 127, 63), 0);
    }

    #[test]
    fn small_storage_limits_rows() {
        let font = builtin_font("8x16").unwrap();
        let console = Console::new(framebuffer(640, 480), font, storage(2 * 80 * 10)).unwrap();
        assert_eq!((console.cols(), console.rows()), (80, 10));
        let tiny = Console::new(framebuffer(64, 480), font, storage(10_000));
        assert!(tiny.is_none());
    }
}
