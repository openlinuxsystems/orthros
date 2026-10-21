// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Restarting the machine, leaving QEMU, and polling the keyboard.

use radix_acpi::GenericAddress;

use crate::io::{inb, outb, outl};
use crate::paging::{self, Access, Cache};
use crate::{acpi, cpu, time};

/// Restarts the machine without cutting power, so RAM keeps the crash log
/// where the hardware allows. Tries, in order: the FADT's reset register,
/// the keyboard controller's reset line, the PCI reset control register
/// (0xCF9) and finally a triple fault.
pub fn reset() -> ! {
    cpu::disable_interrupts();
    // Make sure everything the crash log wrote reaches RAM.
    cpu::wbinvd();

    if let Some((register, value)) = acpi::get().and_then(|a| a.fadt).and_then(|f| f.reset) {
        match register.space {
            GenericAddress::IO if register.address <= 0xffff => {
                // SAFETY: the firmware names this port and value as the way
                // to reset the machine.
                unsafe { outb(register.address as u16, value) };
            }
            GenericAddress::MEMORY => {
                let virt =
                    paging::map_physical(register.address, 1, Access::ReadWrite, Cache::Uncached);
                // SAFETY: as above, for a memory-mapped reset register.
                unsafe { (virt as *mut u8).write_volatile(value) };
            }
            _ => {}
        }
        time::delay_us(50_000);
    }

    // Pulse the reset line through the 8042 keyboard controller.
    for _ in 0..10_000 {
        // SAFETY: port 0x64 is the 8042's status register; reading it has
        // no side effects.
        if unsafe { inb(0x64) } & 0x02 == 0 {
            break;
        }
        time::delay_us(10);
    }
    // SAFETY: command 0xFE pulses the CPU reset line; that is the point.
    unsafe { outb(0x64, 0xfe) };
    time::delay_us(50_000);

    // A hard (warm) reset through the reset control register.
    // SAFETY: port 0xCF9 is the chipset's reset control register; writing
    // 0x02 then 0x06 requests a hard reset without a power cycle.
    unsafe {
        outb(0xcf9, 0x02);
        time::delay_us(10);
        outb(0xcf9, 0x06);
    }
    time::delay_us(50_000);

    // Last resort: an empty IDT turns the next exception into a triple
    // fault, which resets the CPU.
    let empty: [u8; 10] = [0; 10];
    // SAFETY: deliberately loads an empty IDT and raises an exception so
    // the processor shuts down and the chipset resets it.
    unsafe { core::arch::asm!("lidt [{}]", "int3", in(reg) &empty, options(noreturn)) };
}

/// Tells QEMU's isa-debug-exit device to quit with status 33 (success) or
/// 35 (failure). Returns if there is no such device.
pub fn emulator_exit(success: bool) {
    // SAFETY: port 0xF4 is unassigned on PC hardware; under QEMU it is the
    // isa-debug-exit device. Writing it touches no memory.
    unsafe { outl(0xf4, if success { 0x10 } else { 0x11 }) };
}

/// Whether a key was pressed on the built-in keyboard (the i8042 port),
/// draining any waiting bytes.
pub fn key_pressed() -> bool {
    let mut pressed = false;
    for _ in 0..16 {
        // SAFETY: port 0x64 is the 8042's status register; reading it has
        // no side effects.
        let status = unsafe { inb(0x64) };
        // 0xFF means there is no controller; bit 0 means a byte waits.
        if status == 0xff || status & 0x01 == 0 {
            break;
        }
        // SAFETY: port 0x60 is the 8042's data port; reading it takes the
        // waiting byte, which nothing else in the kernel wants.
        let byte = unsafe { inb(0x60) };
        // Bit 5 marks mouse data; scan codes below 0x80 are key presses.
        if status & 0x20 == 0 && byte < 0x80 {
            pressed = true;
        }
    }
    pressed
}
