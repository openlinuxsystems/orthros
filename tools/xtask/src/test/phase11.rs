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
        about: "Spark mounts two NTFS disks, one mkntfs made with 4 KiB sectors and clusters and ntfscp wrote, and the one ntfs-3g filled with 512-byte clusters: every file, link, junction, hard link, compressed and sparse file reads as the host wrote it, an offline file fails, and neither disk changes",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage list --no-write",
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
    Case {
        name: "ntfs-write",
        about: "The storage test on two NTFS disks mkntfs made and ntfscp wrote files to, an NVMe namespace of 4 KiB sectors and clusters and a virtio disk of 512-byte ones: after each boot ntfs-3g's ntfsresize finds every cluster accounted for, its ntfsfix the mirror matching and the volume clean; The Sleuth Kit reads back what was written, and a file ntfscp writes between the boots reads back",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage write",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: storage_write,
        disks: NTFS_WRITE_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage check",
            before: storage_host,
            script: storage_check,
        }),
    },
];

/// An NTFS test disk on `bus` of `kib` KiB holding `fat` ([`NTFS`] or
/// [`NTFS_3G`]), labelled `label`, with sectors of `sector` bytes and
/// clusters of `cluster`, which the host checks with `check`.
const fn ntfs_disk(
    kib: u64,
    fat: u8,
    label: &'static str,
    bus: Bus,
    sector: u64,
    cluster: u32,
    check: DiskCheck,
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
        check,
    }
}

/// The `ntfs` test's disks: an NVMe namespace of 4 KiB sectors with an
/// NTFS of 4 KiB clusters that `mkntfs` made and `ntfscp` wrote
/// [`ntfs_files`] to, and a virtio disk of the volume of 512-byte clusters
/// ntfs-3g filled through FUSE ([`ntfs_3g_files`]), with links, a
/// junction, hard links, and compressed and sparse files. Spark mounts
/// both to write (ADR 0237), and reading them must change neither.
const NTFS_DISKS: &[Disk] = &[
    ntfs_disk(64 * 1024, NTFS, "RADIXNTFS", Bus::Nvme(0), 4096, 4096, btrfs_unchanged),
    ntfs_disk(8 * 1024, NTFS_3G, "NTFS3G", Bus::Virtio, 512, 512, btrfs_unchanged),
];

/// The `ntfs-write` test's disks, as `mkntfs` makes them and `ntfscp`
/// writes [`ntfs_files`] to, which the storage program writes over: an
/// NVMe namespace of 4 KiB sectors and clusters, whose records are 4 KiB
/// too, and a virtio disk of 512-byte sectors and clusters, smaller than
/// a record.
const NTFS_WRITE_DISKS: &[Disk] = &[
    ntfs_disk(64 * 1024, NTFS, "RADIXNTFS", Bus::Nvme(0), 4096, 4096, ntfs_checked),
    ntfs_disk(32 * 1024, NTFS, "RADIXSMALL", Bus::Virtio, 512, 512, ntfs_checked),
];

/// Checks an NTFS disk with ntfs-3g, changing nothing: `ntfsresize
/// --info` finds each cluster in use held by one file and each file's
/// clusters in use, `ntfsfix -n` finds `$MFTMirr` matching `$MFT`, and
/// the volume is not left marked dirty, since the program syncs what it
/// wrote (ADR 0237).
fn ntfs_checked(disk: &DiskImage, _log: &str) -> Result<(), String> {
    let image = disk.path.display();
    let mut resize = std::process::Command::new(fat_tool("ntfsresize"));
    let info = host_tool(resize.env("LC_ALL", "C").args(["-i", "-f", "-n"]).arg(&disk.path))
        .map_err(|err| format!("{image} fails ntfs-3g's ntfsresize --info: {err}"))?;
    if !String::from_utf8_lossy(&info).contains("Accounting clusters") {
        return Err(format!("{image}: ntfsresize did not count its clusters"));
    }
    let fix = host_tool(std::process::Command::new(fat_tool("ntfsfix")).arg("-n").arg(&disk.path))
        .map_err(|err| format!("{image} fails ntfs-3g's ntfsfix -n: {err}"))?;
    if !String::from_utf8_lossy(&fix).contains("Mounting volume... OK") {
        return Err(format!("{image}: ntfsfix finds $MFTMirr unlike $MFT"));
    }
    let info =
        host_tool(std::process::Command::new(fat_tool("ntfsinfo")).arg("-m").arg(&disk.path))
            .map_err(|err| format!("{image}: ntfsinfo: {err}"))?;
    let info = String::from_utf8_lossy(&info);
    let flags = info
        .lines()
        .find_map(|line| line.trim().strip_prefix("Volume Flags:"))
        .and_then(|flags| u16::from_str_radix(flags.trim().trim_start_matches("0x"), 16).ok())
        .ok_or_else(|| format!("{image}: ntfsinfo gives no volume flags"))?;
    // `$Volume`'s dirty flag.
    if flags & 1 != 0 {
        return Err(format!("{image} was left marked dirty"));
    }
    Ok(())
}

