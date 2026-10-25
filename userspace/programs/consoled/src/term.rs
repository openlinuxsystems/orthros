// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The terminal of one virtual console: a grid of character cells, and
//! what the bytes a program writes do to it.
//!
//! It answers to TERM=linux, the terminal type of Linux's console, whose
//! escape sequences console_codes(4) lists and terminfo's `linux` entry
//! names: cursor movement, erasing, inserting and deleting characters and
//! lines, a scrolling region, tab stops, the SGR attributes and colours
//! (16 colours, and the 256 of xterm's palette, which Linux maps to its
//! 16 and this console draws as they are), the DEC line drawing set as G1
//! (`ESC ) 0` with SO and SI, as ncurses draws boxes), the cursor and
//! keypad modes, and the device status replies. Lines that scroll off the
//! top of the whole screen are kept, for Shift+Page Up to look back at.
//! [vte](https://github.com/alacritty/vte), Alacritty's parser, splits the
//! bytes into characters and sequences.

use std::collections::VecDeque;

use vte::{Params, Parser, Perform};

/// The colour text has before a program sets one: light grey.
pub const DEFAULT_FG: u8 = 7;
/// The colour behind text before a program sets one: black.
pub const DEFAULT_BG: u8 = 0;

/// Bold: the brighter of the eight colours, as Linux shows it.
pub const BOLD: u8 = 1;
/// Half bright.
pub const DIM: u8 = 2;
/// Underlined.
pub const UNDERLINE: u8 = 4;
/// Foreground and background swapped.
pub const REVERSE: u8 = 8;
/// The second half of a character two cells wide.
pub const WIDE_TAIL: u8 = 16;

/// How many lines that scrolled off the top are kept.
pub const SCROLLBACK_LINES: usize = 1000;

/// One character position on the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The character.
    pub c: char,
    /// Its colour, an index into the 256-colour palette.
    pub fg: u8,
    /// The colour behind it.
    pub bg: u8,
    /// BOLD, DIM, UNDERLINE, REVERSE and WIDE_TAIL bits.
    pub flags: u8,
}

impl Cell {
    /// An empty cell in the default colours.
    pub const BLANK: Cell = Cell { c: ' ', fg: DEFAULT_FG, bg: DEFAULT_BG, flags: 0 };
}

/// What a G0 or G1 character set maps the printable ASCII characters to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Charset {
    /// Themselves.
    Ascii,
    /// The DEC Special Graphics set: lines and corners for boxes.
    Graphics,
}

/// The modes a program can set that change how keys are sent and text
/// is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Modes {
    /// Text that reaches the last column goes on at the next line
    /// (DECAWM).
    pub autowrap: bool,
    /// Printed characters push the rest of the line right (IRM).
    pub insert: bool,
    /// Cursor positions count from the scrolling region (DECOM).
    pub origin: bool,
    /// The cursor is shown (DECTCEM).
    pub cursor_visible: bool,
    /// The cursor keys send `ESC O` sequences (DECCKM).
    pub app_cursor: bool,
    /// The keypad sends `ESC O` sequences (DECKPAM).
    pub app_keypad: bool,
    /// A line feed also returns the carriage, and Enter sends CR LF
    /// (LNM).
    pub newline: bool,
    /// The whole screen is in reverse video (DECSCNM).
    pub reverse_screen: bool,
}

impl Modes {
    const DEFAULT: Modes = Modes {
        autowrap: true,
        insert: false,
        origin: false,
        cursor_visible: true,
        app_cursor: false,
        app_keypad: false,
        newline: false,
        reverse_screen: false,
    };
}

/// What DECSC saves and DECRC restores.
#[derive(Clone, Copy, Debug)]
struct Saved {
    x: usize,
    y: usize,
    pen: Cell,
    charsets: [Charset; 2],
    shift: usize,
    origin: bool,
}

