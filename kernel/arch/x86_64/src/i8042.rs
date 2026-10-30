// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The i8042 PS/2 controller and the keyboard behind it (Phase 9, ADR
//! 0097): QEMU's default keyboard, and how laptops such as the reference
//! Dell connect theirs.
//!
//! [`start`] sets the controller up in the steps of the OSDev wiki's
//! "8042 PS/2 Controller" page, short of the self-tests, which reset some
//! controllers: it turns both ports off, empties the output buffer, and
//! writes the configuration byte back with the second port's clock off,
//! its interrupt off, and translation into scan code set 1 on, as firmware
//! leaves it (`radix-input` decodes set 1). Then it turns the first port
//! on, tells the keyboard to scan (0xF4, which some firmware leaves off),
//! routes ISA interrupt 1 to a device vector through the I/O APIC, and
//! turns the first port's interrupt on.
//!
//! The interrupt handler reads every byte waiting and passes the
//! keyboard's to the kernel's function; a mouse's go nowhere until a
//! driver wants them. A controller whose status port reads 0xFF is not
//! there. USB keyboards that the firmware emulates as PS/2 ones (until a
//! USB driver takes the controller, Phase 11) come through here too.

use radix_sync::Once;

use crate::io::{inb, outb};
use crate::{acpi, apic, devices, smp, time};

/// The data port: bytes from the keyboard, and to it.
const DATA: u16 = 0x60;
/// The status port when read, the command port when written.
const STATUS: u16 = 0x64;

/// Status: a byte waits in the output buffer.
const OUTPUT_FULL: u8 = 1 << 0;
/// Status: the controller has not taken the last byte written yet.
const INPUT_FULL: u8 = 1 << 1;
/// Status: the waiting byte is the second (mouse) port's.
const AUX_DATA: u8 = 1 << 5;

/// Configuration: the first port's interrupt.
const CONFIG_IRQ1: u8 = 1 << 0;
/// Configuration: the second port's interrupt.
const CONFIG_IRQ12: u8 = 1 << 1;
/// Configuration: the first port's clock is off.
const CONFIG_CLOCK1_OFF: u8 = 1 << 4;
/// Configuration: the second port's clock is off.
const CONFIG_CLOCK2_OFF: u8 = 1 << 5;
/// Configuration: translation into scan code set 1.
const CONFIG_TRANSLATE: u8 = 1 << 6;

/// Commands to the controller.
const READ_CONFIG: u8 = 0x20;
const WRITE_CONFIG: u8 = 0x60;
const DISABLE_PORT2: u8 = 0xa7;
const DISABLE_PORT1: u8 = 0xad;
const ENABLE_PORT1: u8 = 0xae;

/// The keyboard's command to scan keys, and its answer.
const ENABLE_SCANNING: u8 = 0xf4;
const ACK: u8 = 0xfa;

/// The ISA interrupt of the first port.
const IRQ_KEYBOARD: u8 = 1;

/// How long to wait for the controller, in steps of [`STEP_US`].
const TRIES: u32 = 2_000;
const STEP_US: u64 = 10;

/// The kernel's function for the keyboard's bytes.
static BYTE: Once<fn(u8)> = Once::new();

fn status() -> u8 {
    // SAFETY: reading the 8042's status register has no side effects.
    unsafe { inb(STATUS) }
}

/// Waits until the controller can take a byte.
fn wait_writable() -> bool {
    for _ in 0..TRIES {
        if status() & INPUT_FULL == 0 {
            return true;
        }
        time::delay_us(STEP_US);
    }
    false
}

/// Waits for a byte from the controller and takes it.
fn read_waiting() -> Option<u8> {
    for _ in 0..TRIES {
        if status() & OUTPUT_FULL != 0 {
            // SAFETY: the data port holds the byte the status announced;
            // nothing else reads it while the controller is set up.
            return Some(unsafe { inb(DATA) });
        }
        time::delay_us(STEP_US);
    }
    None
}