/// [`storage_host`] for an NTFS disk: its files as The Sleuth Kit, an
/// NTFS reader of its own, reads them (`fls` lists them and `icat`
/// copies each out), none of those the program renamed or removed among
/// them; then the host writes its own file there with ntfs-3g's `ntfscp`,
/// which refuses a volume left dirty.
pub(super) fn ntfs_written(disk: &DiskImage, name: &str, source: &Path) -> Result<(), String> {
    let image = disk.path.display().to_string();
    let mut fls = std::process::Command::new("fls");
    fls.args(["-r", "-p", "-u", "-F", "-f", "ntfs"]).arg(&disk.path);
    let listing = String::from_utf8_lossy(&host_tool(&mut fls)?).into_owned();
    // `r/r 64-128-2:\tnotes/final.txt`: the file's attribute, then its
    // path; a file in no directory would be under `$OrphanFiles`.
    let mut files = BTreeMap::new();
    for line in listing.lines() {
        let Some((number, path)) = line.split_once(":\t") else { continue };
        if path.starts_with("$OrphanFiles/") {
            return Err(format!("{image}: {path} is in no directory"));
        }
        if let Some(number) = number.rsplit(' ').next() {
            files.insert(path.to_string(), number.to_string());
        }
    }
    let read = |path: &str| {
        let number =
            files.get(path).ok_or_else(|| format!("{image}: The Sleuth Kit finds no {path}"))?;
        host_tool(
            std::process::Command::new("icat").args(["-f", "ntfs"]).arg(&disk.path).arg(number),
        )
    };
    let many = files
        .keys()
        .filter(|path| {
            path.strip_prefix("many/")
                .is_some_and(|name| Path::new(name).extension() == Some("txt".as_ref()))
        })
        .count();
    storage_written(&image, name, &read, many)?;
    for gone in ["draft.txt", "scratch.txt"] {
        if files.contains_key(gone) {
            return Err(format!("{image}: {gone} is still there"));
        }
    }
    let mut ntfscp = std::process::Command::new(fat_tool("ntfscp"));
    ntfscp.env("LC_ALL", "C.UTF-8").arg("-q").arg(&disk.path).arg(source);
    host_tool(ntfscp.arg(format!("/{STORAGE_FROM_HOST_NAME}"))).map(drop)
}

/// The `ntfs` test: Spark mounts each disk, and the storage program lists
/// every volume as
/// [`listing`] says of [`ntfs_files`] and [`ntfs_3g_files`]: the offline
/// file fails with `invalid_argument`, as what a filesystem does not do
/// does.
fn ntfs_list(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    let volumes = volume_names(case.disks);
    for (disk, name) in case.disks.iter().zip(&volumes) {
        let label = disk.label;
        vm.expect(&format!(
            "Files: mounted ntfs from {name} on /volumes/{name}, labelled {label}"
        ))?;
        vm.expect(&format!("spark: mounted the NTFS filesystem of {name} at /volumes/{name}"))?;
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
        vm.expect(&format!("storage: list {name}: nothing written"))?;
    }
    storage_end(vm, "list")
}
