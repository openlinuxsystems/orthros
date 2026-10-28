// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Restarting the machine, powering it off, leaving QEMU, and polling the
//! keyboard.

use radix_acpi::{Fadt, GenericAddress, Memory, Sdt};

use crate::io::{inb, inw, outb, outl, outw};
use crate::paging::{self, Access, Cache};
use crate::{acpi, cpu, time};

/// PM1 control: the SCI interrupt is on, so ACPI owns the hardware.
const SCI_EN: u16 = 1 << 0;
/// PM1 control: where SLP_TYP goes.
const SLP_TYP_SHIFT: u16 = 10;
/// PM1 control: enter the sleep state SLP_TYP names.
const SLP_EN: u16 = 1 << 13;
/// Sleep control (hardware-reduced ACPI, ACPI 6.5 4.8.3.7): where
/// SLP_TYP goes, three bits from bit 2.
const SLEEP_TYPE_SHIFT: u8 = 2;
/// Sleep control: enter the sleep state SLP_TYP names.
const SLEEP_ENABLE: u8 = 1 << 5;
/// Sleep status: the wake status bit, cleared by writing it.
const WAKE_STATUS: u8 = 1 << 7;

/// A register of ACPI's power management: a PM1 control register, a
/// hardware-reduced machine's sleep control or status register, or one of
/// the event registers the power button's interrupt reads (`sci`).
#[derive(Clone, Copy)]
pub(crate) enum Control {
    /// In I/O space, at this port.
    Port(u16),
    /// In memory, mapped uncached at this address.
    Mapped(usize),
}

impl Control {
    /// The register `register` names, mapped once if it is in memory (the
    /// wait for ACPI mode reads it hundreds of times); `None` if it is in
    /// neither space.
    pub(crate) fn find(register: GenericAddress) -> Option<Self> {
        Self::block(register, 2)
    }

    /// The first register of the `length` bytes of registers at
    /// `register`, mapped together if they are in memory; the others are
    /// reached with [`Control::offset`].
    pub(crate) fn block(register: GenericAddress, length: usize) -> Option<Self> {
        match register.space {
            GenericAddress::IO if register.address + length as u64 <= 0x1_0000 => {
                Some(Self::Port(register.address as u16))
            }
            GenericAddress::MEMORY => Some(Self::Mapped(paging::map_physical(
                register.address,
                length as u64,
                Access::ReadWrite,
                Cache::Uncached,
            ))),
            _ => None,
        }
    }

    /// The register `bytes` past this one, in the block [`Control::block`]
    /// found.
    pub(crate) fn offset(self, bytes: usize) -> Self {
        match self {
            Self::Port(port) => Self::Port(port + bytes as u16),
            Self::Mapped(virt) => Self::Mapped(virt + bytes),
        }
    }

    pub(crate) fn read(self) -> u16 {
        match self {
            // SAFETY: the FADT names this port as one of ACPI's control,
            // status or enable registers; reading them has no side
            // effects.
            Self::Port(port) => unsafe { inw(port) },
            // SAFETY: `virt` maps the register the FADT names, uncached.
            Self::Mapped(virt) => unsafe { (virt as *const u16).read_volatile() },
        }
    }

    pub(crate) fn write(self, value: u16) {
        match self {
            // SAFETY: the FADT names this port as one of ACPI's control,
            // status or enable registers: writing a PM1 control register
            // with SLP_EN is how ACPI powers the machine off, a status
            // register's set bits clear its events, and an enable
            // register's choose which raise the SCI.
            Self::Port(port) => unsafe { outw(port, value) },
            // SAFETY: as above, for a memory-mapped register.
            Self::Mapped(virt) => unsafe { (virt as *mut u16).write_volatile(value) },
        }
    }

    /// Reads an 8-bit register: a general-purpose event register.
    pub(crate) fn read8(self) -> u8 {
        match self {
            // SAFETY: as for `read`.
            Self::Port(port) => unsafe { inb(port) },
            // SAFETY: `virt` maps the register the FADT names, uncached.
            Self::Mapped(virt) => unsafe { (virt as *const u8).read_volatile() },
        }
    }

    /// Writes an 8-bit register: the sleep control and status registers
    /// of a hardware-reduced machine, or a general-purpose event register.
    pub(crate) fn write8(self, value: u8) {
        match self {
            // SAFETY: the FADT names this port as a sleep control or status
            // register, or a general-purpose event one; writing a sleep
            // control register with SLP_EN is how such a machine powers
            // off, and writing a status register's set bits clears them.
            Self::Port(port) => unsafe { outb(port, value) },
            // SAFETY: as above, for a memory-mapped register.
            Self::Mapped(virt) => unsafe { (virt as *mut u8).write_volatile(value) },
        }
    }
}