/// The grid and the state the escape sequences change.
struct State {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    x: usize,
    y: usize,
    /// The last column was written: the next character goes on the next
    /// line (the "xenl" glitch terminfo describes).
    wrap_pending: bool,
    /// The attributes and colours new characters get; `c` is unused.
    pen: Cell,
    /// The scrolling region's first and last rows.
    top: usize,
    bottom: usize,
    tabs: Vec<bool>,
    charsets: [Charset; 2],
    /// Which of G0 and G1 is in use (SI and SO).
    shift: usize,
    saved: Saved,
    modes: Modes,
    /// Bytes to send back to the program: answers to status requests.
    replies: Vec<u8>,
    scrollback: VecDeque<Vec<Cell>>,
    /// How many lines back the screen shows; 0 for the live screen.
    view: usize,
}

/// A virtual console's terminal.
pub struct Terminal {
    parser: Parser,
    state: State,
}

impl Terminal {
    /// An empty terminal of `cols` by `rows` cells.
    pub fn new(cols: usize, rows: usize) -> Terminal {
        let (cols, rows) = (cols.max(2), rows.max(2));
        let saved = Saved {
            x: 0,
            y: 0,
            pen: Cell::BLANK,
            charsets: [Charset::Ascii, Charset::Graphics],
            shift: 0,
            origin: false,
        };
        let mut state = State {
            cols,
            rows,
            cells: vec![Cell::BLANK; cols * rows],
            x: 0,
            y: 0,
            wrap_pending: false,
            pen: Cell::BLANK,
            top: 0,
            bottom: rows - 1,
            tabs: Vec::new(),
            charsets: [Charset::Ascii, Charset::Graphics],
            shift: 0,
            saved,
            modes: Modes::DEFAULT,
            replies: Vec::new(),
            scrollback: VecDeque::new(),
            view: 0,
        };
        state.reset_tabs();
        Terminal { parser: Parser::new(), state }
    }

    /// Columns.
    pub fn cols(&self) -> usize {
        self.state.cols
    }

    /// Rows.
    pub fn rows(&self) -> usize {
        self.state.rows
    }

    /// Takes in what the program wrote.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.state, bytes);
    }

    /// The cell shown at (`x`, `y`): from the scrollback while the view is
    /// moved back.
    pub fn shown(&self, x: usize, y: usize) -> Cell {
        let s = &self.state;
        if y < s.view {
            let line = &s.scrollback[s.scrollback.len() - s.view + y];
            return line.get(x).copied().unwrap_or(Cell::BLANK);
        }
        s.cells[(y - s.view) * s.cols + x]
    }

    /// The cell at (`x`, `y`) of the live screen: for the tests.
    #[cfg(test)]
    pub fn cell(&self, x: usize, y: usize) -> Cell {
        self.state.cells[y * self.state.cols + x]
    }

    /// Where the cursor is shown, if it is: not while it is hidden or the
    /// view is moved back.
    pub fn cursor(&self) -> Option<(usize, usize)> {
        let s = &self.state;
        (s.modes.cursor_visible && s.view == 0).then_some((s.x, s.y))
    }

    /// The modes the program set.
    pub fn modes(&self) -> Modes {
        self.state.modes
    }

    /// Takes the replies to send to the program.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.state.replies)
    }

    /// Moves the view `lines` back into the scrollback (negative: forward),
    /// within what it holds.
    pub fn scroll_view(&mut self, lines: isize) {
        let s = &mut self.state;
        let view = (s.view as isize + lines).clamp(0, s.scrollback.len() as isize) as usize;
        s.view = view;
    }

    /// Goes back to the live screen.
    pub fn reset_view(&mut self) {
        self.scroll_view(-(self.state.view as isize));
    }

    /// How many lines back the view is: for the tests.
    #[cfg(test)]
    pub fn view(&self) -> usize {
        self.state.view
    }

    /// The text of live row `y`, without trailing blanks: for the tests.
    #[cfg(test)]
    pub fn row_text(&self, y: usize) -> String {
        let s = &self.state;
        let row: String = s.cells[y * s.cols..(y + 1) * s.cols]
            .iter()
            .filter(|cell| cell.flags & WIDE_TAIL == 0)
            .map(|cell| cell.c)
            .collect();
        row.trim_end().to_string()
    }

    /// Clears the screen and starts as a new terminal, keeping the
    /// scrollback: for a new session.
    pub fn reset(&mut self) {
        self.parser = Parser::new();
        self.state.full_reset();
    }
}

/// The DEC Special Graphics character for `c` (VT100's line drawing set).
fn graphics(c: char) -> char {
    match c {
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        '_' => ' ',
        c => c,
    }
}

