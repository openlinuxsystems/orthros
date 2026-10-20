//! VGA text-mode screen (80 x 25 at physical 0xB8000), used only on BIOS
//! boots, where GRUB leaves the adapter in text mode.

use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

const BUFFER: usize = 0xB8000;
const COLUMNS: usize = 80;
const ROWS: usize = 25;
/// Light grey on black.
const ATTRIBUTE: u16 = 0x07 << 8;

/// Index of the next cell to write. Phase 0 runs on one CPU with interrupts
/// off, so the atomic only avoids a `static mut`.
static CURSOR: AtomicUsize = AtomicUsize::new(0);

/// Writer for the VGA text screen.
pub struct Screen;

/// Blanks the screen and homes the cursor.
pub fn clear() {
    for cell in 0..COLUMNS * ROWS {
        write_cell(cell, ATTRIBUTE | u16::from(b' '));
    }
    CURSOR.store(0, Ordering::Relaxed);
}

fn write_cell(index: usize, value: u16) {
    assert!(index < COLUMNS * ROWS);
    // SAFETY: the boot trampoline identity-maps the first 4 GiB, the console
    // enables this module only on BIOS boots, where the text buffer exists,
    // and `index` stays inside its 80 x 25 cells.
    unsafe { (BUFFER as *mut u16).add(index).write_volatile(value) }
}

fn read_cell(index: usize) -> u16 {
    assert!(index < COLUMNS * ROWS);
    // SAFETY: as for `write_cell`.
    unsafe { (BUFFER as *const u16).add(index).read_volatile() }
}

fn scroll() {
    for cell in COLUMNS..COLUMNS * ROWS {
        write_cell(cell - COLUMNS, read_cell(cell));
    }
    for cell in COLUMNS * (ROWS - 1)..COLUMNS * ROWS {
        write_cell(cell, ATTRIBUTE | u16::from(b' '));
    }
}

fn put_byte(byte: u8) {
    let mut cursor = CURSOR.load(Ordering::Relaxed);
    if byte == b'\n' {
        cursor = (cursor / COLUMNS + 1) * COLUMNS;
    } else {
        let printable = if byte.is_ascii_graphic() || byte == b' ' { byte } else { b'?' };
        write_cell(cursor, ATTRIBUTE | u16::from(printable));
        cursor += 1;
    }
    if cursor >= COLUMNS * ROWS {
        scroll();
        cursor = COLUMNS * (ROWS - 1);
    }
    CURSOR.store(cursor, Ordering::Relaxed);
}

impl fmt::Write for Screen {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        s.bytes().for_each(put_byte);
        Ok(())
    }
}
