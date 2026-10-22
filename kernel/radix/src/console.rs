// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The framebuffer console: the kernel log on the screen, and the screen
//! the panic report takes over.

use core::sync::atomic::{AtomicBool, Ordering};

use radix_fbcon::Console;
use radix_hal::Framebuffer;
use radix_klog::{Sink, Style, info, warn};
use radix_sync::{SpinLock, TakeOnce};

/// Cells of grid storage: enough for 1920x1080 with the smallest font
/// (240 x 67, twice), or 4K with the font picked for it.
const STORAGE_CELLS: usize = 96 * 1024;

static STORAGE: TakeOnce<[u32; STORAGE_CELLS]> = TakeOnce::new([0; STORAGE_CELLS]);
static SCREEN: SpinLock<Option<Console>> = SpinLock::new(None);
/// Set by the panic path, which draws its report itself.
static TAKEN_OVER: AtomicBool = AtomicBool::new(false);

/// The kernel log's framebuffer console.
struct ScreenSink;

impl Sink for ScreenSink {
    fn write(&self, text: &str, style: Style) {
        if TAKEN_OVER.load(Ordering::Relaxed) {
            return;
        }
        if let Some(console) = SCREEN.lock().as_mut() {
            console.write(text, style);
            console.flush();
        }
    }
}

static SCREEN_SINK: ScreenSink = ScreenSink;

/// Starts the framebuffer console with the font named by the `font` flag,
/// or one chosen for the screen size.
pub fn init_framebuffer(framebuffer: Framebuffer, font: Option<&str>) {
    let auto = radix_fbcon::choose_font(framebuffer.width, framebuffer.height);
    let font_name = match font {
        Some(name) if radix_fbcon::builtin_font(name).is_some() => name,
        Some(name) => {
            warn!("Framebuffer: no built-in font `{name}`; using {auto}");
            auto
        }
        None => auto,
    };
    let Some(font) = radix_fbcon::builtin_font(font_name) else { return };
    let Some(storage) = STORAGE.take() else { return };
    let (width, height) = (framebuffer.width, framebuffer.height);
    match Console::new(framebuffer, font, storage) {
        Some(console) => {
            *SCREEN.lock() = Some(console);
            radix_klog::add_sink(&SCREEN_SINK, true);
        }
        None => warn!("Framebuffer: cannot draw text on a {width}x{height} framebuffer"),
    }
}

/// Logs what the screen console looks like.
pub fn report() {
    let screen = SCREEN.lock();
    let Some(console) = screen.as_ref() else {
        return;
    };
    let fb = console.framebuffer();
    let (font_width, font_height) = console.font_size();
    let (x, y) = console.origin();
    let (cols, rows) = (console.cols(), console.rows());
    let (address, width, height, pitch, bpp) =
        (fb.phys_addr, fb.width, fb.height, fb.pitch, fb.format.bytes_per_pixel);
    drop(screen);
    info!(
        "Framebuffer: {width}x{height}, {} bits per pixel, pitch {pitch}, at {address:#x}",
        u32::from(bpp) * 8
    );
    info!(
        "Framebuffer: console of {cols}x{rows} cells with font {font_width}x{font_height} at ({x}, {y})"
    );
}

/// Whether there is a framebuffer console.
pub fn present() -> bool {
    SCREEN.lock().is_some()
}

/// Stops log output to the screen and hands it to `draw`, for the panic
/// report.
pub fn take_over(draw: impl FnOnce(&mut Console)) {
    TAKEN_OVER.store(true, Ordering::Relaxed);
    if let Some(console) = SCREEN.lock().as_mut() {
        draw(console);
        console.flush();
    }
}

/// Changes one line of the screen in place, such as a restart countdown.
pub fn update_line(row_from_bottom: usize, text: &str, style: Style) {
    if let Some(console) = SCREEN.lock().as_mut() {
        let row = console.rows().saturating_sub(1 + row_from_bottom);
        console.fill_row(row, Style::Plain);
        console.set_position(0, row);
        console.write(text, style);
        console.flush();
    }
}