/// How many cells `c` takes: 0 for combining marks and other characters
/// of no width, 2 for East Asian wide ones, as wcwidth(3) counts them.
pub fn width(c: char) -> usize {
    let code = u32::from(c);
    const ZERO: &[(u32, u32)] = &[
        (0x0300, 0x036f),
        (0x0483, 0x0489),
        (0x0591, 0x05bd),
        (0x0610, 0x061a),
        (0x064b, 0x065f),
        (0x0e31, 0x0e31),
        (0x0e34, 0x0e3a),
        (0x0e47, 0x0e4e),
        (0x1ab0, 0x1aff),
        (0x1dc0, 0x1dff),
        (0x200b, 0x200f),
        (0x2028, 0x202e),
        (0x2060, 0x2064),
        (0x20d0, 0x20ff),
        (0xfe00, 0xfe0f),
        (0xfe20, 0xfe2f),
        (0xfeff, 0xfeff),
        (0x1f3fb, 0x1f3ff),
        (0xe0100, 0xe01ef),
    ];
    const WIDE: &[(u32, u32)] = &[
        (0x1100, 0x115f),
        (0x231a, 0x231b),
        (0x2329, 0x232a),
        (0x23e9, 0x23ec),
        (0x2614, 0x2615),
        (0x2e80, 0x303e),
        (0x3041, 0x33ff),
        (0x3400, 0x4dbf),
        (0x4e00, 0x9fff),
        (0xa000, 0xa4cf),
        (0xa960, 0xa97f),
        (0xac00, 0xd7a3),
        (0xf900, 0xfaff),
        (0xfe10, 0xfe19),
        (0xfe30, 0xfe6f),
        (0xff00, 0xff60),
        (0xffe0, 0xffe6),
        (0x1f300, 0x1f64f),
        (0x1f900, 0x1f9ff),
        (0x20000, 0x2fffd),
        (0x30000, 0x3fffd),
    ];
    let within =
        |table: &[(u32, u32)]| table.iter().any(|&(low, high)| (low..=high).contains(&code));
    if within(ZERO) {
        0
    } else if within(WIDE) {
        2
    } else {
        1
    }
}

/// The `index`th parameter, or `default` when it is missing or 0.
fn param(params: &Params, index: usize, default: usize) -> usize {
    match params.iter().nth(index).and_then(|p| p.first()) {
        Some(&n) if n != 0 => usize::from(n),
        _ => default,
    }
}

/// The 256-colour palette index nearest to the colour (`r`, `g`, `b`),
/// each 0 to 255, for SGR 38;2 and 48;2: one of the 6x6x6 cube or the
/// grey ramp.
fn nearest(r: u16, g: u16, b: u16) -> u8 {
    let level = |v: u16| -> u16 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (v - 35) / 40
        }
    };
    let (ri, gi, bi) = (level(r).min(5), level(g).min(5), level(b).min(5));
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let step = |i: u16| if i == 0 { 0 } else { 55 + 40 * i };
    let cube_error = |(r, g, b): (u16, u16, u16)| {
        let d = |a: u16, b: u16| (i32::from(a) - i32::from(b)).pow(2);
        d(r, step(ri)) + d(g, step(gi)) + d(b, step(bi))
    };
    let average = (r + g + b) / 3;
    let grey = if average > 238 { 23 } else { average.saturating_sub(3) / 10 };
    let grey_level = 8 + 10 * grey;
    let grey_error = {
        let d = |a: u16| (i32::from(a) - i32::from(grey_level)).pow(2);
        d(r) + d(g) + d(b)
    };
    if grey_error < cube_error((r, g, b)) { (232 + grey) as u8 } else { cube as u8 }
}

impl State {
    fn index(&self, x: usize, y: usize) -> usize {
        y * self.cols + x
    }

    /// An empty cell in the current background colour, which is what
    /// erasing leaves (terminfo's `bce`).
    fn blank(&self) -> Cell {
        Cell { c: ' ', fg: self.pen.fg, bg: self.pen.bg, flags: 0 }
    }

    fn reset_tabs(&mut self) {
        self.tabs = (0..self.cols).map(|x| x % 8 == 0 && x > 0).collect();
    }

