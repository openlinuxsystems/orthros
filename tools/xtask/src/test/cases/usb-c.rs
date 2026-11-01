// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! QEMU with an ACPI table of the test's (`src/acpi/ucsi.asl`) whose UCSI
//! device keeps its mailbox in QEMU's test device's memory, and whose
//! device-specific method runs a simulated PPM: the kernel resets it,
//! reads its two USB-C ports, a charger on port 1 that gives 20 V at 3 A
//! by USB Power Delivery and nothing on port 2, then hears the firmware's
//! notify when a phone is plugged into port 2, reads it and acknowledges
//! it. Linux's classes in `/sys/class/typec` and `/sys/class/power_supply`
//! show both ports at the console (ADR 0248).

use super::super::*;

pub const CASE: Case = Case {
    name: "usb-c",
    about: "QEMU with an ACPI table of the test's whose UCSI device runs two simulated USB-C ports: the kernel reads a charger's Power Delivery contract on one, hears a phone plugged into the other, and Linux's typec and power_supply classes show them",
    firmware: Firmware::Bios,
    media: Media::Usb,
    // No program: Spark starts the console, as on a normal boot.
    cmdline: "lockdep console-sign-in=off",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &["-device", "pc-testdev", "-acpitable", UCSI_TABLE],
    script: usb_c,
    disks: &[],
    second: None,
};

/// The `usb-c` case.
fn usb_c(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    for line in [
        "USB-C: the firmware's UCSI 1.2 interface at \\_SB_.UBTC: 2 ports, USB Power Delivery",
        "USB-C: port 1: a charger; the machine takes 20 V at 3 A (60 W) by USB Power Delivery; \
         charging",
        "USB-C: port 2: nothing is plugged in",
        // The phone, once the firmware notifies.
        "USB-C: port 2: a device; the machine gives 5 V at 1.5 A",
    ] {
        vm.expect_anywhere(line)?;
    }
    vm.expect("Boot complete.")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    // `is` between name and value, so that no needle is in a typed line.
    vm.type_text(
        "cd /sys/class/typec; for f in port0/data_role port0/power_role port0/port_type \
         port0/power_operation_mode port1/power_role port1/power_operation_mode; do \
         echo \"$f is $(cat $f)\"; done\n",
    )?;
    vm.expect_screen(&[
        "port0/data_role is host [device]",
        "port0/power_role is source [sink]",
        "port0/port_type is [dual] source sink",
        "port0/power_operation_mode is usb_power_delivery",
        "port1/power_role is [source] sink",
        "port1/power_operation_mode is 1.5A",
    ])?;
    vm.type_text(
        "clear; cd /sys/class/power_supply; for f in type online voltage_now current_now \
         current_max; do echo \"$f is $(cat ucsi-source-psy-USBC000:001/$f)\"; done; \
         echo \"port 2 is $(cat ucsi-source-psy-USBC000:002/online)\"\n",
    )?;
    vm.expect_screen(&[
        "type is USB",
        "online is 1",
        "voltage_now is 20000000",
        "current_now is 3000000",
        "current_max is 3250000",
        "port 2 is 0",
    ])?;
    vm.type_text("poweroff\n")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}
