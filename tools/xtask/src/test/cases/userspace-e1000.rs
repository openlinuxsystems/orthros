// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The e1000 and e1000e driven by a program, e1000d(8) (ADR 0198), with the
//! kernel told `e1000=program`, behind QEMU's Intel IOMMU with interrupt
//! remapping, beside a virtio card the kernel drives, each card on a user
//! network of its own. The kernel must leave the two Intel cards for a
//! program and keep the virtio card; Spark must give both to e1000d, which
//! must start them and attach them to their interfaces; and netd must set
//! every card up by DHCP. Then userspace/posix/nics.c sends 2 MiB through
//! each card to QEMU's `cat`, and kills e1000d with SIGKILL halfway through
//! the e1000's: the kernel must stop the card and put its interface down,
//! Spark must start e1000d again, the card must come back up, and every byte
//! must come back in order, through both Intel cards and the virtio card
//! alike.

use super::super::*;

pub const CASE: Case = Case {
    name: "userspace-e1000",
    about: "e1000=program: e1000d drives QEMU's e1000 and e1000e behind its IOMMU beside a virtio card the kernel drives; nics kills e1000d mid-traffic, and the card, DHCP and the 2 MiB echo come back (ADR 0198)",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep e1000=program -- --service-dir /system/tests/services/e1000d --posix --network /system/tests/posix/nics -k e1000d net1 10.0.3 net2 10.0.4 net0 10.0.2",
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
        "virtio-net-pci,netdev=n0,disable-legacy=on,iommu_platform=on,mac=52:54:00:12:34:70",
        "-netdev",
        "user,id=n1,net=10.0.3.0/24,guestfwd=tcp:10.0.3.100:7-cmd:cat",
        "-device",
        "e1000,netdev=n1,mac=52:54:00:12:34:71",
        "-netdev",
        "user,id=n2,net=10.0.4.0/24,guestfwd=tcp:10.0.4.100:7-cmd:cat",
        "-device",
        "e1000e,netdev=n2,mac=52:54:00:12:34:72",
    ],
    script: userspace_e1000,
    disks: &[],
    second: None,
};

/// The cards e1000d drives: each one's interface, what the kernel and
/// e1000d call it, its MAC address, how it interrupts, and its network.
const DRIVEN: &[(&str, &str, &str, &str, &str, &str)] = &[
    ("net1", "Intel 82540EM", "82540EM", "52:54:00:12:34:71", "polled", "10.0.3"),
    ("net2", "Intel 82574L", "82574L", "52:54:00:12:34:72", "an MSI", "10.0.4"),
];

/// The line of the log that starts with `start`, from there.
fn line_from<'a>(log: &'a str, start: &str) -> Result<&'a str, String> {
    log.lines()
        .find_map(|line| line.find(start).map(|at| &line[at..]))
        .ok_or_else(|| format!("the kernel never said \"{start}\""))
}

fn userspace_e1000(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    // The cards are found during the PCI scan, among lines boot_report
    // reads past: the Intel ones left, behind the IOMMU, the virtio card
    // kept.
    let log = vm.log();
    let mut addresses = Vec::new();
    for (name, what, ..) in DRIVEN {
        let line = line_from(&log, &format!("Net: {name} is an {what} network card at "))?;
        if !line.ends_with(", left for a driver program (e1000=program)") {
            return Err(format!("the kernel described {name} as \"{line}\""));
        }
        let address = line.split(" at ").nth(1).and_then(|rest| rest.split(',').next());
        let address = address.unwrap_or("?").to_string();
        let confined = format!("IOMMU: {address} reaches only what is mapped");
        if !log.contains(confined.as_str()) {
            return Err(format!("the kernel never said \"{confined}\""));
        }
        addresses.push(address);
    }
    line_from(&log, "Net: net0 is a virtio network card at ")?;
    vm.expect("spark: 2 services in /system/tests/services/e1000d, 2 enabled")?;
    // e1000d, netd and nics run at once: their lines are read ahead.
    for ((name, _, model, mac, how, net), address) in DRIVEN.iter().zip(&addresses) {
        vm.expect_ahead(&format!("Device: {address} is open for spark"))?;
        vm.expect_ahead(&format!("e1000d: {name} is the {model} at {address}, MAC {mac}, {how}"))?;
        vm.expect_ahead(&format!("Net: {name} is attached to its driver, e1000d, MAC {mac}"))?;
        vm.expect_ahead(&format!(
            "netd: {name} has {net}.15/24 from {net}.2 for 1 day, router {net}.2, DNS {net}.3"
        ))?;
    }
    vm.expect_ahead(
        "netd: net0 has 10.0.2.15/24 from 10.0.2.2 for 1 day, router 10.0.2.2, DNS 10.0.2.3",
    )?;
    // Halfway through net1's echo: the card stops before its memory goes,
    // both its interfaces go down, and Spark starts e1000d again, which
    // brings them back. nics says it killed e1000d once kill() returns,
    // and through Spark, by when the kernel may have said the cards went
    // down: those lines are read ahead from before nics' own, as only the
    // kill makes them.
    vm.expect_ahead("nics: killed e1000d (process ")?;
    for ((name, ..), address) in DRIVEN.iter().zip(&addresses) {
        vm.expect_ahead(&format!("Net: {name} is down: its driver let go of it"))?;
        vm.expect_ahead(&format!("Device: {address} stopped, as its driver let go of it"))?;
    }
    vm.expect_ahead("spark: e1000d (process ")?;
    vm.expect_ahead(" was ended by signal 9")?;
    // Spark starts e1000d again only once it has ended, and the cards are
    // opened again after that.
    vm.expect("spark: starting e1000d again in ")?;
    for ((name, _, _, mac, ..), address) in DRIVEN.iter().zip(&addresses) {
        vm.expect_ahead(&format!("Device: {address} is open for spark"))?;
        vm.expect_ahead(&format!("Net: {name} is attached to its driver, e1000d, MAC {mac}"))?;
    }
    for (name, net) in [("net1", "10.0.3"), ("net2", "10.0.4"), ("net0", "10.0.2")] {
        vm.expect_ahead(&format!("nics: {name} echoed 2 MiB through {net}.100 port 7 in "))?;
    }
    let code = program_line(vm, "(/system/tests/posix/nics) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> = log.lines().filter(|line| line.contains("nics: FAILED: ")).collect();
    if !failed.is_empty() || !log.contains("nics: every check passed") || code != "0x0" {
        return Err(format!("the userspace-e1000 test failed ({code}): {}", failed.join("; ")));
    }
    // The card was stopped once, by the kill, and e1000d never broke
    // its link's rules.
    for address in &addresses {
        let stops = format!("Device: {address} stopped");
        if log.matches(stops.as_str()).count() != 1 {
            return Err(format!("\"{stops}\" is not in the log once"));
        }
    }
    if log.contains("the link's rules") {
        return Err("a side of a link broke its rules".into());
    }
    Ok(())
}