    fn full_reset(&mut self) {
        let blank = Cell::BLANK;
        self.cells.fill(blank);
        self.x = 0;
        self.y = 0;
        self.wrap_pending = false;
        self.pen = Cell::BLANK;
        self.top = 0;
        self.bottom = self.rows - 1;
        self.reset_tabs();
        self.charsets = [Charset::Ascii, Charset::Graphics];
        self.shift = 0;
        self.saved = Saved {
            x: 0,
            y: 0,
            pen: Cell::BLANK,
            charsets: self.charsets,
            shift: 0,
            origin: false,
        };
        self.modes = Modes::DEFAULT;
        self.view = 0;
    }

    /// Rows `from..=to` move up `n`, the top ones going; blanks come in at
    /// the bottom. With `keep`, lines leaving the top of the whole screen go
    /// to the scrollback.
    fn scroll_up(&mut self, from: usize, to: usize, n: usize, keep: bool) {
        let n = n.min(to + 1 - from);
        if keep && from == 0 && to == self.rows - 1 {
            for row in 0..n {
                let start = row * self.cols;
                if self.scrollback.len() == SCROLLBACK_LINES {
                    self.scrollback.pop_front();
                }
                self.scrollback.push_back(self.cells[start..start + self.cols].to_vec());
                if self.view > 0 {
                    self.view = (self.view + 1).min(self.scrollback.len());
                }
            }
        }
        let cols = self.cols;
        self.cells.copy_within((from + n) * cols..(to + 1) * cols, from * cols);
        let blank = self.blank();
        self.cells[(to + 1 - n) * cols..(to + 1) * cols].fill(blank);
    }

    /// Rows `from..=to` move down `n`, the bottom ones going; blanks come
    /// in at the top.
    fn scroll_down(&mut self, from: usize, to: usize, n: usize) {
        let n = n.min(to + 1 - from);
        let cols = self.cols;
        self.cells.copy_within(from * cols..(to + 1 - n) * cols, (from + n) * cols);
        let blank = self.blank();
        self.cells[from * cols..(from + n) * cols].fill(blank);
    }

