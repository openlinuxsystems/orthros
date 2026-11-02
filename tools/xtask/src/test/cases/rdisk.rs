// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A blank virtio disk is partitioned at the console. *rdisk* gives it a
//! GPT at its prompt, with an EFI system partition and a second one; the
//! kernel must read the table again and add both, and volumed must leave
//! them unmounted, as they were made, not plugged in. *mkswap* and
//! *swapon* use the second, and *rdisk --script* must then refuse a layout
//! that takes it away, but add a third partition with one that keeps it,
//! the kernel reading the table again while the swap is on. *mkfs -t
//! fat*, *mkfs.exfat* and *fsck.fat* make and check filesystems on the
//! first and third, and */devices/disk/by-label* must lead to them. On the
//! host, *sgdisk* must find the table sound, and *fsck.fat* and
//! *fsck.exfat* the filesystems (ADRs 0300 to 0303).

use std::process::Command;

use super::super::*;

pub const CASE: Case = Case {
    name: "rdisk",
    about: "A blank disk partitioned at the console: rdisk writes a GPT at its prompt and with a layout file, the kernel reads it again each time, keeping a partition in use as swap and refusing a layout that takes it away; mkfs makes FAT and exFAT on the partitions, fsck.fat checks one, /devices/disk/by-label leads to them, and the host's sgdisk, fsck.fat and fsck.exfat find all sound",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "console-sign-in=off",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: rdisk,
    disks: RDISK_DISKS,
    second: None,
};

/// The blank disk: 96 MiB of bytes that hold no table or filesystem.
const RDISK_DISKS: &[Disk] = &[Disk {
    kib: 96 * 1024,
    fat: 0,
    cluster: 0,
    label: "",
    table: Table::None,
    bus: Bus::Virtio,
    sector: 512,
    device: "",
    errors: None,
    read_only: false,
    check: partitioned,
}];

/// What is typed at rdisk's prompt: a new GPT; partition 1, 16 MiB, an
/// EFI system partition; partition 2, 16 MiB, Linux swap; then written.
const TYPED: &str = "g\\nn\\n\\n\\n+16M\\nt\\nefi\\nn\\n\\n\\n+16M\\nt\\n2\\nswap\\nw\\n";

/// A layout that keeps the table and takes partition 2 out.
const TAKES_SWAP: &str = r#"{"table": "gpt", "keep": true, "delete": [2]}"#;

/// A layout that keeps the table and adds partition 3, named Data, in
/// the rest of the disk.
const ADDS_DATA: &str =
    r#"{"table": "gpt", "keep": true, "partitions": [{"type": "linux", "size": "rest", "name": "Data"}]}"#;

