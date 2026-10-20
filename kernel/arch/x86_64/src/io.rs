//! Port I/O instructions.

use core::arch::asm;

/// Reads a byte from an I/O port.
///
/// # Safety
///
/// Reading some ports has side effects on devices. The caller must ensure
/// the read cannot break memory safety.
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: the caller upholds the contract above; `in` touches no memory.
    unsafe {
        asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags))
    };
    value
}

/// Writes a byte to an I/O port.
///
/// # Safety
///
/// The caller must ensure the write cannot break memory safety, for example
/// by reprogramming a DMA engine.
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: the caller upholds the contract above; `out` touches no memory.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags))
    };
}

/// Writes a 32-bit value to an I/O port.
///
/// # Safety
///
/// As for [`outb`].
pub unsafe fn outl(port: u16, value: u32) {
    // SAFETY: the caller upholds the contract above; `out` touches no memory.
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags))
    };
}
