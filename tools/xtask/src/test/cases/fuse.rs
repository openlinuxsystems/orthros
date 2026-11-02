// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A filesystem a program serves (ADR 0226): fuse-test
//! (userspace/programs/fuse-test) serves one in memory with fuser, as a
//! program written for Linux's FUSE does, on a connection to
//! /devices/fuse that it mounts with radix-directory(2)'s mount_fuse. It
//! makes, writes, reads, renames and removes files and directories on it,
//! with links and a 3 MiB file; a root stamped for another user (Spark
//! gives it the names capability, `--names`) is refused a private file,
//! by the owners and modes the program gives, and what it makes is that
//! user's; a set-ID bit is never shown
//! and a device node opens nothing; unmounting ends the program's loop;
//! and a mount whose program has gone fails its calls as not connected.

use super::super::*;

/// The test.
pub const CASE: Case = Case {
    name: "fuse",
    about: "a filesystem fuse-test serves in memory with fuser through /devices/fuse: files, directories, links and a large file; owners and modes checked, no set-ID bits, no devices; unmounted, and lost with its program (ADR 0226)",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep -- --posix --names /system/tests/posix/fuse-test",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: fuse,
    disks: &[],
    second: None,
};

/// The lines fuse-test prints for its parts, in order.
const PARTS: &[&str] = &[
    "fuse-test: mount: ",
    "fuse-test: files: ",
    "fuse-test: large: ",
    "fuse-test: directories: ",
    "fuse-test: links: ",
    "fuse-test: permissions: ",
    "fuse-test: nosuid: ",
    "fuse-test: nodev: ",
    "fuse-test: unmount: ",
    "fuse-test: lost: ",
];

fn fuse(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_ahead("spark: started /system/tests/posix/fuse-test as process ")?;
    for part in PARTS {
        vm.expect_ahead(part)?;
    }
    let code = program_line(vm, "(/system/tests/posix/fuse-test) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> = log.lines().filter(|line| line.contains("fuse-test: FAILED")).map(str::trim).collect();
    if !failed.is_empty() || !log.contains("fuse-test: every check passed") || code != "0x0" {
        return Err(format!("the fuse test failed ({code}): {}", failed.join("; ")));
    }
    // The kernel's line for the mount, in its log.
    if !log.contains("Files: mounted fuse from fuse-test on /tmp/fuse-test") {
        return Err("the kernel did not log the mount".to_string());
    }
    Ok(())
}
