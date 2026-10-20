//! 16550 UART driver: the debug console.
//!
//! COM1 carries the kernel log and the kernel debugger. The Dell Inspiron
//! 15 5570 has no serial port; [`init`] notices that the UART's scratch
//! register does not hold a value and turns the console off, so writes cost
//! nothing there.

use core::sync::atomic::{AtomicBool, Ordering};

use radix_klog::{Sink, Style};
use radix_sync::SpinLock;

use super::io::{inb, outb};

/// A 16550-compatible UART at a fixed I/O port base.
#[derive(Clone, Copy)]
pub struct Serial {
    base: u16,
}

/// Line-status register bit: a received byte is waiting.
const LSR_DATA_READY: u8 = 1 << 0;
/// Line-status register bit: the transmit holding register is empty.
const LSR_THR_EMPTY: u8 = 1 << 5;

/// Polls before giving up on a byte, so a stuck UART cannot hang the kernel.
const TX_SPIN_LIMIT: u32 = 100_000;

/// The first serial port.
const COM1: Serial = Serial { base: 0x3f8 };

static PRESENT: AtomicBool = AtomicBool::new(false);
/// Serialises whole writes, so lines from different CPUs do not interleave.
static LOCK: SpinLock<()> = SpinLock::new(());

impl Serial {
    /// Checks that a UART answers, then programs 115200 baud, 8 data bits,
    /// no parity, one stop bit, FIFOs on and interrupts off.
    fn init(self) -> bool {
        self.write_reg(7, 0x5a);
        if self.read_reg(7) != 0x5a {
            return false;
        }
        self.write_reg(1, 0x00); // interrupt enable: none
        self.write_reg(3, 0x80); // line control: DLAB on
        self.write_reg(0, 0x01); // divisor low: 115200 / 1
        self.write_reg(1, 0x00); // divisor high
        self.write_reg(3, 0x03); // line control: 8N1, DLAB off
        self.write_reg(2, 0xc7); // FIFO control: enable, clear, 14-byte threshold
        self.write_reg(4, 0x03); // modem control: DTR and RTS
        true
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

    fn read_byte(self) -> Option<u8> {
        (self.read_reg(5) & LSR_DATA_READY != 0).then(|| self.read_reg(0))
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

/// Finds and programs COM1. Returns whether it exists.
pub fn init() -> bool {
    let present = COM1.init();
    PRESENT.store(present, Ordering::Relaxed);
    present
}

/// Whether COM1 exists.
pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Writes `text` to COM1, turning `\n` into `\r\n`.
pub fn write(text: &str) {
    if !present() {
        return;
    }
    // Interrupts stay off while the lock is held, so a handler on this CPU
    // that logs cannot deadlock against it.
    let were_enabled = crate::cpu::disable_interrupts();
    let guard = LOCK.lock();
    for byte in text.bytes() {
        if byte == b'\n' {
            COM1.write_byte(b'\r');
        }
        COM1.write_byte(byte);
    }
    drop(guard);
    if were_enabled {
        crate::cpu::enable_interrupts();
    }
}

/// A byte received on COM1, if one is waiting.
pub fn read() -> Option<u8> {
    if present() { COM1.read_byte() } else { None }
}

/// The kernel log's serial console.
pub struct SerialSink;

impl Sink for SerialSink {
    fn write(&self, text: &str, _style: Style) {
        write(text);
    }
}
