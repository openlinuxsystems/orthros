//! Kernel console: the first serial port, plus the VGA text screen when the
//! machine booted through BIOS.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch;

static VGA_ENABLED: AtomicBool = AtomicBool::new(false);

/// Mirrors console output to the VGA text screen. Only call this after
/// confirming a BIOS boot; UEFI machines have no text buffer at 0xB8000.
pub fn enable_vga() {
    arch::vga::clear();
    VGA_ENABLED.store(true, Ordering::Relaxed);
}

/// Backend for [`kprint!`](crate::kprint) and [`kprintln!`](crate::kprintln).
pub fn print(args: fmt::Arguments<'_>) {
    // Neither writer can fail; the results only satisfy `fmt::Write`.
    let mut serial = arch::serial::Serial::COM1;
    let _ = serial.write_fmt(args);
    if VGA_ENABLED.load(Ordering::Relaxed) {
        let _ = arch::vga::Screen.write_fmt(args);
    }
}

/// Prints to the kernel console.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        $crate::console::print(format_args!($($arg)*))
    };
}

/// Prints to the kernel console, followed by a newline.
#[macro_export]
macro_rules! kprintln {
    () => {
        $crate::kprint!("\n")
    };
    ($($arg:tt)*) => {
        $crate::console::print(format_args!("{}\n", format_args!($($arg)*)))
    };
}
