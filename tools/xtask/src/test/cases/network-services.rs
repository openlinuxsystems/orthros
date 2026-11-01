// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The network services as the system runs them (ADRs 0193 to 0196),
//! against servers on lo that netservices-test runs
//! (userspace/programs/netservices-test): DNS on port 5353 over UDP and
//! TCP, DNS over TLS, DNS over HTTPS and a web page with a test
//! authority's certificate, Network Time Security five seconds ahead of
//! the machine's clock, and a log sink. Spark starts every service, and
//! the program beside them with the network. resolved must look names up
//! through each server and transport as its settings name them, keep
//! answers and negative ones, ask again over TCP for an answer too long
//! for UDP, answer localhost and .local itself, and refuse a certificate
//! for another name or from an authority it does not trust; curl must
//! fetch the page, and refuse it without the authority; timed must step
//! the clock five seconds by NTS, which the kernel logs, then leave it;
//! timedatectl must show it, refuse to set the time while timed runs,
//! and set the time zone, which date(1) then uses; and netlog must send
//! the boot's first records and a new one to the sink.

use super::super::*;

/// The test.
pub const CASE: Case = Case {
    name: "network-services",
    about: "resolved looks names up over plain DNS, TLS and HTTPS, caches them and refuses wrong certificates; curl fetches over HTTPS; timed sets the clock by NTS; timedatectl sets the time zone; netlog sends the log: each against servers on lo (ADRs 0193-0196)",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    // Every service, as on a machine, and the test program beside them.
    cmdline: "lockdep -- --services --posix --network /system/tests/posix/netservices-test",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: network_services,
    disks: &[],
    second: None,
};

/// The lines netservices-test prints for its parts, in order.
const PARTS: &[&str] = &[
    "netservices-test: plain DNS: ",
    "netservices-test: DNS over TLS: ",
    "netservices-test: DNS over HTTPS: ",
    "netservices-test: opportunistic DNS over TLS: ",
    "netservices-test: curl: ",
    "netservices-test: timed: ",
    "netservices-test: time zones: ",
    "netservices-test: netlog: ",
];

fn network_services(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_ahead("resolved: answering on 127.0.0.53:53")?;
    vm.expect_ahead("spark: started /system/tests/posix/netservices-test as process ")?;
    for part in PARTS {
        vm.expect_ahead(part)?;
    }
    let code = program_line(vm, "(/system/tests/posix/netservices-test) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> =
        log.lines().filter(|line| line.contains("netservices-test: FAILED")).map(str::trim).collect();
    if !failed.is_empty() || !log.contains("netservices-test: every check passed") || code != "0x0" {
        return Err(format!("the network services test failed ({code}): {}", failed.join("; ")));
    }
    // The kernel logged timed's step of the clock, five seconds on.
    let stepped = log
        .lines()
        .find(|line| line.contains("Time: timed set the clock to "))
        .ok_or("the kernel never said that timed set the clock")?;
    if !stepped.contains(" UTC (+5") && !stepped.contains(" UTC (+49") {
        return Err(format!("timed did not step the clock five seconds: {stepped}"));
    }
    Ok(())
}
