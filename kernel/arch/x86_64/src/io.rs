// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Port I/O instructions.
//!
//! They are `nomem`, like Linux's, which have no memory clobber: the
//! compiler may move memory accesses across them. A driver whose device
//! reads memory the CPU wrote (DMA) puts a fence before the port write
//! that tells the device to look.

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
        asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags));
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
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
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
        asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
    };
}

/// Reads a 32-bit value from an I/O port.
///
/// # Safety
///
/// As for [`inb`].
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    // SAFETY: the caller upholds the contract above; `in` touches no memory.
    unsafe {
        asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack, preserves_flags));
    };
    value
}

/// Reads a 16-bit value from an I/O port.
///
/// # Safety
///
/// As for [`inb`].
pub unsafe fn inw(port: u16) -> u16 {
    let value: u16;
    // SAFETY: the caller upholds the contract above; `in` touches no memory.
    unsafe {
        asm!("in ax, dx", in("dx") port, out("ax") value, options(nomem, nostack, preserves_flags));
    };
    value
}

/// Writes a 16-bit value to an I/O port.
///
/// # Safety
///
/// As for [`outb`].
pub unsafe fn outw(port: u16, value: u16) {
    // SAFETY: the caller upholds the contract above; `out` touches no memory.
    unsafe {
        asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags));
    };
}

/// Waits about a microsecond by writing to the POST diagnostic port, which
/// nothing listens to on a running system.
pub fn io_delay() {
    // SAFETY: port 0x80 is the POST code port; writes have no effect once
    // the firmware has finished.
    unsafe { outb(0x80, 0) };
}
