// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The network cards QEMU emulates besides virtio's, side by side behind
//! its Intel IOMMU, each on a user network of its own (ADR 0197): an
//! e1000 (82540EM, as QEMU's and VirtualBox's) and an 82545EM (VMware's),
//! which have no MSI and are polled, an e1000e (82574L) and an igb
//! (82576), with an MSI each. The kernel must name each card, its MAC
//! address and its interrupt; netd must set each up by DHCP, from the
//! network behind it; and userspace/posix/nics.c must send 2 MiB through
//! each to QEMU's `cat` and get every byte back in order.

use super::super::*;

pub const CASE: Case = Case {
    name: "network-cards",
    about: "QEMU's e1000, 82545EM, e1000e and igb behind its IOMMU, each on its own network: the kernel drives each, netd sets each up by DHCP, and 2 MiB go through each and back (ADR 0197)",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep -- --service-dir /system/tests/services/dhcp --posix --network /system/tests/posix/nics net0 10.0.2 net1 10.0.3 net2 10.0.4 net3 10.0.5",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[
        "-device",
        "intel-iommu,intremap=on",
        "-netdev",
        "user,id=n0,net=10.0.2.0/24,guestfwd=tcp:10.0.2.100:7-cmd:cat",
        "-device",
        "e1000,netdev=n0,mac=52:54:00:12:34:60",
        "-netdev",
        "user,id=n1,net=10.0.3.0/24,guestfwd=tcp:10.0.3.100:7-cmd:cat",
        "-device",
        "e1000-82545em,netdev=n1,mac=52:54:00:12:34:61",
        "-netdev",
        "user,id=n2,net=10.0.4.0/24,guestfwd=tcp:10.0.4.100:7-cmd:cat",
        "-device",
        "e1000e,netdev=n2,mac=52:54:00:12:34:62",
        "-netdev",
        "user,id=n3,net=10.0.5.0/24,guestfwd=tcp:10.0.5.100:7-cmd:cat",
        "-device",
        "igb,netdev=n3,mac=52:54:00:12:34:63",
    ],
    script: network_cards,
    disks: &[],
    second: None,
};

/// Each card: its interface, what the kernel says it is, its MAC address
/// and slots, how it interrupts, and its network.
const CARDS: &[(&str, &str, &str, &str, &str)] = &[
    ("net0", "an Intel 82540EM", "52:54:00:12:34:60, 255 receive and 255 transmit", "polled", "10.0.2"),
    ("net1", "an Intel 82545EM", "52:54:00:12:34:61, 255 receive and 255 transmit", "polled", "10.0.3"),
    ("net2", "an Intel 82574L", "52:54:00:12:34:62, 255 receive and 255 transmit", "MSI vector ", "10.0.4"),
    ("net3", "an Intel 82576", "52:54:00:12:34:63, 255 receive and 255 transmit", "MSI vector ", "10.0.5"),
];

fn network_cards(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    // The cards are found during the PCI scan, among lines boot_report
    // reads past.
    let log = vm.log();
    for (name, what, mac, how, _) in CARDS {
        let start = format!("Net: {name} is {what} network card at ");
        let line = log
            .lines()
            .find_map(|line| line.find(start.as_str()).map(|at| &line[at..]))
            .ok_or_else(|| format!("the kernel never said \"{start}\""))?;
        let described = format!(", MAC {mac} slots, {how}");
        if !line.contains(described.as_str()) {
            return Err(format!("the kernel described {name} as \"{line}\""));
        }
        // Behind the IOMMU.
        let address = line.split(" at ").nth(1).and_then(|rest| rest.split(',').next());
        let confined = format!("IOMMU: {} reaches only what is mapped", address.unwrap_or("?"));
        if !log.contains(confined.as_str()) {
            return Err(format!("the kernel never said \"{confined}\""));
        }
    }
    vm.expect("spark: 1 services in /system/tests/services/dhcp, 1 enabled")?;
    // netd and the program run at once: their lines are read ahead.
    for (name, _, _, _, net) in CARDS {
        vm.expect_ahead(&format!(
            "netd: {name} has {net}.15/24 from {net}.2 for 1 day, router {net}.2, DNS {net}.3"
        ))?;
        vm.expect_ahead(&format!("nics: {name} echoed 2 MiB through {net}.100 port 7 in "))?;
    }
    let code = program_line(vm, "(/system/tests/posix/nics) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> = log.lines().filter(|line| line.contains("nics: FAILED: ")).collect();
    if !failed.is_empty() || !log.contains("nics: every check passed") || code != "0x0" {
        return Err(format!("the network-cards test failed ({code}): {}", failed.join("; ")));
    }
    Ok(())
}
