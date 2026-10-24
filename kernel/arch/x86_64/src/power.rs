// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Restarting the machine, powering it off, leaving QEMU, and polling the
//! keyboard.

use radix_acpi::{GenericAddress, Memory, Sdt};

use crate::io::{inb, inw, outb, outl, outw};
use crate::paging::{self, Access, Cache};
use crate::{acpi, cpu, time};

/// PM1 control: the SCI interrupt is on, so ACPI owns the hardware.
const SCI_EN: u16 = 1 << 0;
/// PM1 control: where SLP_TYP goes.
const SLP_TYP_SHIFT: u16 = 10;
/// PM1 control: enter the sleep state SLP_TYP names.
const SLP_EN: u16 = 1 << 13;

/// Reads a PM1 control register.
fn read_control(register: GenericAddress) -> Option<u16> {
    match register.space {
        GenericAddress::IO if register.address <= 0xffff => {
            // SAFETY: the FADT names this port as a PM1 control register;
            // reading it has no side effects.
            let value = unsafe { inw(register.address as u16) };
            Some(value)
        }
        GenericAddress::MEMORY => {
            let virt =
                paging::map_physical(register.address, 2, Access::ReadWrite, Cache::Uncached);
            // SAFETY: `virt` maps the register the FADT names, uncached.
            Some(unsafe { (virt as *const u16).read_volatile() })
        }
        _ => None,
    }
}

/// Writes a PM1 control register.
fn write_control(register: GenericAddress, value: u16) {
    match register.space {
        // SAFETY: the FADT names this port as a PM1 control register, and
        // writing it with SLP_EN is how ACPI powers the machine off.
        GenericAddress::IO if register.address <= 0xffff => unsafe {
            outw(register.address as u16, value)
        },
        GenericAddress::MEMORY => {
            let virt =
                paging::map_physical(register.address, 2, Access::ReadWrite, Cache::Uncached);
            // SAFETY: as above, for a memory-mapped register.
            unsafe { (virt as *mut u16).write_volatile(value) };
        }
        _ => {}
    }
}

/// The SLP_TYPa and SLP_TYPb values of S5, from the DSDT or else an SSDT.
fn s5_types(acpi: &acpi::Acpi, dsdt: u64) -> Option<(u8, u8)> {
    let memory = acpi::PhysicalMemory;
    let dsdt = Sdt::load(&memory, dsdt, Some(b"DSDT")).ok();
    dsdt.and_then(|dsdt| radix_acpi::sleep_types(dsdt.body(), 5)).or_else(|| {
        acpi.tables
            .iter()
            .filter(|table| &table.signature == b"SSDT" && table.status.is_ok())
            .filter_map(|table| memory.bytes(table.address, table.length as usize))
            .find_map(|bytes| radix_acpi::sleep_types(bytes, 5))
    })
}

/// Powers the machine off through ACPI's soft-off state, S5: writes S5's
/// sleep types with SLP_EN into the PM1 control registers, after taking
/// the machine into ACPI mode if the firmware left it out. Returns if the
/// machine has no such registers or no S5, or is still on a second later.
pub fn power_off() {
    let Some(acpi) = acpi::get() else { return };
    let Some(fadt) = acpi.fadt else { return };
    let (Some(control), false) = (fadt.pm1a_control, fadt.hardware_reduced) else {
        radix_klog::warn!("Power: the firmware describes no way to power off");
        return;
    };
    let Some((type_a, type_b)) = s5_types(acpi, fadt.dsdt) else {
        radix_klog::warn!("Power: the ACPI tables have no S5 sleep state");
        return;
    };
    radix_klog::info!("Power: powering off (ACPI S5, sleep types {type_a} and {type_b})");
    if let Some((port, value)) = fadt.acpi_enable
        && read_control(control).is_some_and(|now| now & SCI_EN == 0)
    {
        // SAFETY: the FADT names this port and value as the way into ACPI
        // mode; the firmware hands the power-management hardware over.
        unsafe { outb(port, value) };
        for _ in 0..300 {
            if read_control(control).is_some_and(|now| now & SCI_EN != 0) {
                break;
            }
            time::delay_us(10_000);
        }
    }
    let were_enabled = cpu::disable_interrupts();
    cpu::wbinvd();
    let sleep = |register: GenericAddress, sleep_type: u8| {
        let kept = read_control(register).unwrap_or(0) & !(7 << SLP_TYP_SHIFT) & !SLP_EN;
        write_control(register, kept | (u16::from(sleep_type) << SLP_TYP_SHIFT) | SLP_EN);
    };
    sleep(control, type_a);
    if let Some(control) = fadt.pm1b_control {
        sleep(control, type_b);
    }
    time::delay_us(1_000_000);
    if were_enabled {
        cpu::enable_interrupts();
    }
    radix_klog::error!("Power: the machine is still on a second after S5");
}

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