/// Takes the machine into ACPI mode, if the firmware left it out of it,
/// through the FADT's SMI command port, and waits up to 3 s for SCI_EN in
/// the PM1 control register `control`. Returns whether the machine is in
/// ACPI mode.
pub(crate) fn enter_acpi_mode(fadt: &Fadt, control: Control) -> bool {
    if control.read() & SCI_EN != 0 {
        return true;
    }
    let Some((port, value)) = fadt.acpi_enable else { return false };
    // SAFETY: the FADT names this port and value as the way into ACPI
    // mode; the firmware hands the power-management hardware over.
    unsafe { outb(port, value) };
    for _ in 0..300 {
        if control.read() & SCI_EN != 0 {
            return true;
        }
        time::delay_us(10_000);
    }
    false
}

/// How the machine enters a sleep state.
enum Sleep {
    /// Through the PM1 control registers: the second if there is one.
    Pm1(Control, Option<Control>),
    /// Through the sleep control register of a hardware-reduced machine,
    /// with its status register if there is one.
    Reduced(Control, Option<Control>),
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
/// the machine into ACPI mode if the firmware left it out, or on a
/// hardware-reduced machine, which has none, into its sleep control
/// register. Returns if the machine has no such registers or no S5, or is
/// still on a second later.
pub fn power_off() {
    let Some(acpi) = acpi::get() else { return };
    let Some(fadt) = acpi.fadt else { return };
    // Mapped now: with interrupts masked, below, mapping could not wait.
    let sleep = if fadt.hardware_reduced {
        fadt.sleep_control
            .and_then(Control::find)
            .map(|control| Sleep::Reduced(control, fadt.sleep_status.and_then(Control::find)))
    } else {
        fadt.pm1a_control
            .and_then(Control::find)
            .map(|control| Sleep::Pm1(control, fadt.pm1b_control.and_then(Control::find)))
    };
    let Some(sleep) = sleep else {
        radix_klog::warn!("Power: the firmware describes no way to power off");
        return;
    };
    let Some((type_a, type_b)) = s5_types(acpi, fadt.dsdt) else {
        radix_klog::warn!("Power: the ACPI tables have no S5 sleep state");
        return;
    };
    radix_klog::info!("Power: powering off (ACPI S5, sleep types {type_a} and {type_b})");
    if let Sleep::Pm1(control, _) = sleep {
        enter_acpi_mode(&fadt, control);
    }
    let were_enabled = cpu::disable_interrupts();
    cpu::wbinvd();
    match sleep {
        Sleep::Pm1(first, second) => {
            let enter = |control: Control, sleep_type: u8| {
                let kept = control.read() & !(7 << SLP_TYP_SHIFT) & !SLP_EN;
                control.write(kept | (u16::from(sleep_type) << SLP_TYP_SHIFT) | SLP_EN);
            };
            enter(first, type_a);
            if let Some(control) = second {
                enter(control, type_b);
            }
        }
        Sleep::Reduced(control, status) => {
            // A wake event still marked would wake the machine at once.
            if let Some(status) = status {
                status.write8(WAKE_STATUS);
            }
            control.write8(((type_a & 7) << SLEEP_TYPE_SHIFT) | SLEEP_ENABLE);
        }
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
    unsafe { core::arch::asm!("lidt [{}]", "int3", in(reg) &raw const empty, options(noreturn)) };
}

/// Tells QEMU's isa-debug-exit device to quit with status 33 (success) or
/// 35 (failure). Returns if there is no such device.
pub fn emulator_exit(success: bool) {
    // SAFETY: port 0xF4 is unassigned on PC hardware; under QEMU it is the
    // isa-debug-exit device. Writing it touches no memory.
    unsafe { outl(0xf4, if success { 0x10 } else { 0x11 }) };
}

/// Whether a key was pressed on the built-in keyboard (the i8042 port),
/// draining any waiting bytes. For the panic screen, where interrupts are
/// off and the keyboard driver no longer runs.
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
        // waiting byte, which the stopped keyboard driver can no longer
        // want.
        let byte = unsafe { inb(0x60) };
        // Bit 5 marks mouse data; scan codes below 0x80 are key presses.
        if status & 0x20 == 0 && byte < 0x80 {
            pressed = true;
        }
    }
    pressed
}
