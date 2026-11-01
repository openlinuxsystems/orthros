// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Two of QEMU's USB serial ports, FTDI chips, on its xHCI controller,
//! wired to each other through a Unix socket on the host: the kernel makes each a
//! terminal, `/devices/ttyUSB0` and `/devices/ttyUSB1`, stty sets both to
//! 115200 baud and raw, what is written to either is read from the other,
//! lsusb shows the driver, and the port unplugged takes its terminal away,
//! ending the program that read it (ADR 0246).

use super::super::*;

pub const CASE: Case = Case {
    name: "usb-serial",
    about: "two FTDI USB serial ports wired to each other: each is a terminal, stty sets them, what is written to one is read from the other, and one unplugged hangs its reader up",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep console-sign-in=off",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[
        "-chardev",
        SERIAL_LINE[0],
        "-chardev",
        SERIAL_LINE[1],
        "-device",
        "usb-serial,bus=xhci.0,chardev=line0,id=ftdi0",
        "-device",
        "usb-serial,bus=xhci.0,chardev=line1,id=ftdi1",
    ],
    script: usb_serial,
    disks: &[],
    second: None,
};

/// The `usb-serial` case.
fn usb_serial(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    for line in [
        "USB: 1-6: QEMU USB SERIAL (0403:6001, QEMU), vendor-specific, full speed",
        "USB: 1-6: interface 0: FT232BM, the terminal /devices/ttyUSB0",
        "USB: 1-7: QEMU USB SERIAL (0403:6001, QEMU), vendor-specific, full speed",
        "USB: 1-7: interface 0: FT232BM, the terminal /devices/ttyUSB1",
    ] {
        vm.expect_anywhere(line)?;
    }
    vm.expect("consoled: 6 consoles; tty1 is shown")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    // Both at 115200 baud, raw, without echo; the settings stay.
    vm.type_text("for t in 0 1; do stty -F /devices/ttyUSB$t 115200 raw -echo; done\n")?;
    vm.type_text("stty -F /devices/ttyUSB1 -a | head -1\n")?;
    vm.expect_screen(&["speed 115200 baud; rows 0; columns 0;"])?;
    // Each way: the reader first, then the writer. What the screen shows
    // is worked out, so the command typed does not show it.
    vm.type_text(
        "head -c 7 /devices/ttyUSB1 > /tmp/got & sleep 1; echo port$((6 * 7)) > /devices/ttyUSB0; wait; cat /tmp/got\n",
    )?;
    vm.expect_screen(&["port42"])?;
    vm.type_text(
        "head -c 7 /devices/ttyUSB0 > /tmp/got & sleep 1; echo back$((6 * 8)) > /devices/ttyUSB1; wait; cat /tmp/got\n",
    )?;
    vm.expect_screen(&["back48"])?;
    vm.type_text("lsusb -t\n")?;
    vm.expect_screen(&[
        "    |__ Port 006: Dev 003, If 0, Class=Vendor Specific Class, Driver=ftdi_sio, 12M",
        "    |__ Port 007: Dev 004, If 0, Class=Vendor Specific Class, Driver=ftdi_sio, 12M",
    ])?;
    // A reader of the port unplugged finds the end of the file.
    vm.type_text("cat /devices/ttyUSB0 & sleep 1\n")?;
    vm.monitor("device_del ftdi0")?;
    vm.expect("USB: 1-6: QEMU USB SERIAL was unplugged")?;
    vm.type_text("wait; echo $((40 + 3)) read; echo left: $(ls /devices | grep tty.SB)\n")?;
    vm.expect_screen(&["43 read", "left: ttyUSB1"])?;
    vm.type_text("poweroff\n")?;
    vm.expect("spark: asked to power off the machine")?;
    vm.expect("spark: every service has stopped")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}
