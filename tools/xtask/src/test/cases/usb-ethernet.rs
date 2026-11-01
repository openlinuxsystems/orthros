// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! QEMU's USB Ethernet adapter, `usb-net`, on its xHCI controller, on a
//! user network of its own (ADR 0199). Its first configuration is RNDIS,
//! which the kernel must pass over for its CDC-ECM one; the kernel must
//! give it an interface, `net0`, with the MAC address its string holds;
//! netd must set it up by DHCP; and userspace/posix/nics.c must send
//! 2 MiB through it to QEMU's `cat` and get every byte back in order.
//! Then the adapter is unplugged, which takes its interface down, and
//! plugged in again: the kernel must give it its interface back, not a
//! new one, and netd must get its address again through it.

use super::super::*;

pub const CASE: Case = Case {
    name: "usb-ethernet",
    about: "QEMU's usb-net, a CDC-ECM adapter behind an RNDIS configuration: the kernel gives it net0, netd sets it up by DHCP, 2 MiB go through it and back, and unplugged and plugged in again it gets net0 back (ADR 0199)",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep -- --service-dir /system/tests/services/dhcp --posix --network /system/tests/posix/nics net0 10.0.3",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[
        "-netdev",
        "user,id=n1,net=10.0.3.0/24,guestfwd=tcp:10.0.3.100:7-cmd:cat",
        "-device",
        ADAPTER,
    ],
    script: usb_ethernet,
    disks: &[],
    second: None,
};

/// The adapter, as it is plugged in at boot and again.
const ADAPTER: &str = "usb-net,bus=xhci.0,netdev=n1,mac=52:54:00:12:34:80,id=usbnet0";

/// What the kernel says the adapter is: its address is the one in its
/// string, which QEMU starts with 0x40, not the card's.
const AN_ADAPTER: &str = "USB CDC-ECM network adapter on ";
const ITS_MAC: &str = ", MAC 40:54:00:12:34:80";

/// The place in the log after `start`, on the line that has it.
fn after<'a>(log: &'a str, start: &str) -> Result<&'a str, String> {
    log.lines()
        .find_map(|line| line.find(start).map(|at| &line[at + start.len()..]))
        .ok_or_else(|| format!("the kernel never said \"{start}\""))
}

fn usb_ethernet(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_anywhere("RNDIS/QEMU USB Network Device (0525:a4a2, QEMU)")?;
    vm.expect_anywhere(&format!("Net: net0 is a {AN_ADAPTER}"))?;
    let log = vm.log();
    let rest = after(&log, &format!("Net: net0 is a {AN_ADAPTER}"))?;
    let Some(place) = rest.strip_suffix(ITS_MAC) else {
        return Err(format!("the kernel described the adapter as on \"{rest}\""));
    };
    let place = place.to_string();
    if log.contains("ttyACM") {
        return Err("the kernel took the adapter's RNDIS configuration for a serial port".into());
    }
    vm.expect("spark: 1 services in /system/tests/services/dhcp, 1 enabled")?;
    // netd and nics run at once: their lines are read ahead.
    vm.expect_ahead("netd: net0 has 10.0.3.15/24 from 10.0.3.2 for 1 day, router 10.0.3.2")?;
    vm.expect_ahead("nics: net0 echoed 2 MiB through 10.0.3.100 port 7 in ")?;
    let code = program_line(vm, "(/system/tests/posix/nics) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> = log.lines().filter(|line| line.contains("nics: FAILED: ")).collect();
    if !failed.is_empty() || !log.contains("nics: every check passed") || code != "0x0" {
        return Err(format!("the usb-ethernet test failed ({code}): {}", failed.join("; ")));
    }
    // Unplugged: its interface goes down, and stays, with its address.
    vm.monitor("device_del usbnet0")?;
    vm.expect(&format!("USB: {place}: RNDIS/QEMU USB Network Device was unplugged"))?;
    vm.expect("Net: net0 is down: its USB adapter is gone")?;
    // Long enough for netd to look at the cards and see the link gone,
    // twice over.
    std::thread::sleep(Duration::from_secs(12));
    vm.monitor(&format!("device_add {}", ADAPTER.replace("id=usbnet0", "id=usbnet1")))?;
    vm.expect(&format!("Net: net0 is back: the {AN_ADAPTER}"))?;
    vm.expect_ahead("netd: net0's link is up again; asking for its address")?;
    vm.expect("netd: net0 keeps 10.0.3.15/24 from 10.0.3.2 for 1 day")?;
    let log = vm.log();
    let back = after(&log, &format!("Net: net0 is back: the {AN_ADAPTER}"))?;
    if !back.ends_with(ITS_MAC) {
        return Err(format!("the kernel described the adapter back as on \"{back}\""));
    }
    if log.contains("Net: net1 ") {
        return Err("the adapter plugged in again was given a new interface".into());
    }
    if log.contains("the link's rules") {
        return Err("a side of the adapter's link broke its rules".into());
    }
    Ok(())
}
