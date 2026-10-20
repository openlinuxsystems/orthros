//! x86-64 support for Radix: the boot trampoline, port I/O, the serial port
//! and the VGA text screen.
//!
//! This crate grows into the x86-64 implementation of the hardware
//! abstraction layer in Phase 1. It is part of the trusted computing base,
//! so it may use `unsafe`, and every `unsafe` block states why it is sound.

#![no_std]

mod boot;
mod io;
pub mod serial;
pub mod vga;

/// Initialises the first serial port for console output.
pub fn serial_init() {
    serial::Serial::COM1.init();
}

/// Turns the boot information address into a byte slice.
///
/// Returns `None` if the address is null or not 8-byte aligned.
///
/// # Safety
///
/// `addr` must be the boot information address a Multiboot2 boot loader
/// passed in EBX, and nothing may write to the structure for the rest of the
/// kernel's life.
pub unsafe fn boot_info_bytes(addr: usize) -> Option<&'static [u8]> {
    if addr == 0 || !addr.is_multiple_of(8) {
        return None;
    }
    let ptr = addr as *const u8;
    // SAFETY: EBX is a 32-bit register, so the structure lies below 4 GiB,
    // which the boot trampoline identity-maps. The address is non-null and
    // aligned, and the caller guarantees it points at the structure, whose
    // first field is its total size in bytes.
    let total = unsafe { ptr.cast::<u32>().read() } as usize;
    // SAFETY: the structure spans `total` bytes from `ptr`, inside the same
    // identity-mapped range, and the caller guarantees nothing writes to it.
    Some(unsafe { core::slice::from_raw_parts(ptr, total) })
}

/// Exit status reported through QEMU's `isa-debug-exit` device.
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum QemuExit {
    /// QEMU exits with status 33.
    Success = 0x10,
    /// QEMU exits with status 35.
    Failure = 0x11,
}

/// Leaves QEMU through the `isa-debug-exit` device at port 0xF4. On a
/// machine without that device the write is ignored and the CPU halts.
pub fn qemu_exit(code: QemuExit) -> ! {
    // SAFETY: port 0xF4 is unassigned on PC hardware; under QEMU it is the
    // isa-debug-exit device. Writing it touches no memory.
    unsafe { io::outl(0xF4, code as u32) };
    halt()
}

/// Stops the CPU for good.
pub fn halt() -> ! {
    loop {
        // SAFETY: `cli; hlt` only stops this CPU; it touches no memory.
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) };
    }
}
