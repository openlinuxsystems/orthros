//! 16550 UART driver for console output.
//!
//! The Dell Inspiron 15 5570 has no serial port; there the line-status
//! register reads as 0xFF, writes go nowhere, and nothing blocks.

use core::fmt;

use super::io::{inb, outb};

/// A 16550-compatible UART at a fixed I/O port base.
#[derive(Clone, Copy)]
pub struct Serial {
    base: u16,
}

/// Line-status register bit: the transmit holding register is empty.
const LSR_THR_EMPTY: u8 = 1 << 5;

/// Polls before giving up on a byte, so a stuck UART cannot hang the kernel.
const TX_SPIN_LIMIT: u32 = 100_000;

impl Serial {
    /// The first serial port, COM1.
    pub const COM1: Serial = Serial { base: 0x3F8 };

    /// Programs 115200 baud, 8 data bits, no parity, one stop bit, with
    /// FIFOs on and interrupts off.
    pub fn init(self) {
        self.write_reg(1, 0x00); // interrupt enable: none
        self.write_reg(3, 0x80); // line control: DLAB on
        self.write_reg(0, 0x01); // divisor low: 115200 / 1
        self.write_reg(1, 0x00); // divisor high
        self.write_reg(3, 0x03); // line control: 8N1, DLAB off
        self.write_reg(2, 0xC7); // FIFO control: enable, clear, 14-byte threshold
        self.write_reg(4, 0x03); // modem control: DTR and RTS
    }

    fn write_byte(self, byte: u8) {
        for _ in 0..TX_SPIN_LIMIT {
            if self.read_reg(5) & LSR_THR_EMPTY != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        self.write_reg(0, byte);
    }

    fn read_reg(self, offset: u16) -> u8 {
        // SAFETY: the eight ports from `base` belong to the UART; reading
        // them affects only the UART and touches no memory.
        unsafe { inb(self.base + offset) }
    }

    fn write_reg(self, offset: u16, value: u8) {
        // SAFETY: the eight ports from `base` belong to the UART; writing
        // them affects only the UART and touches no memory.
        unsafe { outb(self.base + offset, value) }
    }
}

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}
