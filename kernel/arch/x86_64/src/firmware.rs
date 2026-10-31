// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the interpreter of the firmware's AML needs from a PC (Phase 11c,
//! ADR 0200): where the RSDP is, physical memory mapped as AML names it,
//! I/O ports, and the System Control Interrupt.
//!
//! AML reaches whatever the firmware's code names, by design, so these
//! do what they are asked without knowing why; only uACPI, which runs
//! the firmware's AML, calls them, and the kernel trusts that AML as it
//! trusts the firmware, which runs beneath it in System Management Mode.

use radix_sync::{Once, SpinLock};

use crate::io::{inb, inl, inw, outb, outl, outw};
use crate::paging::{self, Access, Cache};
use crate::{acpi, apic, devices, entry, memory, smbus, smp};

/// The most one mapping of firmware memory takes: a region the firmware
/// declares larger fails alone, rather than costing the kernel its memory
/// for page tables.
const MAX_MAPPING: usize = 256 << 20;

/// The physical address of the RSDP: where the BIOS areas have it, or the
/// boot loader's copy in the boot information, which stays reserved.
pub fn rsdp() -> Option<u64> {
    let acpi = acpi::get()?;
    match acpi.rsdp_address {
        Some(address) => Some(address),
        // The boot information is read through the direct map.
        None => Some(entry::RSDP_COPY.get()?.as_ptr() as u64 - memory::direct_map()),
    }
}

/// Maps `len` bytes from `phys`: RAM through the direct map, anything else
/// uncached in the mapping window, until [`unmap`].
pub fn map(phys: u64, len: usize) -> Option<usize> {
    let end = phys.checked_add(len.max(1) as u64)?;
    if memory::is_ram(phys..end) {
        return Some(memory::phys_to_virt(phys));
    }
    if len > MAX_MAPPING {
        radix_klog::warn!(
            "ACPI: the firmware asked for {} MiB of memory at {phys:#x} mapped; over 256 MiB, \
             it is refused",
            len >> 20
        );
        return None;
    }
    paging::try_map_physical(phys, len as u64, Access::ReadWrite, Cache::Uncached)
}

/// Undoes a [`map`]: a mapping in the window goes, the direct map stays.
pub fn unmap(virt: usize, len: usize) {
    let window = memory::window();
    if (window..window + memory::WINDOW_SIZE).contains(&(virt as u64)) {
        paging::unmap_physical(virt, len);
    }
}

/// Reads `width` bytes (1, 2 or 4) from I/O port `port`.
pub fn read_port(port: u16, width: u8) -> u32 {
    smbus::firmware_port(port);
    // SAFETY: the firmware's AML names the port (see the module's
    // comment); a read touches no memory.
    unsafe {
        match width {
            1 => u32::from(inb(port)),
            2 => u32::from(inw(port)),
            _ => inl(port),
        }
    }
}

/// Writes `width` bytes (1, 2 or 4) of `value` to I/O port `port`.
pub fn write_port(port: u16, width: u8, value: u32) {
    smbus::firmware_port(port);
    // SAFETY: the firmware's AML names the port and the value, as its
    // code has the machine do (see the module's comment).
    unsafe {
        match width {
            1 => outb(port, value as u8),
            2 => outw(port, value as u16),
            _ => outl(port, value),
        }
    }
}

/// The handler [`start_interrupt`] was given.
static HANDLER: Once<fn()> = Once::new();

/// The vector and global system interrupt routed for it, until stopped.
static ROUTED: SpinLock<Option<(u32, u32)>> = SpinLock::new(None);

/// Routes the SCI, ISA interrupt `irq`, to `handler` on the boot CPU,
/// level-triggered and active low unless the MADT says otherwise, as
/// ADR 0142's own SCI. Only once: false if it was routed before, or
/// cannot be.
pub fn start_interrupt(irq: u32, handler: fn()) -> bool {
    let Ok(irq) = u8::try_from(irq) else { return false };
    if irq >= 16 || HANDLER.set(handler).is_err() {
        return false;
    }
    let Some((vector, _)) = devices::allocate(None, 0, interrupt, 0) else {
        radix_klog::warn!("ACPI: no interrupt vector is free for the SCI");
        return false;
    };
    let madt = acpi::get().and_then(|acpi| acpi.madt.as_ref());
    let destination = smp::info(0).map_or(0, |cpu| cpu.hardware_id);
    let Some(gsi) = apic::route_sci(madt, irq, vector as u8, destination) else {
        radix_klog::warn!("ACPI: the SCI, ISA interrupt {irq}, could not be routed");
        devices::free(vector);
        return false;
    };
    *ROUTED.lock_irq() = Some((vector, gsi));
    true
}

/// Masks the SCI [`start_interrupt`] routed and frees its vector.
pub fn stop_interrupt() {
    let routed = ROUTED.lock_irq().take();
    if let Some((vector, gsi)) = routed {
        apic::mask_gsi(gsi);
        devices::free(vector);
    }
}

fn interrupt(_: usize) {
    if let Some(handler) = HANDLER.get() {
        handler();
    }
}
