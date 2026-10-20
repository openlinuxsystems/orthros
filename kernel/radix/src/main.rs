//! Radix, the OrthrOS kernel.
//!
//! This is the Phase 0 stub. It proves the boot path end to end: GRUB loads
//! the image through Multiboot2, the trampoline in the `radix-arch-x86_64`
//! crate reaches long mode, and [`kernel_entry`] reports what the boot loader
//! handed over on the serial port (and on the VGA text screen under BIOS).
//!
//! Kernel command-line flags understood in Phase 0:
//!
//! - `qemu-exit`: leave QEMU through the `isa-debug-exit` device when done,
//!   with status 33 on success and 35 on panic. Used by `ob test`.

#![no_std]
#![no_main]

mod console;

#[cfg(target_arch = "x86_64")]
use radix_arch_x86_64 as arch;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_bootinfo::{BootInfo, Firmware, MULTIBOOT2_MAGIC};

/// Set once the command line asks for `qemu-exit`, so the panic handler can
/// report failure to the test harness instead of hanging.
static QEMU_EXIT: AtomicBool = AtomicBool::new(false);

/// Called by the boot trampoline in long mode with the Multiboot2 magic and
/// the physical address of the boot information structure.
#[unsafe(no_mangle)]
extern "C" fn kernel_entry(magic: u32, info_addr: usize) -> ! {
    arch::serial_init();

    if magic != MULTIBOOT2_MAGIC {
        panic!("not started by a Multiboot2 boot loader (magic {magic:#x})");
    }
    // SAFETY: the magic matched, so `info_addr` is the boot information
    // address GRUB passed in EBX. Nothing has written to it since, and the
    // Phase 0 kernel has no memory allocator that could ever reuse it.
    let bytes = unsafe { arch::boot_info_bytes(info_addr) }
        .unwrap_or_else(|| panic!("boot information address {info_addr:#x} is invalid"));
    let info = BootInfo::parse(bytes).unwrap_or_else(|err| panic!("bad boot information: {err}"));

    let command_line = info.command_line().unwrap_or("");
    if command_line.split_ascii_whitespace().any(|flag| flag == "qemu-exit") {
        QEMU_EXIT.store(true, Ordering::Relaxed);
    }
    let firmware = info.firmware();
    if firmware == Firmware::Bios {
        console::enable_vga();
    }

    kprintln!("Radix {} (OrthrOS Phase 0 stub kernel)", env!("CARGO_PKG_VERSION"));
    kprintln!("Boot loader: {}", info.boot_loader_name().unwrap_or("unknown"));
    kprintln!("Firmware: {firmware}");
    kprintln!("Command line: {command_line}");
    match info.memory_map() {
        Some(map) => {
            let (bytes, regions) = map.usable_summary();
            kprintln!("Memory: {} MiB usable in {regions} regions", bytes / (1024 * 1024));
        }
        None => kprintln!("Memory: no memory map from the boot loader"),
    }
    kprintln!("Boot complete.");

    if QEMU_EXIT.load(Ordering::Relaxed) {
        arch::qemu_exit(arch::QemuExit::Success);
    }
    arch::halt()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    kprintln!();
    kprintln!("Radix panic: {info}");
    if QEMU_EXIT.load(Ordering::Relaxed) {
        arch::qemu_exit(arch::QemuExit::Failure);
    }
    arch::halt()
}
