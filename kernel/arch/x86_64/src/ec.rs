// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The I/O ports of the Chromium EC's host interface (ADR 0207), which
//! the kernel's `crosec` reaches through the HAL once the firmware names
//! such an EC, and no others.

use crate::io::{inb, outb};

/// Whether the EC's interface may be read at `port`: its data and command
/// ports, its packet, and its memory map at either of its places.
fn readable(port: u16) -> bool {
    matches!(port, 0x200 | 0x204 | 0x800..0x900 | 0x900..0x9ff | 0xe00..0xeff)
}

/// Whether it may be written at `port`: the command port and the packet.
fn writable(port: u16) -> bool {
    matches!(port, 0x204 | 0x800..0x900)
}

/// Reads `port`; all ones for a port that is not the EC's.
pub fn read(port: u16) -> u8 {
    if !readable(port) {
        return 0xff;
    }
    // SAFETY: the EC's ports, which move no memory.
    unsafe { inb(port) }
}

/// Writes `port`, if it is one of the EC's that the host writes.
pub fn write(port: u16, value: u8) {
    if writable(port) {
        // SAFETY: as in `read`.
        unsafe { outb(port, value) }
    }
}
