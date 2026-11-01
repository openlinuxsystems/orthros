// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The storage test on four SD cards, each in an SD card reader of its own
//! (QEMU's `sdhci-pci`, a version 2.00 controller with 32-bit
//! descriptors), the last partitioned with a GPT: the kernel names each
//! card `mmcblk0` and on and says what it is, and the files written in the
//! first boot are checked in the second (ADR 0247). With 5 GiB of memory,
//! page caches lie above 4 GiB, and their requests go through the bounce
//! buffer.

use super::super::*;

pub const CASE: Case = Case {
    name: "storage-sd",
    about: "The storage test on four SD cards in four SD card readers, one partitioned with a GPT",
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

/// The storage test's disks as SD cards ([`Bus::Sd`]), whose sizes QEMU
/// wants powers of two: FAT12, FAT16 and FAT32 on whole cards, and FAT32
/// in a GPT partition.
const DISKS: &[Disk] = &[
    fat_disk(2 * 1024, 12, "RADIX12", Table::None, Bus::Sd),
    fat_disk(32 * 1024, 16, "RADIX16", Table::None, Bus::Sd),
    fat_disk(64 * 1024, 32, "RADIX32", Table::None, Bus::Sd),
    fat_disk(128 * 1024, 32, "RADIXPART", Table::Gpt, Bus::Sd),
];
