// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Phase 11's boot tests (hardware support), added after this file was made. A new
//! test of the phase goes here, with its script, so that the phases built
//! in parallel add theirs to different files. The test still needs its
//! entry in ob(1)'s list (in Phase 11's part of it).

#[allow(unused_imports)]
use super::*;

/// Phase 11's boot tests, run after those in `super::CASES`.
pub const CASES: &[Case] = &[Case {
    name: "usb-ehci",
    about: "QEMU's EHCI controller, without companions, names a keyboard, which types, and a tablet, and lsusb shows its bus",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep console-sign-in=off",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    // The keyboard takes QEMU's input from the screen, as in usb-input.
    qemu: &[
        "-vga",
        "none",
        "-device",
        "VGA,id=video0",
        "-device",
        "usb-ehci,id=ehci,addr=0x1d",
        "-device",
        "usb-kbd,bus=ehci.0,port=1,id=ehcikbd,display=video0",
        "-device",
        "usb-tablet,bus=ehci.0,port=2",
    ],
    script: usb_ehci,
    disks: &[],
    second: None,
}];

/// The `usb-ehci` case.
fn usb_ehci(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    for line in [
        "USB: bus 2 is the EHCI 1.0 controller at 0000:00:1d.0: 6 ports, 0 companion controllers, polled",
        "USB: 2-1: QEMU USB Keyboard (0627:0001, QEMU), keyboard, high speed",
        "USB: 2-1: interface 0: a keyboard, read into /devices/kbd",
        "USB: 2-2: QEMU USB Tablet (0627:0001, QEMU), HID, high speed",
        "USB: 2-2: interface 0: a tablet, read into /devices/mouse",
    ] {
        vm.expect_anywhere(line)?;
    }
    vm.expect("consoled: 6 consoles; tty1 is shown")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    vm.type_on(VIDEO, "echo $((6 * 7)) ehci\n")?;
    vm.expect_screen(&["# echo $((6 * 7)) ehci", "42 ehci"])?;
    vm.type_text("lsusb -t\n")?;
    vm.expect_screen(&[
        "/:  Bus 002.Port 001: Dev 001, Class=root_hub, Driver=ehci-pci/6p, 480M",
        "    |__ Port 001: Dev 002, If 0, Class=Human Interface Device, Driver=usbhid, 480M",
        "    |__ Port 002: Dev 003, If 0, Class=Human Interface Device, Driver=usbhid, 480M",
    ])?;
    vm.monitor("device_del ehcikbd")?;
    vm.expect("USB: 2-1: QEMU USB Keyboard was unplugged")?;
    vm.type_text("poweroff\n")?;
    vm.expect("spark: asked to power off the machine")?;
    vm.expect("spark: every service has stopped")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}