/// The `rdisk` test.
fn rdisk(vm: &mut Vm, _case: &Case) -> Result<(), String> {
    vm.expect("Disk: vda has no partition table; it is used whole")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;

    vm.type_text("rdisk -l vda | tail -1\n")?;
    vm.expect_screen(&["Partition table: none"])?;
    vm.type_text(&format!("printf '{TYPED}' | rdisk vda | tail -2\n"))?;
    vm.expect_screen(&["The partition table is written.", "The kernel has read it"])?;
    vm.expect("Disk: vda1: 16 MiB from sector 2048, GPT")?;
    vm.expect("Disk: vda2: 16 MiB from sector 34816, GPT")?;
    vm.expect("Disk: vda's partition table read again: a GPT with 2 partitions, 2 new, 0 gone")?;
    // The EFI system partition volumed leaves alone; the other it leaves
    // unmounted, as it was made, not plugged in.
    vm.expect("volumed: vda2 is new on vda, which was partitioned again; left unmounted")?;

    vm.type_text("mkswap -q /devices/vda2 && swapon /devices/vda2 && echo \"on: $((40 + 2))\"\n")?;
    vm.expect_screen(&["on: 42"])?;
    vm.type_text(&format!("echo '{TAKES_SWAP}' > /tmp/takes.json\n"))?;
    vm.type_text("rdisk --script /tmp/takes.json vda; echo \"status: $?\"\n")?;
    vm.expect_screen(&["vda2 is swap that is on", "status: 1"])?;
    vm.type_text(&format!("echo '{ADDS_DATA}' > /tmp/adds.json\n"))?;
    vm.type_text("rdisk --script /tmp/adds.json vda | grep kernel\n")?;
    vm.expect_screen(&["The kernel has read it"])?;
    vm.expect("Disk: vda3: 62.9 MiB from sector 67584, GPT \"Data\"")?;
    vm.expect("Disk: vda's partition table read again: a GPT with 3 partitions, 1 new, 0 gone")?;
    vm.type_text("swapoff /devices/vda2 && echo \"off: $((40 + 3))\"\n")?;
    vm.expect_screen(&["off: 43"])?;

    vm.type_text("mkfs -t fat -n EFI vda1 && mkfs.exfat -L DATA /devices/vda3 && echo \"made: $((40 + 4))\"\n")?;
    vm.expect_screen(&["mkfs-fat: made FAT16", "mkfs-exfat: made exFAT", "made: 44"])?;
    vm.type_text("fsck.fat -n vda1; echo \"checked: $?\"\n")?;
    vm.expect_screen(&["/devices/vda1: 0 files", "checked: 0"])?;
    vm.type_text("readlink /devices/disk/by-label/EFI /devices/disk/by-label/DATA\n")?;
    vm.expect_screen(&["/devices/vda1", "/devices/vda3"])?;

    vm.type_text("poweroff\n")?;
    vm.expect("spark: asked to power off the machine")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// The partitions of the disk, as sfdisk reads its table: each one's first
/// sector, size, type GUID and name, from its dump's lines, which read
/// `disk.img1 : start=2048, size=32768, type=C12A..., uuid=..., name="EFI"`.
fn read_table(path: &Path) -> Result<Vec<(u64, u64, String, String)>, String> {
    let text = host_tool(Command::new(fat_tool("sfdisk")).arg("--dump").arg(path))?;
    let text = String::from_utf8_lossy(&text);
    let mut parts = Vec::new();
    for line in text.lines() {
        let Some((_, fields)) = line.split_once(" : ") else { continue };
        let field = |key: &str| {
            fields.split(", ").find_map(|f| f.trim().strip_prefix(key)).map(|v| v.trim().trim_matches('"').to_string())
        };
        let number = |key: &str| field(key).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let kind = field("type=").unwrap_or_default().to_lowercase();
        parts.push((number("start="), number("size="), kind, field("name=").unwrap_or_default().to_lowercase()));
    }
    Ok(parts)
}

/// Checks the disk on the host: its GPT with sgdisk, the partitions with
/// sfdisk, and the filesystems with fsck.fat and fsck.exfat, each copied
/// out of the disk to a file of its own.
fn partitioned(disk: &DiskImage, _log: &str) -> Result<(), String> {
    let said = host_tool(Command::new(fat_tool("sgdisk")).arg("-v").arg(&disk.path))?;
    let said = String::from_utf8_lossy(&said);
    if !said.contains("No problems found") {
        return Err(format!("sgdisk -v found problems in the table rdisk wrote:\n{said}"));
    }
    let parts = read_table(&disk.path)?;
    let expected = [
        (2048, 32768, "c12a7328-f81f-11d2-ba4b-00a0c93ec93b", ""),
        (34816, 32768, "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f", ""),
        (67584, 128_991, "0fc63daf-8483-4772-8e79-3d69d8477de4", "data"),
    ];
    let found: Vec<(u64, u64, &str, &str)> =
        parts.iter().map(|(s, n, t, name)| (*s, *n, t.as_str(), name.as_str())).collect();
    if found != expected {
        return Err(format!("sfdisk reads the partitions as {found:?}, not {expected:?}"));
    }
    let image = std::fs::read(&disk.path).map_err(|err| format!("reading {}: {err}", disk.path.display()))?;
    for (number, tool) in [(1, "fsck.fat"), (3, "fsck.exfat")] {
        let (first, sectors, _, _) = parts[number - 1];
        let part = disk.path.with_extension(format!("part{number}"));
        let bytes = &image[(first * 512) as usize..((first + sectors) * 512) as usize];
        std::fs::write(&part, bytes).map_err(|err| format!("writing {}: {err}", part.display()))?;
        let checked = host_tool(Command::new(fat_tool(tool)).arg("-n").arg(&part));
        let _ = std::fs::remove_file(&part);
        checked.map_err(|err| format!("{tool} on partition {number}: {err}"))?;
    }
    Ok(())
}
