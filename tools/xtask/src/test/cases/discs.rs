// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Spark mounts two disc images on virtio disks, read-only: an ISO 9660
//! one xorriso made with Rock Ridge and Joliet, and a UDF one with the
//! ISO 9660 bridge a DVD has, which genisoimage made and Spark mounts as
//! UDF. The storage program lists every file, directory and link of both
//! as the host made them, a write to either fails with `read_only`, and
//! neither disk changes (ADR 0245).

use super::super::*;

pub const CASE: Case = Case {
    name: "discs",
    about: "Spark mounts an ISO 9660 disc image and a UDF one read-only: every file, directory and link reads as the host made it, writes fail, and neither disk changes",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep -- /system/tests/storage list",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: discs,
    disks: DISKS,
    second: None,
};

/// An ISO 9660 disc and a UDF one, on virtio disks of 512-byte sectors,
/// as images written to a disk or USB stick are.
const DISKS: &[Disk] = &[disc(ISO9660, "RADIXISO"), disc(UDF, "RADIXUDF")];

const fn disc(fat: u8, label: &'static str) -> Disk {
    Disk {
        kib: 4096,
        fat,
        cluster: 0,
        label,
        table: Table::None,
        bus: Bus::Virtio,
        sector: 512,
        device: "",
        errors: None,
        read_only: false,
        check: btrfs_unchanged,
    }
}

/// The disks found, both mounted read-only, and each one's list.
fn discs(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    let volumes = volume_names(case.disks);
    for (disk, name) in case.disks.iter().zip(&volumes) {
        let (kind, said) = disk.kind();
        let label = disk.label;
        vm.expect(&format!(
            "Files: mounted {kind} from {name} on /volumes/{name}, read-only, labelled {label}"
        ))?;
        vm.expect(&format!(
            "spark: mounted the {said} filesystem of {name} at /volumes/{name}, read-only"
        ))?;
    }
    vm.expect("spark: built the hierarchy of 9 directories and mounted 9 filesystems")?;
    vm.expect_ahead("spark: started /system/tests/storage as process ")?;
    // The program lists the volumes in name order.
    let mut sorted: Vec<(&Disk, &String)> = case.disks.iter().zip(&volumes).collect();
    sorted.sort_by(|a, b| a.1.cmp(b.1));
    for (disk, name) in sorted {
        for line in listing(&disc_files(disk.fat), name, &[]) {
            vm.expect(&line)?;
        }
        vm.expect(&format!("storage: list {name}: writes fail with read_only"))?;
    }
    storage_end(vm, "list")
}
