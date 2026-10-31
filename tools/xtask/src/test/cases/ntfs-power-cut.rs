// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The power is cut while _/system/tests/storage churn_ writes, syncs,
//! renames and removes files on a 64 MiB virtio disk that *mkntfs* made,
//! of 4 KiB clusters, 0 to 1.5 seconds after its thirtieth file is
//! synced. The volume must be left marked dirty, for Windows to check,
//! and The Sleuth Kit (*fls* and *icat*) must find every file synced
//! whole. On the next boot Radix must mount it read-only and say why,
//! _/system/tests/storage churned-kept_ must read each file whole or a
//! start of itself and have its write refused, and the disk must not
//! change (ADR 0238). ntfs-3g and The Sleuth Kit must be installed (*ob
//! doctor* says how).

use super::super::*;

pub const CASE: Case = Case {
    name: "ntfs-power-cut",
    about: "The power is cut while files are written, synced, renamed and removed on NTFS: the volume is left marked for Windows to check, The Sleuth Kit finds every synced file whole, and Radix then mounts it read-only, reads the files and changes nothing",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep -- /system/tests/storage churn",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: power_cut,
    disks: NTFS_CUT_DISKS,
    second: Some(SecondBoot {
        cmdline: "lockdep -- /system/tests/storage churned-kept",
        before: ntfs_cut_host,
        script: ntfs_cut_kept,
    }),
};

/// The `ntfs-power-cut` test's disk: a virtio disk of an empty NTFS
/// that `mkntfs` made, of 4 KiB clusters, which only the churn fills.
const NTFS_CUT_DISKS: &[Disk] = &[Disk {
    kib: 64 * 1024,
    fat: NTFS_EMPTY,
    cluster: 4096,
    label: "RADIXNTFS",
    table: Table::None,
    bus: Bus::Virtio,
    sector: 512,
    device: "",
    errors: None,
    read_only: false,
    check: ntfs_cut,
}];

/// The flags `$Volume` has on the NTFS of `disk`, in `$MFT`'s copy of it,
/// as The Sleuth Kit's `icat` reads its `$VOLUME_INFORMATION` (type 112).
/// ntfs-3g's tools refuse a volume whose `$MFTMirr` differs, as a power
/// cut between the two copies' writes leaves it, until Windows mends it.
fn ntfs_flags(disk: &DiskImage) -> Result<u16, String> {
    let image = disk.path.display();
    let mut icat = std::process::Command::new("icat");
    let info = host_tool(icat.args(["-f", "ntfs"]).arg(&disk.path).arg("3-112"))
        .map_err(|err| format!("{image}: icat: {err}"))?;
    match info.get(10..12) {
        Some(flags) => Ok(u16::from_le_bytes([flags[0], flags[1]])),
        None => Err(format!("{image}: $VOLUME_INFORMATION is {} bytes", info.len())),
    }
}

/// The files of the NTFS of `disk` copied out to a directory, as The
/// Sleuth Kit reads them (`fls` lists them and `icat` copies each), which
/// reads a volume whatever its flags say.
fn ntfs_restore(disk: &DiskImage) -> Result<PathBuf, String> {
    let to = disk.path.with_extension("restored");
    if to.exists() {
        std::fs::remove_dir_all(&to).map_err(|err| format!("removing {}: {err}", to.display()))?;
    }
    std::fs::create_dir(&to).map_err(|err| format!("making {}: {err}", to.display()))?;
    let mut fls = std::process::Command::new("fls");
    fls.args(["-u", "-F", "-f", "ntfs"]).arg(&disk.path);
    let listing = String::from_utf8_lossy(&host_tool(&mut fls)?).into_owned();
    for line in listing.lines() {
        // `r/r 64-128-2:\tfile-0003`, the root's files only.
        let Some((number, name)) = line.split_once(":\t") else { continue };
        let Some(number) = number.rsplit(' ').next() else { continue };
        if name.starts_with('$') || name.contains('/') {
            continue;
        }
        let mut icat = std::process::Command::new("icat");
        let data = host_tool(icat.args(["-f", "ntfs"]).arg(&disk.path).arg(number))?;
        std::fs::write(to.join(name), data).map_err(|err| format!("writing {name}: {err}"))?;
    }
    Ok(to)
}

/// Checks the `ntfs-power-cut` test's disk after a boot. After the one
/// whose power was cut, the volume is marked dirty, for Windows to check,
/// as Radix leaves it while it writes (ADR 0237), and The Sleuth Kit finds
/// each file the program synced whole, as [`btrfs_cut`] checks Btrfs.
/// After the next, which only reads it, it is as it was.
fn ntfs_cut(disk: &DiskImage, log: &str) -> Result<(), String> {
    let synced = churn_rounds(log, "synced");
    let Some(&last) = synced.last() else { return btrfs_unchanged(disk, log) };
    let image = disk.path.display();
    if ntfs_flags(disk)? & 1 == 0 {
        return Err(format!("{image} is not marked dirty after a power cut"));
    }
    let removed = churn_rounds(log, "removed");
    let restored = ntfs_restore(disk)?;
    let result = churn_left(&restored, &synced, &removed, last.checked_sub(CHURN_KEEP));
    let _ = std::fs::remove_dir_all(&restored);
    result
        .map_err(|err| format!("{image}: {err}, after a power cut after file-{last:04} was synced"))
}

/// Between the boots of the `ntfs-power-cut` test: the disk's sum, which
/// the boot that only reads it must leave as it is.
fn ntfs_cut_host(_: &Case, disks: &[DiskImage]) -> Result<(), String> {
    for disk in disks {
        let sum = host_tool(std::process::Command::new("sha256sum").arg(&disk.path))?;
        let sums = disk.path.with_extension("sha256");
        std::fs::write(&sums, sum).map_err(|err| format!("writing {}: {err}", sums.display()))?;
    }
    Ok(())
}

/// The `ntfs-power-cut` test's second boot: Radix mounts the volume left
/// dirty read-only and says why; the program finds the churn's files
/// whole or a start of themselves, and its write is refused.
fn ntfs_cut_kept(vm: &mut Vm, case: &Case) -> Result<(), String> {
    // Mounted read-only, the kernel saying why first.
    disks_found(vm, case)?;
    for (disk, name) in case.disks.iter().zip(volume_names(case.disks)) {
        let label = disk.label;
        vm.expect(&format!("Files: the NTFS of {name} is only read: it is marked to be checked"))?;
        vm.expect(&format!(
            "Files: mounted ntfs from {name} on /volumes/{name}, read-only, labelled {label}"
        ))?;
        vm.expect(&format!(
            "spark: mounted the NTFS filesystem of {name} at /volumes/{name}, read-only"
        ))?;
    }
    vm.expect("spark: built the hierarchy of 9 directories and mounted 9 filesystems")?;
    vm.expect_ahead("spark: started /system/tests/storage as process ")?;
    for name in volume_names(case.disks) {
        vm.expect(&format!("storage: /volumes/{name}: after the power cut, "))?;
        vm.expect(&format!(
            "storage: /volumes/{name}: only read after the power cut, for Windows to check it"
        ))?;
    }
    vm.expect("storage: /processes lists ")?;
    storage_end(vm, "churned-kept")
}
