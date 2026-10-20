//! VGA text-mode screen (80 x 25), used when the boot loader leaves the PC
//! in text mode instead of setting up a framebuffer.

use core::sync::atomic::{AtomicUsize, Ordering};

use radix_klog::{Sink, Style};
use radix_sync::SpinLock;

/// Text-mode geometry.
const COLUMNS: usize = 80;
const ROWS: usize = 25;

/// Virtual address of the text buffer once mapped, or 0.
static BUFFER: AtomicUsize = AtomicUsize::new(0);
/// Index of the next cell to write.
static CURSOR: SpinLock<usize> = SpinLock::new(0);

fn attribute(style: Style) -> u16 {
    let colour: u16 = match style {
        Style::Plain => 0x07,
        Style::Error => 0x0c,
        Style::Warning => 0x0e,
        Style::Debug => 0x08,
        Style::Emphasis => 0x0f,
        Style::Banner => 0x4f,
    };
    colour << 8
}

/// Starts using the text buffer at `virt` (the mapping of physical
/// 0xB8000) and blanks it.
pub fn init(virt: usize) {
    BUFFER.store(virt, Ordering::Relaxed);
    for cell in 0..COLUMNS * ROWS {
        write_cell(cell, attribute(Style::Plain) | u16::from(b' '));
    }
    *CURSOR.lock() = 0;
}

fn write_cell(index: usize, value: u16) {
    let base = BUFFER.load(Ordering::Relaxed);
    if base == 0 || index >= COLUMNS * ROWS {
        return;
    }
    // SAFETY: `base` maps the 4000-byte VGA text buffer (set by `init` from
    // a mapping of physical 0xB8000 made on a BIOS text-mode boot), and
    // `index` is inside its 80 x 25 cells.
    unsafe { (base as *mut u16).add(index).write_volatile(value) }
}

fn read_cell(index: usize) -> u16 {
    let base = BUFFER.load(Ordering::Relaxed);
    if base == 0 || index >= COLUMNS * ROWS {
        return 0;
    }
    // SAFETY: as for `write_cell`.
    unsafe { (base as *const u16).add(index).read_volatile() }
}

fn put(cursor: &mut usize, c: char, style: Style) {
    if c == '\n' {
        *cursor = (*cursor / COLUMNS + 1) * COLUMNS;
    } else {
        // Code page 437 matches ASCII in the printable range.
        let byte = if c.is_ascii_graphic() || c == ' ' { c as u8 } else { b'?' };
        write_cell(*cursor, attribute(style) | u16::from(byte));
        *cursor += 1;
    }
    if *cursor >= COLUMNS * ROWS {
        for cell in COLUMNS..COLUMNS * ROWS {
            write_cell(cell - COLUMNS, read_cell(cell));
        }
        for cell in COLUMNS * (ROWS - 1)..COLUMNS * ROWS {
            write_cell(cell, attribute(Style::Plain) | u16::from(b' '));
        }
        *cursor = COLUMNS * (ROWS - 1);
    }
}

/// The kernel log's text-mode screen.
pub struct VgaSink;

impl Sink for VgaSink {
    fn write(&self, text: &str, style: Style) {
        let mut cursor = CURSOR.lock();
        for c in text.chars().filter(|&c| c != '\r') {
            put(&mut cursor, c, style);
        }
    }
}