    /// Moves down a line, scrolling the region at its bottom.
    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.y == self.bottom {
            self.scroll_up(self.top, self.bottom, 1, true);
        } else if self.y + 1 < self.rows {
            self.y += 1;
        }
    }

    /// Moves up a line, scrolling the region at its top (RI).
    fn reverse_line_feed(&mut self) {
        self.wrap_pending = false;
        if self.y == self.top {
            self.scroll_down(self.top, self.bottom, 1);
        } else if self.y > 0 {
            self.y -= 1;
        }
    }

    /// Puts the cursor at (`x`, `y`), counted from the region's top in
    /// origin mode, within the screen (or the region).
    fn go_to(&mut self, x: usize, y: usize) {
        let (low, high) =
            if self.modes.origin { (self.top, self.bottom) } else { (0, self.rows - 1) };
        let base = if self.modes.origin { self.top } else { 0 };
        self.x = x.min(self.cols - 1);
        self.y = (base + y).clamp(low, high);
        self.wrap_pending = false;
    }

    /// Erases cells `from..to` of the grid, as indices.
    fn erase(&mut self, from: usize, to: usize) {
        let blank = self.blank();
        let to = to.min(self.cells.len());
        if from < to {
            self.cells[from..to].fill(blank);
        }
    }

    fn put(&mut self, c: char) {
        let c = if self.charsets[self.shift] == Charset::Graphics { graphics(c) } else { c };
        let width = width(c);
        if width == 0 {
            return;
        }
        if self.wrap_pending && self.modes.autowrap {
            self.x = 0;
            self.line_feed();
        }
        self.wrap_pending = false;
        if width == 2 && self.x + 1 >= self.cols {
            // No room for both halves: it goes on the next line.
            if self.modes.autowrap {
                let at = self.index(self.x, self.y);
                self.cells[at] = self.blank();
                self.x = 0;
                self.line_feed();
            } else {
                return;
            }
        }
        let row = self.y * self.cols;
        if self.modes.insert {
            let end = row + self.cols;
            let at = row + self.x;
            self.cells.copy_within(at..end - width, at + width);
        }
        let cell = Cell { c, ..self.pen };
        self.cells[row + self.x] = cell;
        if width == 2 {
            self.cells[row + self.x + 1] = Cell { c: ' ', flags: cell.flags | WIDE_TAIL, ..cell };
        }
        if self.x + width >= self.cols {
            if self.modes.autowrap {
                self.wrap_pending = true;
            }
            self.x = self.cols - 1;
        } else {
            self.x += width;
        }
    }

    fn sgr(&mut self, params: &Params) {
        let mut items = params.iter().peekable();
        if items.peek().is_none() {
            self.pen = Cell::BLANK;
            return;
        }
        while let Some(item) = items.next() {
            let code = item.first().copied().unwrap_or(0);
            match code {
                0 => self.pen = Cell::BLANK,
                1 => self.pen.flags |= BOLD,
                2 => self.pen.flags |= DIM,
                4 => self.pen.flags |= UNDERLINE,
                7 => self.pen.flags |= REVERSE,
                21 | 22 => self.pen.flags &= !(BOLD | DIM),
                24 => self.pen.flags &= !UNDERLINE,
                27 => self.pen.flags &= !REVERSE,
                30..=37 => self.pen.fg = (code - 30) as u8,
                39 => self.pen.fg = DEFAULT_FG,
                40..=47 => self.pen.bg = (code - 40) as u8,
                49 => self.pen.bg = DEFAULT_BG,
                90..=97 => self.pen.fg = (code - 90 + 8) as u8,
                100..=107 => self.pen.bg = (code - 100 + 8) as u8,
                38 | 48 => {
                    // 38;5;n or 38;2;r;g;b, or the same with colons.
                    let mut words: Vec<u16> = item[1..].to_vec();
                    if words.is_empty() {
                        let kind = items.next().and_then(|p| p.first().copied());
                        if let Some(kind) = kind {
                            words.push(kind);
                            let more = if kind == 5 {
                                1
                            } else if kind == 2 {
                                3
                            } else {
                                0
                            };
                            for _ in 0..more {
                                words.push(
                                    items.next().and_then(|p| p.first().copied()).unwrap_or(0),
                                );
                            }
                        }
                    }
                    let colour = match words.as_slice() {
                        [5, n, ..] => Some((*n).min(255) as u8),
                        // A colon form may name a colour space first.
                        [2, _, r, g, b] | [2, r, g, b] => {
                            Some(nearest((*r).min(255), (*g).min(255), (*b).min(255)))
                        }
                        _ => None,
                    };
                    if let Some(colour) = colour {
                        if code == 38 {
                            self.pen.fg = colour;
                        } else {
                            self.pen.bg = colour;
                        }
                    }
                }
                // Blink, italics, fonts and the rest: Linux's console shows
                // most of them as colours, if at all.
                _ => {}
            }
        }
    }

    fn set_mode(&mut self, private: bool, params: &Params, on: bool) {
        for item in params {
            let mode = item.first().copied().unwrap_or(0);
            match (private, mode) {
                (true, 1) => self.modes.app_cursor = on,
                (true, 5) => self.modes.reverse_screen = on,
                (true, 6) => {
                    self.modes.origin = on;
                    self.go_to(0, 0);
                }
                (true, 7) => self.modes.autowrap = on,
                (true, 25) => self.modes.cursor_visible = on,
                (false, 4) => self.modes.insert = on,
                (false, 20) => self.modes.newline = on,
                _ => {}
            }
        }
    }

    fn save(&mut self) {
        self.saved = Saved {
            x: self.x,
            y: self.y,
            pen: self.pen,
            charsets: self.charsets,
            shift: self.shift,
            origin: self.modes.origin,
        };
    }

    fn restore(&mut self) {
        let saved = self.saved;
        self.x = saved.x.min(self.cols - 1);
        self.y = saved.y.min(self.rows - 1);
        self.pen = saved.pen;
        self.charsets = saved.charsets;
        self.shift = saved.shift;
        self.modes.origin = saved.origin;
        // A row saved above or below the scrolling region set since is
        // brought into it, where origin mode keeps the cursor.
        if saved.origin {
            self.y = self.y.clamp(self.top, self.bottom);
        }
        self.wrap_pending = false;
    }

    fn reply(&mut self, text: &str) {
        self.replies.extend_from_slice(text.as_bytes());
    }
}