/// Writes `byte` to the port `port` once the controller can take it.
fn write(port: u16, byte: u8) -> bool {
    if !wait_writable() {
        return false;
    }
    // SAFETY: `port` is the 8042's data or command port, and every byte
    // written here is a command or argument the controller and keyboard
    // document; neither touches memory.
    unsafe { outb(port, byte) };
    true
}

/// Throws away the bytes waiting in the output buffer.
fn drain() {
    for _ in 0..16 {
        if status() & OUTPUT_FULL == 0 {
            break;
        }
        // SAFETY: as in `read_waiting`.
        let _ = unsafe { inb(DATA) };
    }
}

/// Reads, changes and writes back the configuration byte.
fn configure(change: impl FnOnce(u8) -> u8) -> bool {
    if !write(STATUS, READ_CONFIG) {
        return false;
    }
    let Some(config) = read_waiting() else { return false };
    write(STATUS, WRITE_CONFIG) && write(DATA, change(config))
}

/// Sets up the controller and its keyboard, with `byte` for what the
/// keyboard sends. Returns its name, or `None` if there is none.
pub fn start(byte: fn(u8)) -> Option<&'static str> {
    if status() == 0xff || BYTE.get().is_some() {
        return None;
    }
    let ready = write(STATUS, DISABLE_PORT1) && write(STATUS, DISABLE_PORT2);
    drain();
    let configured = ready
        && configure(|config| {
            (config & !(CONFIG_IRQ1 | CONFIG_IRQ12 | CONFIG_CLOCK1_OFF))
                | CONFIG_CLOCK2_OFF
                | CONFIG_TRANSLATE
        })
        && write(STATUS, ENABLE_PORT1);
    if !configured {
        radix_klog::warn!("Keyboard: the i8042 controller does not answer");
        return None;
    }
    // Scanning on. A keyboard that does not answer may still send keys.
    let answered = write(DATA, ENABLE_SCANNING) && read_waiting() == Some(ACK);
    BYTE.call_once(|| byte);
    let Some((vector, _)) = devices::allocate(None, 0, interrupt, 0) else {
        radix_klog::warn!("Keyboard: no interrupt vector is free");
        return None;
    };
    let destination = smp::info(0).map_or(0, |cpu| cpu.hardware_id);
    let madt = acpi::get().and_then(|acpi| acpi.madt.as_ref());
    let Some(gsi) = apic::route_isa(madt, IRQ_KEYBOARD, vector as u8, destination) else {
        radix_klog::warn!("Keyboard: ISA interrupt {IRQ_KEYBOARD} could not be routed");
        devices::free(vector);
        return None;
    };
    if !configure(|config| config | CONFIG_IRQ1) {
        radix_klog::warn!("Keyboard: the i8042 controller does not take its interrupt");
        return None;
    }
    // A byte that came while the interrupt was off would hold its line
    // high, and an edge-triggered input would never see another edge.
    drain();
    radix_klog::info!(
        "Keyboard: PS/2 keyboard on the i8042 controller, interrupt {IRQ_KEYBOARD} (GSI {gsi}, \
         vector {vector:#x}){}",
        if answered { "" } else { "; it did not answer the scan command" }
    );
    Some("PS/2 keyboard (i8042)")
}

/// Sends `byte` to the keyboard.
pub fn send(byte: u8) -> bool {
    BYTE.get().is_some() && write(DATA, byte)
}

/// The keyboard's interrupt: passes every byte waiting on.
fn interrupt(_: usize) {
    let Some(&byte) = BYTE.get() else { return };
    for _ in 0..16 {
        let status = status();
        if status & OUTPUT_FULL == 0 {
            break;
        }
        // SAFETY: the data port holds the byte the status announced; only
        // this handler reads it once the controller is set up.
        let data = unsafe { inb(DATA) };
        if status & AUX_DATA == 0 {
            byte(data);
        }
    }
}
