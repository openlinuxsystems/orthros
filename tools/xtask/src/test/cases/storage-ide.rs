// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The storage test on four disks on the two channels of QEMU's PIIX3 IDE
//! controller, the last solid state and partitioned with a GPT: the kernel
//! names each disk with its channel and end, and the files written in the
//! first boot are checked in the second (ADR 0244). With 5 GiB of memory,
//! page caches lie above 4 GiB, and their requests go through the bounce
//! buffer.

use super::super::*;

pub const CASE: Case = Case {
    name: "storage-ide",
    about: "The storage test on four disks on the two channels of an IDE controller, one of them solid state and partitioned with a GPT",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep -- /system/tests/storage write",
    cpus: 2,
    // With more than 4 GiB, page caches go above it, and their requests
    // through the bounce buffer.
    memory: "5G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: storage_write,
    disks: DISKS,
    second: Some(SecondBoot {
        cmdline: "lockdep -- /system/tests/storage check",
        before: storage_host,
        script: storage_check,
    }),
};

/// The storage test's disks on an IDE controller ([`Bus::Ide`]): the
/// primary channel's master and slave, then the secondary's, the last one
/// solid state and its partition in a GPT.
const DISKS: &[Disk] = &[
    fat_disk(1440, 12, "RADIX12", Table::None, Bus::Ide),
    fat_disk(32 * 1024, 16, "RADIX16", Table::None, Bus::Ide),
    fat_disk(64 * 1024, 32, "RADIX32", Table::None, Bus::Ide),
    Disk {
        device: ",rotation_rate=1",
        ..fat_disk(48 * 1024, 32, "RADIXPART", Table::Gpt, Bus::Ide)
    },
];