impl Perform for State {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                self.x = self.x.saturating_sub(1);
                self.wrap_pending = false;
            }
            0x09 => {
                let next = (self.x + 1..self.cols).find(|&x| self.tabs[x]);
                self.x = next.unwrap_or(self.cols - 1);
                self.wrap_pending = false;
            }
            0x0a..=0x0c => {
                self.line_feed();
                if self.modes.newline {
                    self.x = 0;
                }
            }
            0x0d => {
                self.x = 0;
                self.wrap_pending = false;
            }
            0x0e => self.shift = 1,
            0x0f => self.shift = 0,
            // The bell, and the rest: nothing to show.
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let private = intermediates.first() == Some(&b'?');
        if !intermediates.is_empty() && !private {
            return;
        }
        let n = param(params, 0, 1);
        let (cols, rows) = (self.cols, self.rows);
        match action {
            'h' | 'l' => return self.set_mode(private, params, action == 'h'),
            _ if private => return,
            _ => {}
        }
        match action {
            'A' => {
                let low = if self.y >= self.top { self.top } else { 0 };
                self.y = self.y.saturating_sub(n).max(low);
                self.wrap_pending = false;
            }
            'B' | 'e' => {
                let high = if self.y <= self.bottom { self.bottom } else { rows - 1 };
                self.y = (self.y + n).min(high);
                self.wrap_pending = false;
            }
            'C' | 'a' => {
                self.x = (self.x + n).min(cols - 1);
                self.wrap_pending = false;
            }
            'D' => {
                self.x = self.x.saturating_sub(n);
                self.wrap_pending = false;
            }
            'E' => {
                let high = if self.y <= self.bottom { self.bottom } else { rows - 1 };
                self.y = (self.y + n).min(high);
                self.x = 0;
                self.wrap_pending = false;
            }
            'F' => {
                let low = if self.y >= self.top { self.top } else { 0 };
                self.y = self.y.saturating_sub(n).max(low);
                self.x = 0;
                self.wrap_pending = false;
            }
            'G' | '`' => {
                self.x = (n - 1).min(cols - 1);
                self.wrap_pending = false;
            }
            'H' | 'f' => {
                let (row, col) = (param(params, 0, 1), param(params, 1, 1));
                self.go_to(col - 1, row - 1);
            }
            'd' => {
                let x = self.x;
                self.go_to(x, n - 1);
            }
            'J' => {
                let at = self.index(self.x, self.y);
                match param(params, 0, 0) {
                    0 => self.erase(at, cols * rows),
                    1 => self.erase(0, at + 1),
                    2 => self.erase(0, cols * rows),
                    3 => {
                        self.erase(0, cols * rows);
                        self.scrollback.clear();
                        self.view = 0;
                    }
                    _ => {}
                }
            }
            'K' => {
                let row = self.y * cols;
                match param(params, 0, 0) {
                    0 => self.erase(row + self.x, row + cols),
                    1 => self.erase(row, row + self.x + 1),
                    2 => self.erase(row, row + cols),
                    _ => {}
                }
            }
            'L' | 'M' => {
                if (self.top..=self.bottom).contains(&self.y) {
                    if action == 'L' {
                        self.scroll_down(self.y, self.bottom, n);
                    } else {
                        self.scroll_up(self.y, self.bottom, n, false);
                    }
                    self.x = 0;
                    self.wrap_pending = false;
                }
            }
            '@' => {
                let row = self.y * cols;
                let n = n.min(cols - self.x);
                self.cells.copy_within(row + self.x..row + cols - n, row + self.x + n);
                self.erase(row + self.x, row + self.x + n);
                self.wrap_pending = false;
            }
            'P' => {
                let row = self.y * cols;
                let n = n.min(cols - self.x);
                self.cells.copy_within(row + self.x + n..row + cols, row + self.x);
                self.erase(row + cols - n, row + cols);
                self.wrap_pending = false;
            }
            'X' => {
                let row = self.y * cols;
                let n = n.min(cols - self.x);
                self.erase(row + self.x, row + self.x + n);
                self.wrap_pending = false;
            }
            'S' => self.scroll_up(self.top, self.bottom, n, true),
            'T' => self.scroll_down(self.top, self.bottom, n),
            'r' => {
                let top = param(params, 0, 1) - 1;
                let bottom = param(params, 1, rows).min(rows) - 1;
                if top < bottom {
                    self.top = top;
                    self.bottom = bottom;
                    self.go_to(0, 0);
                }
            }
            's' => self.save(),
            'u' => self.restore(),
            'g' => match param(params, 0, 0) {
                0 => self.tabs[self.x] = false,
                3 => self.tabs.fill(false),
                _ => {}
            },
            'm' => self.sgr(params),
            'n' => match param(params, 0, 0) {
                5 => self.reply("\x1b[0n"),
                6 => {
                    let y =
                        if self.modes.origin { self.y.saturating_sub(self.top) } else { self.y };
                    let text = format!("\x1b[{};{}R", y + 1, self.x + 1);
                    self.reply(&text);
                }
                _ => {}
            },
            // What Linux's console answers: a VT102.
            'c' if param(params, 0, 0) == 0 => self.reply("\x1b[?6c"),
            // LEDs, Linux's setterm requests and the rest.
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore {
            return;
        }
        match (intermediates, byte) {
            ([], b'D') => self.line_feed(),
            ([], b'E') => {
                self.x = 0;
                self.line_feed();
            }
            ([], b'H') => self.tabs[self.x] = true,
            ([], b'M') => self.reverse_line_feed(),
            ([], b'7') => self.save(),
            ([], b'8') => self.restore(),
            ([], b'c') => self.full_reset(),
            ([], b'=') => self.modes.app_keypad = true,
            ([], b'>') => self.modes.app_keypad = false,
            ([], b'Z') => self.reply("\x1b[?6c"),
            ([b'(' | b')'], set) => {
                let g = usize::from(intermediates[0] == b')');
                self.charsets[g] = if set == b'0' { Charset::Graphics } else { Charset::Ascii };
            }
            ([b'#'], b'8') => {
                let fill = Cell { c: 'E', ..Cell::BLANK };
                self.cells.fill(fill);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(text: &str) -> Terminal {
        let mut term = Terminal::new(20, 5);
        term.feed(text.as_bytes());
        term
    }

    fn rows(term: &Terminal) -> Vec<String> {
        (0..term.rows()).map(|y| term.row_text(y)).collect()
    }

    #[test]
    fn prints_wraps_and_scrolls() {
        let term = term("hello\r\nworld");
        assert_eq!(rows(&term)[..2], ["hello", "world"]);
        assert_eq!(term.cursor(), Some((5, 1)));
        let term = self::term(&"x".repeat(25));
        assert_eq!(rows(&term)[..2], ["x".repeat(20), "x".repeat(5)]);
        let term = self::term("1\n\r2\n\r3\n\r4\n\r5\n\r6");
        assert_eq!(rows(&term), ["2", "3", "4", "5", "6"]);
        assert_eq!(term.state.scrollback.len(), 1);
    }

    #[test]
    fn the_last_column_waits_for_the_next_character() {
        let mut term = term(&"y".repeat(20));
        assert_eq!(term.cursor(), Some((19, 0)));
        term.feed(b"\r\n");
        assert_eq!(term.cursor(), Some((0, 1)));
        assert_eq!(term.row_text(1), "");
    }

    #[test]
    fn moves_the_cursor_and_erases() {
        let term = term("abcdef\x1b[1;3H\x1b[K");
        assert_eq!(term.row_text(0), "ab");
        let term = self::term("abcdef\x1b[1;3H\x1b[1K");
        assert_eq!(term.row_text(0), "   def");
        let term = self::term("one\r\ntwo\x1b[2J");
        assert_eq!(rows(&term), ["", "", "", "", ""]);
        let term = self::term("\x1b[3;4Hx\x1b[A\x1b[Dy\x1b[6n");
        assert_eq!(term.row_text(2), "   x");
        assert_eq!(term.row_text(1), "   y");
        assert_eq!(term.state.replies, b"\x1b[2;5R");
    }

    #[test]
    fn inserts_and_deletes_characters_and_lines() {
        let term = term("abcdef\r\x1b[2@");
        assert_eq!(term.row_text(0), "  abcdef");
        let term = self::term("abcdef\r\x1b[2P");
        assert_eq!(term.row_text(0), "cdef");
        let term = self::term("1\r\n2\r\n3\x1b[2;1H\x1b[L");
        assert_eq!(rows(&term)[..4], ["1", "", "2", "3"]);
        let term = self::term("1\r\n2\r\n3\x1b[1;1H\x1b[M");
        assert_eq!(rows(&term)[..3], ["2", "3", ""]);
        // Deleted lines at the top of the screen are not kept.
        assert!(term.state.scrollback.is_empty());
    }

    #[test]
    fn scrolls_only_the_region() {
        let term = term("top\x1b[2;4r\x1b[4;1Ha\r\nb\r\nc");
        assert_eq!(rows(&term), ["top", "a", "b", "c", ""]);
        assert!(term.state.scrollback.is_empty());
        let term = self::term("\x1b[2;3r\x1b[2;1Hx\x1bM");
        assert_eq!(rows(&term)[..3], ["", "", "x"]);
    }

    #[test]
    fn keeps_colours_and_attributes() {
        let term = term("\x1b[1;31;44mR\x1b[0mn\x1b[38;5;208mo\x1b[48;2;255;0;0mt");
        let r = term.cell(0, 0);
        assert_eq!((r.fg, r.bg, r.flags), (1, 4, BOLD));
        assert_eq!(term.cell(1, 0), Cell { c: 'n', ..Cell::BLANK });
        assert_eq!(term.cell(2, 0).fg, 208);
        assert_eq!(term.cell(3, 0).bg, 196);
        // Erasing leaves the background colour.
        let term = self::term("\x1b[42m\x1b[2J");
        assert_eq!(term.cell(5, 3).bg, 2);
    }

    #[test]
    fn draws_lines_from_the_graphics_set() {
        let term = term("\x1b)0\x0elqk\x0fx");
        assert_eq!(term.row_text(0), "┌─┐x");
    }

    #[test]
    fn tabs_modes_and_replies() {
        let mut term = term("a\tb\x1b[?25l\x1b[?1h\x1b=\x1b[c");
        assert_eq!(term.row_text(0), "a       b");
        let modes = term.modes();
        assert!(!modes.cursor_visible && modes.app_cursor && modes.app_keypad);
        assert_eq!(term.cursor(), None);
        assert_eq!(term.take_replies(), b"\x1b[?6c");
        term.feed(b"\x1bc");
        assert!(term.modes().cursor_visible);
        assert_eq!(term.row_text(0), "");
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let term = term("a中b");
        assert_eq!(term.cell(1, 0).c, '中');
        assert_eq!(term.cell(2, 0).flags & WIDE_TAIL, WIDE_TAIL);
        assert_eq!(term.cell(3, 0).c, 'b');
        assert_eq!(term.row_text(0), "a中b");
        // Combining marks take none.
        assert_eq!(self::term("e\u{301}x").row_text(0), "ex");
    }

    #[test]
    fn looks_back_at_the_scrollback() {
        let mut term = term("1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7");
        assert_eq!(term.state.scrollback.len(), 2);
        term.scroll_view(10);
        assert_eq!(term.view(), 2);
        assert_eq!(term.shown(0, 0).c, '1');
        assert_eq!(term.cursor(), None);
        term.reset_view();
        assert_eq!(term.shown(0, 0).c, '3');
    }

    #[test]
    fn nearest_colours() {
        assert_eq!(nearest(0, 0, 0), 16);
        assert_eq!(nearest(255, 255, 255), 231);
        assert_eq!(nearest(128, 128, 128), 244);
        // Parameters past 255 are 255, not an overflow.
        let term = term("\x1b[38;2;65535;65535;65535mw");
        assert_eq!(term.cell(0, 0).fg, 231);
    }

    #[test]
    fn origin_mode_reports_within_the_region() {
        // The saved row is above the region set after it.
        let mut term = term("\x1b[?6h\x1b7\x1b[3;5r\x1b8\x1b[6n");
        assert_eq!(term.take_replies(), b"\x1b[1;1R");
        assert_eq!(term.cursor().map(|(_, y)| y), Some(2));
    }
}
