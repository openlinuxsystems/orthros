// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Phase 11's boot tests (hardware support), added after this file was made. A new
//! test of the phase goes here, with its script, so that the phases built
//! in parallel add theirs to different files. The test still needs its
//! entry in ob(1)'s list (in Phase 11's part of it).

#[allow(unused_imports)]
use super::*;

/// Phase 11's boot tests, run after those in `super::CASES`.
pub const CASES: &[Case] = &[
    Case {
        name: "ntfs",
        about: "Spark mounts two NTFS disks read-only, one mkntfs made with 4 KiB sectors and clusters and ntfscp wrote, and the one ntfs-3g filled with 512-byte clusters: every file, link, junction, hard link, compressed and sparse file reads as the host wrote it, and an offline file fails",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage list",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: ntfs_list,
        disks: NTFS_DISKS,
        second: None,
    },
];

/// An NTFS test disk on `bus` of `kib` KiB holding `fat` ([`NTFS`] or
/// [`NTFS_3G`]), labelled `label`, with sectors of `sector` bytes and
/// clusters of `cluster`; the host checks that it is as it made it.
const fn ntfs_disk(
    kib: u64,
    fat: u8,
    label: &'static str,
    bus: Bus,
    sector: u64,
    cluster: u32,
) -> Disk {
    Disk {
        kib,
        fat,
        cluster,
        label,
        table: Table::None,
        bus,
        sector,
        device: "",
        errors: None,
        read_only: false,
        check: btrfs_unchanged,
    }
}

/// The `ntfs` test's disks: an NVMe namespace of 4 KiB sectors with an
/// NTFS of 4 KiB clusters that `mkntfs` made and `ntfscp` wrote
/// [`ntfs_files`] to, and a virtio disk of the volume of 512-byte clusters
/// ntfs-3g filled through FUSE ([`ntfs_3g_files`]), with links, a
/// junction, hard links, and compressed and sparse files. Spark mounts
/// both read-only, as Radix does not write NTFS yet.
const NTFS_DISKS: &[Disk] = &[
    ntfs_disk(64 * 1024, NTFS, "RADIXNTFS", Bus::Nvme(0), 4096, 4096),
    ntfs_disk(8 * 1024, NTFS_3G, "NTFS3G", Bus::Virtio, 512, 512),
];

/// The `ntfs` test: Spark mounts each disk read-only, as Radix does not
/// write NTFS yet, and the storage program lists every volume as
/// [`listing`] says of [`ntfs_files`] and [`ntfs_3g_files`]: the offline
/// file fails with `invalid_argument`, as what a filesystem does not do
/// does.
fn ntfs_list(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    let volumes = volume_names(case.disks);
    for (disk, name) in case.disks.iter().zip(&volumes) {
        let label = disk.label;
        vm.expect(&format!(
            "Files: mounted ntfs from {name} on /volumes/{name}, read-only, labelled {label}"
        ))?;
        vm.expect(&format!(
            "spark: mounted the NTFS filesystem of {name} at /volumes/{name}, read-only"
        ))?;
    }
    vm.expect("spark: built the hierarchy of 9 directories and mounted 9 filesystems")?;
    vm.expect_ahead("spark: started /system/tests/storage as process ")?;
    // The program lists the volumes in name order.
    let mut sorted: Vec<(&Disk, &String)> = case.disks.iter().zip(&volumes).collect();
    sorted.sort_by(|a, b| a.1.cmp(b.1));
    for (disk, name) in sorted {
        let lines = if disk.fat == NTFS_3G {
            listing(&ntfs_3g_files(), name, &[(NTFS_OFFLINE, "invalid_argument")])
        } else {
            listing(&ntfs_files(), name, &[])
        };
        for line in lines {
            vm.expect(&line)?;
        }
    }
    storage_end(vm, "list")
}

