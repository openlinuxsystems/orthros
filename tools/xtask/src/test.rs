// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob test`: host unit tests and QEMU boot tests.
//!
//! Each boot test is a [`Case`]: a machine (firmware, media, CPUs, memory),
//! kernel flags, and a script that follows the serial console like a person
//! would, waiting for lines, typing commands to `kdb`, and reading the
//! framebuffer console from a screenshot. All cases boot copies of one test
//! image; only the kernel command line in its grub.cfg differs, written
//! into a slot reserved for it.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::{Firmware, Media};
use crate::image::{self, ImageSpec, crc32};
use crate::process::{self, step};
use crate::qemu::{self, Machine, Serial};
use crate::{
    Args, Ctx, Error, Result, VERSION, btrfs, corpus, doctor, fixture, kernel, screen, tla,
    userspace,
};

/// QEMU's exit status after the kernel writes 0x10 to isa-debug-exit.
pub const QEMU_EXIT_SUCCESS: i32 = 33;
/// QEMU's exit status after the kernel writes 0x11 (a panic).
const QEMU_EXIT_PANIC: i32 = 35;
/// QEMU's exit status when the guest restarts under `-no-reboot`.
const QEMU_EXIT_RESET: i32 = 0;
/// QEMU's exit status when the guest powers the machine off.
const QEMU_EXIT_POWER_OFF: i32 = 0;

/// The SSDT `acpi-power` gives QEMU (`src/acpi/power.asl`), and the
/// argument `run_case` replaces with the file it writes it to.
const POWER_AML: &[u8] = include_bytes!("acpi/power.aml");
const POWER_TABLE: &str = "file=power.aml";
/// The SSDT `acpi-thermal` gives QEMU (`src/acpi/thermal.asl`), likewise.
const THERMAL_AML: &[u8] = include_bytes!("acpi/thermal.aml");
const THERMAL_TABLE: &str = "file=thermal.aml";
/// The SSDT of the `framework-ec` case: a Framework EC's ACPI device.
const CROSEC_AML: &[u8] = include_bytes!("acpi/crosec.aml");
const CROSEC_TABLE: &str = "file=crosec.aml";
/// The SSDT `gpio` gives QEMU (`src/acpi/gpio.asl`), likewise.
const GPIO_AML: &[u8] = include_bytes!("acpi/gpio.aml");
const GPIO_TABLE: &str = "file=gpio.aml";
/// The SMBIOS memory device of the `smbus` case, which QEMU gives in place
/// of its own ([`ddr4_device`]).
const DDR4_DEVICE: &str = "file=ddr4.smbios";
/// The release's name, which the kernel's banner gives after its version
/// (`BANNER` in kernel/radix/src/main.rs) and the console's greeting
/// begins with (userspace/defaults/issue).
const RELEASE: &str = "Dukovo Server";
/// The first line of the kernel's panic report on the serial port
/// (kernel/radix/src/panic.rs).
const PANIC_BANNER: &str = "==================== RADIX KERNEL PANIC";

/// Every library of the kernel workspace with host unit tests.
pub const KERNEL_LIBRARIES: &[&str] = &[
    "radix-abi",
    "radix-acpi",
    "radix-ahci",
    "radix-amdvi",
    "radix-bootfs",
    "radix-block",
    "radix-bootinfo",
    "radix-btrfs",
    "radix-cap",
    "radix-crosec",
    "radix-elf",
    "radix-exfat",
    "radix-ext4",
    "radix-fat",
    "radix-fbcon",
    "radix-gpio",
    "radix-hal",
    "radix-hid",
    "radix-hw",
    "radix-input",
    "radix-iopt",
    "radix-ipc",
    "radix-kdb",
    "radix-klog",
    "radix-lockdep",
    "radix-ksyms",
    "radix-mca",
    "radix-microcode",
    "radix-mm",
    "radix-net",
    "radix-nvme",
    "radix-posix",
    "radix-pstate",
    "radix-random",
    "radix-relocs",
    "radix-sched",
    "radix-scsi",
    "radix-sensors",
    "radix-smbios",
    "radix-smbus",
    "radix-spd",
    "radix-sync",
    "radix-tty",
    "radix-uacpi",
    "radix-usb",
    "radix-vfs",
    "radix-virtio",
    "radix-vtd",
    "radix-watchdog",
    "radix-xhci",
];

/// Length of the command-line slot in the test image's grub.cfg.
const CMDLINE_SLOT: usize = 160;

/// Which test image a case boots.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TestImage {
    /// The kernel alone.
    Plain,
    /// The kernel plus a microcode module of made-up updates.
    Microcode,
}

/// One boot test.
pub struct Case {
    /// Name for `--case` and the log files.
    pub name: &'static str,
    /// What it checks, for the progress output and `ob help test`.
    pub about: &'static str,
    firmware: Firmware,
    media: Media,
    cmdline: &'static str,
    cpus: u32,
    memory: &'static str,
    /// Let the guest restart, for the crash log test.
    reboot: bool,
    image: TestImage,
    /// A CPU model to emulate (without KVM) instead of the host's.
    cpu_model: Option<&'static str>,
    /// More QEMU arguments.
    qemu: &'static [&'static str],
    script: Script,
    /// Disks to attach, made afresh for each run and checked after each
    /// boot.
    disks: &'static [Disk],
    /// A second boot from the same disks, once the first has passed.
    second: Option<SecondBoot>,
}

/// What a case does with its machine: follows the console, and fails with
/// the reason.
type Script = fn(&mut Vm, &Case) -> Result<(), String>;

/// What the host checks on a disk after each boot, given the boot's
/// serial log.
type DiskCheck = fn(&DiskImage, &str) -> Result<(), String>;

/// A disk of a boot test: a FAT filesystem `mkfs.fat` makes, an exFAT one
/// `mkfs.exfat` makes, a Btrfs one `mkfs.btrfs` makes, an ext4 one
/// `mkfs.ext4` makes, or raw bytes.
pub struct Disk {
    /// Its size in KiB.
    kib: u64,
    /// 12, 16 or 32; [`EXFAT`] for exFAT; [`BTRFS`] for Btrfs of
    /// [`btrfs_files`], [`BTRFS_NEW`] for Btrfs to write, or [`BTRFS_LOG`]
    /// for Btrfs Linux left with a log; [`BTRFS_RAID`] for a disk of a
    /// Btrfs RAID1; [`EXT4`] for ext4; [`SWAP`] for a swap area; or 0 for
    /// no filesystem, the disk holding
    /// [`storage_big_byte`]'s bytes.
    fat: u8,
    /// The size of an exFAT disk's clusters, or an ext4 disk's blocks, in
    /// bytes; 0 for the others.
    cluster: u32,
    /// The filesystem's label.
    label: &'static str,
    /// The partition table the filesystem is in, or none for a
    /// filesystem on the whole disk.
    table: Table,
    bus: Bus,
    /// The size of its sectors in bytes, which its filesystem's sectors
    /// have too: 512, or 4096 for a disk of 4 KiB sectors, which QEMU
    /// gives virtio and NVMe disks but not SATA ones.
    sector: u64,
    /// More of QEMU's properties of the disk's device, each after a comma.
    device: &'static str,
    /// Errors for QEMU's blkdebug driver to inject, as its configuration
    /// file.
    errors: Option<fn(usize) -> String>,
    /// Attached read-only (QEMU's `readonly=on`), so that the kernel takes
    /// it for a disk that refuses writes.
    read_only: bool,
    /// What the host checks after each boot: [`fsck`] for a FAT disk.
    check: DiskCheck,
}

/// [`Disk::fat`] for a Btrfs disk.
const BTRFS: u8 = 1;

/// [`Disk::fat`] for a Btrfs disk to write, holding only the host's file
/// ([`STORAGE_FROM_HOST_NAME`]).
const BTRFS_NEW: u8 = 2;

/// [`Disk::fat`] for an exFAT disk.
const EXFAT: u8 = 3;

/// [`Disk::fat`] for an ext4 disk, holding only the host's file
/// ([`STORAGE_FROM_HOST_NAME`]).
const EXT4: u8 = 4;

/// [`Disk::fat`] for a copy of the test image with a Dukovo root
/// partition after its own, as `./ob image` makes one
/// ([`image::append_root`]); [`Disk::kib`] is not used.
const ROOT: u8 = 5;

/// [`Disk::fat`] for the Btrfs disk Linux left with changes in its log
/// of fsyncs alone, as `ob fixture btrfs-log` keeps it (ADR 0151).
const BTRFS_LOG: u8 = 6;

/// [`Disk::fat`] for a swap area the host's `mkswap` makes (ADR 0152).
const SWAP: u8 = 7;

/// [`Disk::fat`] for a disk of the Btrfs RAID1 the host's `mkfs.btrfs`
/// makes across each such disk of a case ([`make_btrfs_raid`], ADR 0153).
const BTRFS_RAID: u8 = 8;

/// The partition table of a test disk with a filesystem.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    /// None: the filesystem is the whole disk.
    None,
    /// An MBR with one FAT32 (LBA) partition from [`PARTITION_START`] to
    /// the disk's end.
    Mbr,
    /// A GPT with one basic data partition, named [`GPT_NAME`], from
    /// [`PARTITION_START`] to the last whole MiB before the backup table
    /// ([`gpt_last`]), as Linux's partitioning tools align it.
    Gpt,
}

/// Where a test disk is attached.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bus {
    /// A virtio block device: `vda`, `vdb` and on.
    Virtio,
    /// A port of an AHCI controller of the case's own, the q35 chipset's
    /// (`ich9-ahci`): `sda`, `sdb` and on.
    Sata,
    /// A namespace of NVMe controller `n` of the case's own, numbered as
    /// they come: `nvme<n>n1`, `nvme<n>n2` and on.
    Nvme(u8),
    /// A USB stick (QEMU's `usb-storage`) on the xHCI controller of a case
    /// that boots from USB, at this QEMU port: a root port (`2`), where it
    /// runs at SuperSpeed, or a port of a hub of the case's own on one
    /// (`4.1`), at the hub's full speed. Named `sd` and a letter after the
    /// SATA disks and the boot stick, in the order the controller finds
    /// them: the root ports in order, then the hubs' ports.
    Usb(&'static str),
    /// Logical unit `n` of a USB disk that speaks USB Attached SCSI
    /// (QEMU's `usb-uas`, with a `scsi-hd` for each unit), at this QEMU
    /// port of a case with [`UAS_PORTS`]: port `1` or `2` at SuperSpeed,
    /// with streams, `3` or `4` at high speed, without. Named as a stick
    /// is, its units in order.
    Uas(&'static str, u8),
}

impl Disk {
    /// Whether it holds Btrfs.
    fn btrfs(&self) -> bool {
        matches!(self.fat, BTRFS | BTRFS_NEW | ROOT | BTRFS_LOG | BTRFS_RAID)
    }

    /// Whether it holds exFAT.
    fn exfat(&self) -> bool {
        self.fat == EXFAT
    }

    /// Whether it holds ext4.
    fn ext4(&self) -> bool {
        self.fat == EXT4
    }

    /// The filesystem's name, as the kernel and Spark give it.
    fn kind(&self) -> (&'static str, &'static str) {
        if self.btrfs() {
            ("btrfs", "Btrfs")
        } else if self.exfat() {
            ("exfat", "exFAT")
        } else if self.ext4() {
            ("ext4", "ext4")
        } else {
            ("fat", "FAT")
        }
    }

    /// Whether the kernel takes it for solid state: an NVMe disk, or a
    /// SATA one with QEMU's rotation rate of 1.
    fn solid(&self) -> bool {
        matches!(self.bus, Bus::Nvme(_)) || self.device.contains("rotation_rate=1")
    }
}

/// The name of partition `number` of disk `disk`: after a `p` when the
/// disk's name ends in a digit, as Linux has it (`sda1`, `nvme0n1p1`).
fn partition_name(disk: &str, number: u32) -> String {
    let p = if disk.ends_with(|c: char| c.is_ascii_digit()) { "p" } else { "" };
    format!("{disk}{p}{number}")
}

/// The QEMU ports of a case's UAS disks, in order: one `usb-uas` each.
fn uas_devices(disks: &[Disk]) -> Vec<&'static str> {
    let mut ports: Vec<&str> = disks
        .iter()
        .filter_map(|disk| match disk.bus {
            Bus::Uas(port, _) => Some(port),
            _ => None,
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// The NVMe controllers a case's disks are on, in order.
fn nvme_controllers(disks: &[Disk]) -> Vec<u8> {
    let mut controllers: Vec<u8> = disks
        .iter()
        .filter_map(|disk| match disk.bus {
            Bus::Nvme(n) => Some(n),
            _ => None,
        })
        .collect();
    controllers.sort_unstable();
    controllers.dedup();
    controllers
}

/// A case's second boot: its command line, what the host does to the
/// disks first, and its script.
pub struct SecondBoot {
    cmdline: &'static str,
    before: fn(&Case, &[DiskImage]) -> Result<(), String>,
    script: Script,
}

/// A disk made for a run: its file, where its filesystem starts, its
/// size and the size of its sectors, what checks it, and for Btrfs the
/// `btrfs` tool that reads it on the host.
pub struct DiskImage {
    path: PathBuf,
    offset: u64,
    bytes: u64,
    sector: u64,
    check: DiskCheck,
    btrfs: Option<PathBuf>,
}

/// The names the kernel gives a case's disks: `vda`, `vdb` and on for
/// virtio ones, `sda`, `sdb` and on for SATA ones, then USB ones after
/// the boot stick's, each in their order, and `nvme<n>n1` and on for
/// each NVMe controller's namespaces.
fn disk_names(disks: &[Disk]) -> Vec<String> {
    let sata_disks = disks.iter().filter(|disk| disk.bus == Bus::Sata).count() as u8;
    let (mut virtio, mut sata, mut usb) = (b'a', b'a', b'a' + sata_disks + 1);
    let mut namespaces = [0u32; 256];
    disks
        .iter()
        .map(|disk| {
            let (prefix, letter) = match disk.bus {
                Bus::Virtio => ("vd", &mut virtio),
                Bus::Sata => ("sd", &mut sata),
                Bus::Usb(_) | Bus::Uas(..) => ("sd", &mut usb),
                Bus::Nvme(n) => {
                    namespaces[usize::from(n)] += 1;
                    return format!("nvme{n}n{}", namespaces[usize::from(n)]);
                }
            };
            *letter += 1;
            format!("{prefix}{}", char::from(*letter - 1))
        })
        .collect()
}

/// Where the kernel says a USB disk at QEMU port `port` is: QEMU puts a
/// SuperSpeed device on its controller's USB 3 root port of that number,
/// 1 to 4, and a full-speed hub on its USB 2 one, 5 to 8.
fn usb_place(port: &str) -> String {
    match port.split_once('.') {
        Some((root, below)) => {
            let root: u8 = root.parse().expect("a root port's number");
            format!("1-{}.{below}", root + 4)
        }
        None => format!("1-{port}"),
    }
}

/// QEMU's `xhci` with [`UAS_PORTS`]: SuperSpeed root ports 1 and 2 of 2,
/// so that QEMU puts a USB 3 device on its ports 3 and 4 at high speed,
/// on the controller's USB 2 root ports 5 and 6.
const UAS_PORTS: &[&str] = &["-global", "qemu-xhci.p3=2"];

/// Where the kernel says the UAS disk at QEMU port `port` and logical
/// unit `lun` of a case with [`UAS_PORTS`] is: its device's place, and
/// the unit if the device has more than one ([`Bus::Uas`]).
fn uas_place(disks: &[Disk], port: &str, lun: u8) -> String {
    let root: u8 = port.parse().expect("a root port's number");
    let place = if root <= 2 { format!("1-{root}") } else { format!("1-{}", root + 2) };
    let units = disks.iter().filter(|disk| matches!(disk.bus, Bus::Uas(p, _) if p == port));
    if units.count() > 1 { format!("{place} LUN {lun}") } else { place }
}

/// The volumes Spark mounts from a case's FAT and Btrfs disks: each
/// disk, or its partition.
fn volume_names(disks: &[Disk]) -> Vec<String> {
    disk_names(disks)
        .into_iter()
        .zip(disks)
        .filter(|(_, disk)| disk.fat != 0)
        .map(|(name, disk)| if disk.table == Table::None { name } else { partition_name(&name, 1) })
        .collect()
}

/// Two NUMA nodes of 512 MiB and two CPUs each, 21 apart (with `-m 1G`).
const TWO_NODES: &[&str] = &[
    "-object",
    "memory-backend-ram,id=m0,size=512M",
    "-object",
    "memory-backend-ram,id=m1,size=512M",
    "-numa",
    "node,nodeid=0,cpus=0-1,memdev=m0",
    "-numa",
    "node,nodeid=1,cpus=2-3,memdev=m1",
    "-numa",
    "dist,src=0,dst=1,val=21",
];

/// A FAT test disk on `bus`.
const fn fat_disk(kib: u64, fat: u8, label: &'static str, table: Table, bus: Bus) -> Disk {
    let check = if matches!(table, Table::Gpt) { fsck_gpt } else { fsck };
    Disk {
        kib,
        fat,
        cluster: 0,
        label,
        table,
        bus,
        sector: 512,
        device: "",
        errors: None,
        read_only: false,
        check,
    }
}

/// The storage test's disks: FAT12, FAT16 and FAT32 on whole disks, and
/// FAT32 in an MBR partition, as virtio disks. Spark mounts them at
/// `/volumes/vda`, `vdb`, `vdc` and `vdd1`.
const STORAGE_DISKS: &[Disk] = &[
    fat_disk(1440, 12, "RADIX12", Table::None, Bus::Virtio),
    fat_disk(32 * 1024, 16, "RADIX16", Table::None, Bus::Virtio),
    fat_disk(64 * 1024, 32, "RADIX32", Table::None, Bus::Virtio),
    fat_disk(48 * 1024, 32, "RADIXPART", Table::Mbr, Bus::Virtio),
];

/// The same disks on SATA, the last one solid state (QEMU's rotation rate
/// of 1) and its partition in a GPT.
const SATA_DISKS: &[Disk] = &[
    fat_disk(1440, 12, "RADIX12", Table::None, Bus::Sata),
    fat_disk(32 * 1024, 16, "RADIX16", Table::None, Bus::Sata),
    fat_disk(64 * 1024, 32, "RADIX32", Table::None, Bus::Sata),
    Disk {
        device: ",rotation_rate=1",
        ..fat_disk(48 * 1024, 32, "RADIXPART", Table::Gpt, Bus::Sata)
    },
];

/// The same disks as USB sticks: two at SuperSpeed on root ports, `sdb`
/// and `sdc` after the boot stick's `sda`, and two at full speed behind
/// a hub ([`USB_HUB`]), `sdd` and `sde1`, the partition in a GPT.
const USB_DISKS: &[Disk] = &[
    fat_disk(1440, 12, "RADIX12", Table::None, Bus::Usb("2")),
    fat_disk(32 * 1024, 16, "RADIX16", Table::None, Bus::Usb("3")),
    fat_disk(64 * 1024, 32, "RADIX32", Table::None, Bus::Usb("4.1")),
    fat_disk(48 * 1024, 32, "RADIXPART", Table::Gpt, Bus::Usb("4.2")),
];

/// The same disks as USB disks that speak UAS, after the boot stick's
/// `sda`: two logical units of one at SuperSpeed (`sdb` and `sdc`), and
/// two at high speed (`sdd` and `sde1`, the partition in a GPT), with
/// [`UAS_PORTS`].
const UAS_DISKS: &[Disk] = &[
    fat_disk(1440, 12, "RADIX12", Table::None, Bus::Uas("2", 0)),
    fat_disk(32 * 1024, 16, "RADIX16", Table::None, Bus::Uas("2", 1)),
    fat_disk(64 * 1024, 32, "RADIX32", Table::None, Bus::Uas("3", 0)),
    fat_disk(48 * 1024, 32, "RADIXPART", Table::Gpt, Bus::Uas("4", 0)),
];

/// The hub [`USB_DISKS`] has two of its sticks behind, at QEMU port 4.
const USB_HUB: &[&str] = &["-device", "usb-hub,bus=xhci.0,port=4"];

/// The stick of the `volumes` test, after the boot stick's `sda`: `sdb`,
/// which volumed(8) mounts by its label once it is plugged in again.
const VOLUME_DISKS: &[Disk] = &[fat_disk(32 * 1024, 16, "DUKOVOUSB", Table::None, Bus::Usb("2"))];

/// What volumetest writes on the stick before it ejects it.
const VOLUME_TEXT: &[u8] = b"written on Dukovo, ejected safely\n";

/// The same disks as NVMe namespaces, two on each of two controllers:
/// `nvme0n1`, `nvme0n2`, `nvme1n1` and `nvme1n2p1`, the partition in a
/// GPT.
const NVME_DISKS: &[Disk] = &[
    fat_disk(1440, 12, "RADIX12", Table::None, Bus::Nvme(0)),
    fat_disk(32 * 1024, 16, "RADIX16", Table::None, Bus::Nvme(0)),
    fat_disk(64 * 1024, 32, "RADIX32", Table::None, Bus::Nvme(1)),
    fat_disk(48 * 1024, 32, "RADIXPART", Table::Gpt, Bus::Nvme(1)),
];

/// The storage test's disks with sectors of 4 KiB, and their filesystems
/// with them: FAT32 on two NVMe namespaces, whole and in a GPT, the
/// smallest a FAT32 volume of such sectors can be, and FAT12 and FAT16 on
/// virtio disks: `nvme0n1`, `nvme0n2p1`, `vda` and `vdb`, in the order
/// the kernel finds them, as the NVMe controller comes first.
const DISKS_4K: &[Disk] = &[
    Disk { sector: 4096, ..fat_disk(288 * 1024, 32, "RADIX32", Table::None, Bus::Nvme(0)) },
    Disk { sector: 4096, ..fat_disk(288 * 1024, 32, "RADIXPART", Table::Gpt, Bus::Nvme(0)) },
    Disk { sector: 4096, ..fat_disk(1440, 12, "RADIX12", Table::None, Bus::Virtio) },
    Disk { sector: 4096, ..fat_disk(32 * 1024, 16, "RADIX16", Table::None, Bus::Virtio) },
];

/// The size of the raw disks of the `disk-raw` and `disk-errors` tests.
const RAW_KIB: u64 = 8 * 1024;

/// A raw test disk on `bus`, which the host checks with `check`.
const fn raw_disk(bus: Bus, check: DiskCheck) -> Disk {
    Disk {
        kib: RAW_KIB,
        fat: 0,
        cluster: 0,
        label: "",
        table: Table::None,
        bus,
        sector: 512,
        device: "",
        errors: None,
        read_only: false,
        check,
    }
}

/// The `disk-raw` test's disks: one virtio, one solid state SATA disk and
/// an NVMe namespace.
const RAW_DISKS: &[Disk] = &[
    raw_disk(Bus::Virtio, raw_check),
    Disk { device: ",rotation_rate=1", ..raw_disk(Bus::Sata, raw_check) },
    raw_disk(Bus::Nvme(0), raw_check),
];

/// The `iommu` test's disks: [`RAW_DISKS`], the virtio disk's DMA going
/// through the IOMMU as every other device's does.
const IOMMU_DISKS: &[Disk] = &[
    Disk { device: ",disable-legacy=on,iommu_platform=on", ..raw_disk(Bus::Virtio, raw_check) },
    Disk { device: ",rotation_rate=1", ..raw_disk(Bus::Sata, raw_check) },
    raw_disk(Bus::Nvme(0), raw_check),
];

/// The `disk-raw-4k` test's disks, of 4 KiB sectors: one virtio disk and
/// an NVMe namespace.
const RAW_DISKS_4K: &[Disk] = &[
    Disk { sector: 4096, ..raw_disk(Bus::Virtio, raw_check) },
    Disk { sector: 4096, ..raw_disk(Bus::Nvme(0), raw_check) },
];

/// What the `disk-raw` test's program writes to each disk: at these
/// offsets, these many bytes, each the complement of the one there
/// before; and one write of 20 bytes 10 before the end, of which 10 fit.
/// Then it discards [`RAW_DISCARD`] (offset and length), which QEMU
/// zeroes. The program (userspace/storage) has the same lists.
const RAW_WRITES: &[(u64, u64)] =
    &[(513, 3), (3 * 512 - 1, 2), (20 * 4096 + 100, 70_000), (1 << 20, 8192)];
const RAW_DISCARD: (u64, u64) = (4 << 20, 1 << 20);

/// The `net` test's network card: virtio 1.0 only, behind the IOMMU, with
/// the MAC address the test looks for.
const NET_CARD: &str = "virtio-net-pci,netdev=n0,disable-legacy=on,iommu_platform=on,\
                        mac=52:54:00:12:34:56";

/// The `sockets` test's network: QEMU's user network, which runs `cat`
/// for each connection to 10.0.2.100 port 7, the test's echo server.
const ECHO_NETDEV: &str = "user,id=n0,guestfwd=tcp:10.0.2.100:7-cmd:cat";

/// The size of the `disk-check` test's disk: the smallest disktest takes.
const CHECK_KIB: u64 = 128 * 1024;

/// The `disk-check` test's disk: an NVMe namespace of the first
/// controller, whose serial number is `dukovo-test-0`, which the host
/// checks with Linux's side of disktest ([`linux_disk_check`]).
const CHECK_DISKS: &[Disk] =
    &[Disk { kib: CHECK_KIB, check: linux_disk_check, ..raw_disk(Bus::Nvme(0), raw_check) }];

/// The `disk-errors` test's disks, a SATA disk and an NVMe namespace,
/// and the errors QEMU gives their reads and writes ([`ERROR_RULES`]):
/// a read of sector 6000 fails once, one of sector 10000 every time, and
/// so does a write of sector 12000. The program (userspace/storage) reads
/// and writes those sectors.
const ERROR_DISKS: &[Disk] = &[
    Disk { errors: Some(error_rules), ..raw_disk(Bus::Sata, raw_unchanged) },
    Disk { errors: Some(error_rules), ..raw_disk(Bus::Nvme(0), raw_unchanged) },
];

/// The `disk-health` test's disks, raw: a SATA disk, which keeps SMART,
/// an NVMe namespace, whose controller keeps a health log, and a virtio
/// disk, which keeps neither: `sda`, `nvme0n1` and `vda`.
const HEALTH_DISKS: &[Disk] = &[
    raw_disk(Bus::Sata, raw_unchanged),
    raw_disk(Bus::Nvme(0), raw_unchanged),
    raw_disk(Bus::Virtio, raw_unchanged),
];

/// The `storage-tools` test's disks: FAT32 in a GPT partition, `vda1`,
/// and a Btrfs disk to write, `vdb`, which Spark mounts at
/// `/volumes/vda1` and `/volumes/vdb`.
const TOOLS_DISKS: &[Disk] = &[
    fat_disk(48 * 1024, 32, "RADIXPART", Table::Gpt, Bus::Virtio),
    btrfs_disk(Bus::Virtio, 512, BTRFS_NEW, btrfs_checked),
];

/// [`ERROR_RULES`], for each of [`ERROR_DISKS`].
fn error_rules(_: usize) -> String {
    ERROR_RULES.into()
}

/// The size of the `btrfs` test's disks.
const BTRFS_KIB: u64 = 128 * 1024;

/// A Btrfs test disk on `bus`, with sectors of `sector` bytes, holding
/// `fat` ([`BTRFS`] or [`BTRFS_NEW`]), which the host checks with
/// `check`.
const fn btrfs_disk(bus: Bus, sector: u64, fat: u8, check: DiskCheck) -> Disk {
    Disk {
        kib: BTRFS_KIB,
        fat,
        cluster: 0,
        label: "RADIXBTRFS",
        table: Table::None,
        bus,
        sector,
        device: "",
        errors: None,
        read_only: false,
        check,
    }
}

/// The `btrfs` test's disks: an NVMe namespace of 4 KiB sectors with
/// BLAKE2b, nodes of 4 KiB, metadata once, data compressed with zstd and
/// holes named, without `no-holes`, which Radix does not write; and a
/// virtio disk made as `mkfs.btrfs` makes one by default (CRC-32C, nodes
/// of 16 KiB, metadata twice) but for data compressed with zlib, attached
/// read-only, as QEMU's NVMe namespaces do not say they are
/// ([`make_btrfs`]): `nvme0n1` and `vda`, in the order the kernel finds
/// them, as the NVMe controller comes first. Spark mounts both read-only,
/// and the host checks that they are as it made them.
const BTRFS_DISKS: &[Disk] = &[
    btrfs_disk(Bus::Nvme(0), 4096, BTRFS, btrfs_unchanged),
    Disk { read_only: true, ..btrfs_disk(Bus::Virtio, 512, BTRFS, btrfs_unchanged) },
];

/// The `btrfs-write` test's disks: of the `btrfs` test's two kinds, but
/// holding only the host's file; Linux's `btrfs check` checks them after
/// each boot. Spark has the first, [`BTRFS_COMPRESSED`], compress what is
/// written to it.
const BTRFS_WRITE_DISKS: &[Disk] = &[
    btrfs_disk(Bus::Nvme(0), 4096, BTRFS_NEW, btrfs_checked),
    btrfs_disk(Bus::Virtio, 512, BTRFS_NEW, btrfs_checked),
];

/// The disk of the `btrfs-write` test that compresses (its command line
/// names it to Spark too).
const BTRFS_COMPRESSED: &str = "nvme0n1";

/// The `btrfs-power-cut` test's disk: a virtio disk as `mkfs.btrfs` makes
/// one by default, holding only the host's file.
const POWER_CUT_DISKS: &[Disk] = &[btrfs_disk(Bus::Virtio, 512, BTRFS_NEW, btrfs_cut)];

/// The `btrfs-log` test's disk: the virtio disk of 128 MiB Linux left
/// with a log of fsyncs to replay.
const LOG_DISKS: &[Disk] =
    &[Disk { label: "LOG", ..btrfs_disk(Bus::Virtio, 512, BTRFS_LOG, btrfs_replayed) }];

/// The `btrfs-raid` test's disks: two virtio disks of 512 MiB, `vda` and
/// `vdb`, of one Btrfs RAID1, a third, `vdc`, that the second boot puts
/// in the place of `vdb`, pulled out (ADR 0155), and a fourth, `vdd`,
/// that it adds before it balances and takes `vda` out (ADR 0159).
const RAID_DISKS: &[Disk] = &[RAID_DISK, RAID_DISK, RAID_NEW, RAID_NEW];

/// A disk of the `btrfs-raid` test.
const RAID_DISK: Disk = Disk {
    kib: 512 * 1024,
    label: "RAIDTEST",
    ..btrfs_disk(Bus::Virtio, 512, BTRFS_RAID, raid_checked)
};

/// The `btrfs-raid-pull` test's disks: two virtio disks of 512 MiB, `vda`
/// and `vdb`, of one Btrfs RAID1, one of which QEMU fails at a random
/// write ([`pull_rules`]).
const RAID_PULL_DISKS: &[Disk] = &[RAID_PULLED, RAID_PULLED];

/// A disk of the `btrfs-raid-pull` test.
const RAID_PULLED: Disk = Disk { label: "RAIDPULL", errors: Some(pull_rules), ..RAID_DISK };

/// The writes to a disk of the `btrfs-raid-pull` test before the one it
/// fails at: [`PULL_FIRST`] and fewer than [`PULL_SPAN`] more. The first
/// boot writes each disk some 150 times (QEMU's `virtio_blk_handle_write`
/// trace), so the disk fails in the first third, and commits come after
/// that leave it behind.
const PULL_FIRST: u32 = 1;
const PULL_SPAN: u32 = 48;

/// The disk of the `btrfs-raid` test that replaces the one pulled out:
/// raw bytes, of no filesystem, until the second boot rebuilds onto it.
const RAID_NEW: Disk = Disk { fat: 0, label: "", ..RAID_DISK };

/// The `swap` test's disk: a 1 GiB swap area on a virtio disk, `vda`.
const SWAP_DISKS: &[Disk] = &[Disk {
    kib: SWAP_KIB,
    fat: SWAP,
    cluster: 0,
    label: "SWAPTEST",
    table: Table::None,
    bus: Bus::Virtio,
    sector: 512,
    device: "",
    errors: None,
    read_only: false,
    check: swap_kept,
}];

/// The size of the `swap` test's area, in KiB.
const SWAP_KIB: u64 = 1 << 20;

/// The UUID the host's `mkswap` gives the `swap` test's area.
const SWAP_UUID: &str = "6c6f9a34-1b7e-4f0e-9a51-0d2f3c5b8e17";

/// The `root` test's disk: a copy of the test image with a Dukovo root
/// partition, its fifth, as a virtio disk: `vda5`.
const ROOT_DISKS: &[Disk] =
    &[Disk { label: image::ROOT_LABEL, ..btrfs_disk(Bus::Virtio, 512, ROOT, root_checked) }];

/// The round of the storage program's churn after whose file the
/// `btrfs-power-cut` test cuts the power, 0 to 1.5 seconds later.
const CUT_AFTER: usize = 30;

/// An exFAT test disk on `bus` of `kib` KiB, with clusters of `cluster`
/// bytes, which exfatprogs' `fsck.exfat` checks after each boot.
const fn exfat_disk(kib: u64, cluster: u32, label: &'static str, bus: Bus) -> Disk {
    Disk {
        kib,
        fat: EXFAT,
        cluster,
        label,
        table: Table::None,
        bus,
        sector: 512,
        device: "",
        errors: None,
        read_only: false,
        check: exfat_checked,
    }
}

/// The `exfat-write` test's disks: an NVMe namespace with clusters of 32
/// KiB, larger than a page, and a virtio disk with clusters of a sector:
/// `nvme0n1` and `vda`, in the order the kernel finds them.
const EXFAT_DISKS: &[Disk] = &[
    exfat_disk(64 * 1024, 32 * 1024, "RADIXEXFAT", Bus::Nvme(0)),
    exfat_disk(32 * 1024, 512, "RADIXSMALL", Bus::Virtio),
];

/// An ext4 test disk on `bus` of `kib` KiB, with sectors of `sector`
/// bytes and blocks of `block`, as `mkfs.ext4` makes it, which the host
/// checks with `check`.
const fn ext4_disk(
    kib: u64,
    block: u32,
    label: &'static str,
    bus: Bus,
    sector: u64,
    check: DiskCheck,
) -> Disk {
    Disk {
        kib,
        fat: EXT4,
        cluster: block,
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

/// The `ext4-write` test's disks: an NVMe namespace of 4 KiB sectors with
/// 4 KiB blocks, as `mkfs.ext4` makes it by default, and a virtio disk
/// with 1 KiB blocks, smaller than a page: `nvme0n1` and `vda`, in the
/// order the kernel finds them.
const EXT4_DISKS: &[Disk] = &[
    ext4_disk(64 * 1024, 4096, "RADIXEXT4", Bus::Nvme(0), 4096, ext4_checked),
    ext4_disk(32 * 1024, 1024, "RADIXSMALL", Bus::Virtio, 512, ext4_checked),
];

/// The `ext4-power-cut` test's disk: a virtio disk as `mkfs.ext4` makes
/// one by default, holding only the host's file.
const EXT4_CUT_DISKS: &[Disk] =
    &[ext4_disk(64 * 1024, 4096, "RADIXEXT4", Bus::Virtio, 512, ext4_cut)];

/// QEMU's blkdebug rules for [`ERROR_DISKS`].
const ERROR_RULES: &str = "[inject-error]\nevent = \"read_aio\"\nerrno = \"5\"\nsector = \"6000\"\nonce = \"on\"\n\n\
     [inject-error]\nevent = \"read_aio\"\nerrno = \"5\"\nsector = \"10000\"\n\n\
     [inject-error]\nevent = \"write_aio\"\nerrno = \"5\"\nsector = \"12000\"\n";

/// What the storage test's program writes (userspace/storage), as the
/// host reads it back with mtools.
const STORAGE_HELLO: &[u8] = b"Written by Dukovo before a reboot.\n";
const STORAGE_LONG_NAME: &str = "A file with a long name, and spaces.txt";
const STORAGE_BIG: usize = 200 * 1024 + 123;
const STORAGE_MANY: usize = 40;
/// What the host writes to each disk between the boots, for the program
/// to read back.
const STORAGE_FROM_HOST: &[u8] = b"Written on the host between the boots.\n";
/// Its name, which `mkfs.btrfs` puts on a Btrfs disk to write before the
/// first boot instead.
const STORAGE_FROM_HOST_NAME: &str = "Written on the host.txt";
/// The size of the program's `grow.bin`, on Btrfs volumes only.
const STORAGE_GROW: usize = 12 * 1024 * 1024 + 4321;
/// The size of the program's `words.txt` on a Btrfs volume, and the text
/// of each of its 64-byte lines after the line's number.
const STORAGE_WORDS: usize = 4 * 128 * 1024 + 70_001;
const STORAGE_WORDS_LINE: &[u8; 56] = b" words of a file that compresses, alike on each of them\n";

/// The CPU the microcode test emulates: an Intel Skylake client part,
/// signature 0x506e3.
const MICROCODE_CPU: &str = "qemu64,vendor=GenuineIntel,family=6,model=94,stepping=3";
const MICROCODE_SIGNATURE: u32 = 0x506e3;
const MICROCODE_REVISION: u32 = 0x7f;

// Each phase that is built in parallel with others adds its boot tests to
// a file of its own, so that two pull requests do not add theirs at the
// same place in CASES. They run after CASES, in the order of the phases.
mod phase11;
mod phase12;
mod phase13;

/// Every boot test, in the order they run: CASES, then each phase's.
pub fn all_cases() -> impl Iterator<Item = &'static Case> {
    CASES.iter().chain(phase11::CASES).chain(phase12::CASES).chain(phase13::CASES)
}

/// `--shard <part>/<parts>`, as CI gives it to run the boot tests on
/// several machines at once.
fn parse_shard(value: &str) -> Result<(usize, usize)> {
    let parsed = value.split_once('/').and_then(|(part, parts)| {
        Some((part.parse::<usize>().ok()?, parts.parse::<usize>().ok()?))
    });
    match parsed {
        Some((part, parts)) if (1..=parts).contains(&part) => Ok((part, parts)),
        _ => crate::bail!("--shard takes <part>/<parts> such as 1/3, not `{value}`"),
    }
}

/// The cases of shard `part` of `parts`: every `parts`th, starting at the
/// `part`th, so that each shard gets some of each kind and the slow ones
/// (which sit together in CASES, such as the Btrfs tests) are spread out.
fn in_shard(cases: Vec<&Case>, part: usize, parts: usize) -> Vec<&Case> {
    cases.into_iter().enumerate().filter(|(i, _)| i % parts == part - 1).map(|(_, c)| c).collect()
}

/// The boot tests, in the order they run. New ones of a phase built in
/// parallel go in that phase's file (test/phase11.rs and so on) instead.
pub const CASES: &[Case] = &[
    Case {
        name: "uefi-usb",
        about: "4 CPUs boot under UEFI from USB",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: boot,
        disks: &[],
        second: None,
    },
    Case {
        name: "bios-usb",
        about: "4 CPUs boot under BIOS from USB, with the lock checker on",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: boot,
        disks: &[],
        second: None,
    },
    Case {
        name: "uefi-cdrom",
        about: "4 CPUs boot under UEFI from CD-ROM, local APICs in xAPIC mode",
        firmware: Firmware::Uefi,
        media: Media::Cdrom,
        cmdline: "qemu-exit nox2apic",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: boot_xapic,
        disks: &[],
        second: None,
    },
    Case {
        name: "bios-cdrom",
        about: "4 CPUs boot under BIOS from CD-ROM, without KASLR and PCIDs",
        firmware: Firmware::Bios,
        media: Media::Cdrom,
        cmdline: "qemu-exit nokaslr nopcid",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: boot,
        disks: &[],
        second: None,
    },
    Case {
        name: "screen",
        about: "the boot log is readable on the UEFI framebuffer",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        // A program, so that Spark starts no services: consoled would take
        // the screen from the log.
        cmdline: "-- /system/tests/hello",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: screen_log,
        disks: &[],
        second: None,
    },
    Case {
        name: "consoles",
        about: "a normal boot shows a shell on consoled's virtual consoles; typing, notifications on the bottom line, Alt+F2, a German keyboard layout, poweroff",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        // No program: Spark starts the services, as on a normal boot.
        cmdline: "console-sign-in=off",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: consoles,
        disks: &[],
        second: None,
    },
    Case {
        name: "power-button",
        about: "a normal boot under BIOS; pressing the ACPI power button stops the services and powers the machine off",
        firmware: Firmware::Bios,
        media: Media::Usb,
        // No program: Spark starts the services, as on a normal boot.
        cmdline: "",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: power_button,
        disks: &[],
        second: None,
    },
    Case {
        name: "aml-off",
        about: "with aml=off no AML runs, under UEFI: the kernel hears the ACPI power button and powers the machine off by itself",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "aml=off",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: power_button,
        disks: &[],
        second: None,
    },
    Case {
        name: "acpi-power",
        about: "QEMU with an ACPI table of the test's: uACPI finds its battery, AC adapter, lid and power button, Linux's /sys/class/power_supply and /proc/acpi/button/lid show them at the console, and the power button powers off",
        firmware: Firmware::Bios,
        media: Media::Usb,
        // No program: Spark starts the console, as on a normal boot.
        cmdline: "console-sign-in=off",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &["-acpitable", POWER_TABLE],
        script: acpi_power,
        disks: &[],
        second: None,
    },
    Case {
        name: "acpi-thermal",
        about: "QEMU with an ACPI table of the test's: uACPI finds its thermal zone, Linux's /sys/class/thermal and sensors show it at the console, and as it warms past its passive, hot and critical trip points the kernel says so and Spark powers the machine off",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "console-sign-in=off",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &["-acpitable", THERMAL_TABLE],
        script: acpi_thermal,
        disks: &[],
        second: None,
    },
    Case {
        name: "usb-devices",
        about: "the xHCI controller under UEFI names the boot stick, a keyboard, two hubs and the devices behind them, lsusb lists them and their tree, the controller follows devices plugged in and pulled out, and has no Debug Capability for dbc",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        // No program: Spark starts the services, and the power button ends
        // the test. QEMU's controller has no Debug Capability for `dbc`.
        cmdline: "lockdep dbc console-sign-in=off",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[
            "-device",
            "usb-kbd,bus=xhci.0,port=2",
            "-device",
            "usb-hub,bus=xhci.0,port=3",
            "-device",
            "usb-mouse,bus=xhci.0,port=3.1",
            "-device",
            "usb-hub,bus=xhci.0,port=3.8,id=hub2",
            "-device",
            "usb-kbd,bus=xhci.0,port=3.8.4",
        ],
        script: usb_devices,
        disks: &[],
        second: None,
    },
    Case {
        name: "usb-input",
        about: "a USB keyboard types on the screen's shell beside the PS/2 one, its held key repeats, and a USB mouse and tablet are read from /devices/mouse",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        // No program: Spark starts the services, as on a normal boot.
        cmdline: "lockdep console-sign-in=off",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        // The USB keyboard is bound to the display, so that QMP's
        // input-send-event reaches it and send-key the PS/2 keyboard. A
        // USB mouse cannot be bound; QEMU gives it the motion and buttons
        // that no pointer bound to the display takes, before the PS/2
        // mouse, which the kernel does not start. The script plugs a
        // tablet in later.
        qemu: &[
            "-vga",
            "none",
            "-device",
            "VGA,id=video0",
            "-device",
            "usb-kbd,bus=xhci.0,port=2,id=usbkbd,display=video0",
            "-device",
            "usb-mouse,bus=xhci.0,port=3",
        ],
        script: usb_input,
        disks: &[],
        second: None,
    },
    Case {
        name: "watchdog",
        about: "Spark pets QEMU's i6300esb; when it stops responding, the watchdog restarts the machine once kdb stops petting it, and the next boot says so",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "kdb -- --watchdog 12 --test-hang",
        cpus: 2,
        memory: "1G",
        reboot: true,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &["-device", "i6300esb"],
        script: watchdog_esb,
        disks: &[],
        second: None,
    },
    Case {
        name: "watchdog-tco",
        about: "as watchdog, under UEFI, with the TCO timer of q35's ICH9",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "-- --watchdog 12 --test-hang",
        cpus: 2,
        memory: "1G",
        reboot: true,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: watchdog_tco,
        disks: &[],
        second: None,
    },
    Case {
        name: "machine-check",
        about: "a corrected error QEMU puts in a machine-check bank is logged at the next read, and a memory controller's counted by channel in Linux's EDAC files; an uncorrected one panics, and the next boot finds it in its bank",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "mce-poll=1 panic=0 console-sign-in=off",
        cpus: 2,
        memory: "1G",
        reboot: true,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: machine_check,
        disks: &[],
        second: None,
    },
    Case {
        name: "machine-check-program",
        about: "a page QEMU says went bad ends only the program that read it, with SIGBUS, and leaves its memory and the frame allocator for good; a bad page no one read goes too",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "mce-poll=1 -- --posix /system/tests/posix/memfail",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        // Intel's, whose software error recovery QEMU emulates.
        cpu_model: Some(MICROCODE_CPU),
        qemu: &[],
        script: machine_check_program,
        disks: &[],
        second: None,
    },
    Case {
        name: "spark-oom",
        about: "a service that runs the machine out of memory is stopped, and Spark starts it again",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "lockdep -- --service-dir /system/tests/services",
        cpus: 2,
        memory: "256M",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: spark_oom,
        disks: &[],
        second: None,
    },
    Case {
        name: "swap",
        about: "a swap area Linux's mkswap made on a virtio disk: swapon turns it on, a program fills 384 MiB of a 256 MiB machine and gets it all back, through a fork too, and swapoff turns it off with pages still on it; the area stays as mkswap made it",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- --posix /system/tests/posix/swaptest /devices/vda 384",
        cpus: 2,
        memory: "256M",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: swap,
        disks: SWAP_DISKS,
        second: None,
    },
    Case {
        name: "spark-stop",
        about: "Spark stops a service that ignores SIGTERM, and what a service leaves behind",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "lockdep -- --service-dir /system/tests/services/stop",
        cpus: 2,
        memory: "256M",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: spark_stop,
        disks: &[],
        second: None,
    },
    Case {
        name: "logging",
        about: "logd keeps the kernel log, the services' lines among them, as lines of JSON; log, journalctl and dmesg read it by service, level, time and pattern; a program cannot pass its lines off as a service's; logd started again keeps no record twice; the audit log holds Spark's and the kernel's security events and no program's; logtrim and journalctl --vacuum-time trim the log; notifyd names each notification's sender as the kernel does, refuses a flood and logs none of them",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "lockdep -- --services --posix /system/tests/posix/logtest",
        cpus: 2,
        memory: "512M",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: logging,
        disks: &[],
        second: None,
    },
    Case {
        name: "principals",
        about: "processes and capabilities act for names: alice, with a root stamped for her and no bypass right, cannot read bob's 0700 home, give her files away, remove bob's files from a sticky directory, run a file without an execute bit, or signal, trace or read more than the status of bob's processes, while root's bypass right reaches them all",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "lockdep -- --posix --names /system/tests/posix/principals",
        cpus: 2,
        memory: "512M",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: principals,
        disks: &[],
        second: None,
    },
    Case {
        name: "views",
        about: "a view made of attached capabilities, as a session's will be: alice, whose root is one, sees its names at their real paths and nothing past them, with .., getcwd, realpath and links working across them, a /tmp of the session's own, read-only entries and directories of the view's that take nothing, no mount or attach in it, and a view of her own; and what a view refuses to take",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "lockdep -- --posix --names /system/tests/posix/views",
        cpus: 2,
        memory: "512M",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: views,
        disks: &[],
        second: None,
    },
    Case {
        name: "services",
        about: "service files in TOML: every shipped one reads with service check and is kept as Spark's last good copy; Spark converts a file of before Phase 13 with its comments as it starts its service; service new writes a template and service edit puts a file in place only once it reads; a file broken under a running service leaves its last good copy running; a service sees only the view its file declares, and reads there the secret service secret set kept for it",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "lockdep -- --services --posix /system/tests/posix/servicetest",
        cpus: 2,
        memory: "512M",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: services,
        disks: &[],
        second: None,
    },
    Case {
        name: "accounts",
        about: "authd keeps the accounts: user add makes an administrator and an account with an empty password, with their groups, homes and Argon2id hashes in files of the right modes, which getpwnam, getgrnam, id and user list read; a right password opens an account, a wrong one or a name there is not is answered alike after a wait, and tries at once wait their turn; root changes a password with passwd, and alice changes hers given hers, but not bob's, makes no account, checks no one else's password and reads no hash",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "lockdep -- --services --posix --names /system/tests/posix/accounttest",
        cpus: 2,
        memory: "512M",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: accounts,
        disks: &[],
        second: None,
    },
    Case {
        name: "signin",
        about: "sign-in on the consoles: a machine with no account asks for its owner's, an administrator's, and signs her in; a second account signs in on Alt+F2 beside her; each session acts for its user on its own terminal, who lists both, neither reads the other's home, bob mounts and unmounts nothing, a wrong password waits and fails, and signing out shows the sign-in again",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        // Spark starts the services, consoled among them with sign-in, and
        // signintest as root, which makes bob once alice is made.
        cmdline: "lockdep -- --services --posix /system/tests/posix/signintest",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: signin,
        disks: &[],
        second: None,
    },
    Case {
        name: "spark-network",
        about: "Spark gives each service the network its file asks for, and none to one that asks for nothing",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- --service-dir /system/tests/services/network",
        cpus: 2,
        memory: "512M",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: spark_network,
        disks: &[],
        second: None,
    },
    Case {
        name: "page-fault",
        about: "a page fault panics with registers and a symbolized backtrace, on screen too",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "selftest=page-fault",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: page_fault,
        disks: &[],
        second: None,
    },
    Case {
        name: "crash-log-uefi",
        about: "the panic report survives a restart under UEFI",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=crash-reboot panic=0",
        cpus: 2,
        // With more than 4 GiB the log goes above 4 GiB, as on the Dell.
        memory: "5G",
        reboot: true,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: crash_reboot,
        disks: &[],
        second: None,
    },
    Case {
        name: "crash-log-bios",
        about: "the panic report survives a restart under BIOS",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=crash-reboot panic=0",
        cpus: 2,
        // With less, the log goes at 64 MiB.
        memory: "1G",
        reboot: true,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: crash_reboot,
        disks: &[],
        second: None,
    },
    Case {
        name: "kdb",
        about: "kdb stops at a breakpoint and answers every command on the serial port",
        firmware: Firmware::Bios,
        media: Media::Cdrom,
        cmdline: "qemu-exit kdb trace selftest=breakpoint",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: kdb,
        disks: &[],
        second: None,
    },
    Case {
        name: "mm-stress",
        about: "4 CPUs on 2 NUMA nodes stress the heap and the mapping window without leaks",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=mm-stress",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: TWO_NODES,
        script: mm_stress,
        disks: &[],
        second: None,
    },
    Case {
        name: "tlb",
        about: "TLB shootdowns and address-space switches are seen by all 4 CPUs",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=tlb",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: tlb,
        disks: &[],
        second: None,
    },
    Case {
        name: "framework-ec",
        about: "QEMU with an ACPI table of the test's that names a Framework EC: the kernel finds the device, sees no EC's memory map behind it and leaves it alone; ectool at the console, whose shell holds the platform capability, says there is no EC",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "console-sign-in=off",
        cpus: 1,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &["-acpitable", CROSEC_TABLE],
        script: framework_ec,
        disks: &[],
        second: None,
    },
    Case {
        name: "smbus",
        about: "the kernel drives the SMBus controller of q35's ICH9 and finds the eight memory modules' EEPROMs QEMU puts on it; SeaBIOS's SMBIOS tables are found in its area, and say the memory is DDR4, so the first module's EEPROM is read as an EE1004 at the console, with Linux's /sys/bus/i2c files, and hwinfo decodes it",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "console-sign-in=off",
        cpus: 1,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        // QEMU has no EE1004: two DDC devices take its page selects.
        qemu: &[
            "-smbios",
            DDR4_DEVICE,
            "-device",
            "i2c-ddc,address=0x36",
            "-device",
            "i2c-ddc,address=0x37",
        ],
        script: smbus,
        disks: &[],
        second: None,
    },
    Case {
        name: "smbios",
        about: "the kernel finds QEMU's SMBIOS tables through the UEFI system table, logs the machine as Linux does and shows its names in Linux's /sys/class/dmi/id, without the serial numbers, and says QEMU's kind of memory has no SPD EEPROM it reads; hwinfo shows them all at the console",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "console-sign-in=off",
        cpus: 1,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[
            "-smbios",
            "type=1,manufacturer=Dukovo,product=Test Machine,version=2.0,serial=DK-0042,sku=DK-T1,family=Testers",
            "-smbios",
            "type=2,manufacturer=Dukovo,product=Test Board,serial=DB-0042",
            "-smbios",
            "type=17,part=DK-RAM-1,speed=3200",
        ],
        script: smbios,
        disks: &[],
        second: None,
    },
    Case {
        name: "gpio",
        about: "QEMU with a test device's memory as an AMD and an Intel GPIO controller's registers, which an ACPI table of the test's names: the kernel sets both up and routes their interrupts, asks for the firmware's events on their pins, and each event the table's AML fires runs its method once",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=gpio",
        cpus: 1,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &["-device", "pc-testdev", "-acpitable", GPIO_TABLE],
        script: gpio,
        disks: &[],
        second: None,
    },
    Case {
        name: "msr",
        about: "a read of an MSR the CPU lacks faults and goes on",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=msr",
        cpus: 1,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: msr,
        disks: &[],
        second: None,
    },
    Case {
        name: "caps",
        about: "no capability can be forged or widened, and revokes win against 3 other CPUs",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=caps",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: caps,
        disks: &[],
        second: None,
    },
    Case {
        name: "stack-overflow",
        about: "a kernel stack overflow hits its guard page and panics saying so",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=stack-overflow",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: stack_overflow,
        disks: &[],
        second: None,
    },
    Case {
        name: "smep",
        about: "SMEP stops the kernel from running code in a user page",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=smep",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: smep,
        disks: &[],
        second: None,
    },
    Case {
        name: "smap",
        about: "SMAP stops the kernel from reading a user page",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=smap",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: smap,
        disks: &[],
        second: None,
    },
    Case {
        name: "microcode",
        about: "a microcode module is found, checked and matched to an emulated Intel CPU",
        firmware: Firmware::Bios,
        media: Media::Cdrom,
        cmdline: "qemu-exit",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Microcode,
        cpu_model: Some(MICROCODE_CPU),
        qemu: &[],
        script: microcode,
        disks: &[],
        second: None,
    },
    Case {
        name: "fpu",
        about: "vector registers survive thousands of switches on 4 CPUs",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=fpu",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: fpu,
        disks: &[],
        second: None,
    },
    Case {
        name: "sched-fair",
        about: "fair threads share a CPU by nice level and domain CPU weight",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=sched-fair",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: sched_fair,
        disks: &[],
        second: None,
    },
    Case {
        name: "sched-rt",
        about: "real-time threads preempt at once, on time, FIFO and round robin",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=sched-rt",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: sched_rt,
        disks: &[],
        second: None,
    },
    Case {
        name: "sched-pi",
        about: "priority inheritance ends priority inversion, also through a chain",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=sched-pi",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: sched_pi,
        disks: &[],
        second: None,
    },
    Case {
        name: "sched-balance",
        about: "threads started on one CPU spread over 4 CPUs on 2 NUMA nodes",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=sched-balance",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: TWO_NODES,
        script: sched_balance,
        disks: &[],
        second: None,
    },
    Case {
        name: "lockdep",
        about: "the lock checker catches seeded deadlocks between spin locks and mutexes",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=lockdep",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: lockdep,
        disks: &[],
        second: None,
    },
    Case {
        name: "rcu",
        about: "RCU readers on 3 CPUs never see an object retired too early",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=rcu",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: rcu,
        disks: &[],
        second: None,
    },
    Case {
        name: "user",
        about: "a user program checks every system call; 8 kinds of fault end only theirs",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=user",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: user,
        disks: &[],
        second: None,
    },
    Case {
        name: "ipc",
        about: "user programs ping-pong over endpoints, notifications, channels and rings",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=ipc",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: ipc,
        disks: &[],
        second: None,
    },
    Case {
        name: "fuzz",
        about: "groups of user programs make random system calls; the kernel stands and leaks nothing",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=fuzz",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: fuzz,
        disks: &[],
        second: None,
    },
    Case {
        name: "spark",
        about: "the kernel starts Spark from the boot archive; Spark starts a program, reaps it",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/hello one two",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: spark,
        disks: &[],
        second: None,
    },
    Case {
        name: "storage",
        about: "Spark mounts four FAT disks; what is written reads back after a reboot and on the host, and a chown to another user is refused, as FAT keeps no owners",
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
        disks: STORAGE_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage check",
            before: storage_host,
            script: storage_check,
        }),
    },
    Case {
        name: "storage-sata",
        about: "The storage test on four SATA disks of an AHCI controller, one of them solid state and partitioned with a GPT",
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
        disks: SATA_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage check",
            before: storage_host,
            script: storage_check,
        }),
    },
    Case {
        name: "storage-usb",
        about: "The storage test on four USB sticks, two at SuperSpeed and two at full speed behind a hub, one partitioned with a GPT; then one is pulled out and plugged in again",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage write",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: USB_HUB,
        script: storage_write,
        disks: USB_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage check",
            before: storage_host,
            script: storage_usb_check,
        }),
    },
    Case {
        name: "storage-uas",
        about: "The storage test on four disks of three USB devices that speak USB Attached SCSI: two logical units of one at SuperSpeed, with streams, and two at high speed, without, one partitioned with a GPT; then the one with two units is pulled out",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage write",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: UAS_PORTS,
        script: storage_write,
        disks: UAS_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage check",
            before: storage_host,
            script: storage_uas_check,
        }),
    },
    Case {
        name: "volumes",
        about: "volumed leaves a stick Spark mounted at boot as it is, unmounts it when it is pulled out, and mounts it by its label when it is plugged in again; a program writes a file there and ejects it with eject, and the host reads the file back",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- --services --posix /system/tests/posix/volumetest DUKOVOUSB",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: volumes,
        disks: VOLUME_DISKS,
        second: None,
    },
    Case {
        name: "storage-nvme",
        about: "The storage test on four NVMe namespaces of two controllers, each with a queue and an interrupt for each of four CPUs, one partitioned with a GPT",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage write",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: storage_write,
        disks: NVME_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage check",
            before: storage_host,
            script: storage_check,
        }),
    },
    Case {
        name: "storage-4k",
        about: "The storage test on two virtio disks and two NVMe namespaces of 4 KiB sectors, their FAT volumes of 4 KiB sectors too, one in a GPT of 4 KiB blocks",
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
        disks: DISKS_4K,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage check",
            before: storage_host,
            script: storage_check,
        }),
    },
    Case {
        name: "exfat-write",
        about: "The storage test on two exFAT disks Linux's mkfs.exfat made, with clusters of a sector and of 32 KiB: fsck.exfat passes after each boot, and The Sleuth Kit reads back what was written; a chown to another user is refused",
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
        disks: EXFAT_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage check",
            before: storage_host,
            script: storage_check,
        }),
    },
    Case {
        name: "disk-raw",
        about: "Raw reads, writes and discards of a virtio, a SATA and an NVMe disk, and Linux's block requests; the host checks every byte",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage raw vda sda nvme0n1",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: disk_raw,
        disks: RAW_DISKS,
        second: None,
    },
    Case {
        name: "iommu",
        about: "Behind QEMU's Intel IOMMU each device reaches only what it is given and raises only its own interrupts: the disk-raw checks pass, edu's DMA to a page never given or taken back is stopped, edu cannot raise another function's interrupt, and the power button's goes through the I/O APIC's entry",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep selftest=iommu init-after-selftest -- /system/tests/storage raw vda sda \
                  nvme0n1",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &["-device", "intel-iommu,intremap=on", "-device", "edu,dma_mask=0xffffffff"],
        script: iommu,
        disks: IOMMU_DISKS,
        second: None,
    },
    Case {
        name: "iommu-amd",
        about: "Behind QEMU's AMD IOMMU each device reaches only what it is given and raises only its own interrupts: the iommu checks, but for the stopped writes and interrupts, which QEMU's AMD IOMMU does not log",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep selftest=iommu init-after-selftest -- /system/tests/storage raw vda sda \
                  nvme0n1",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &["-device", "amd-iommu,intremap=on", "-device", "edu,dma_mask=0xffffffff"],
        script: iommu_amd,
        disks: IOMMU_DISKS,
        second: None,
    },
    Case {
        name: "disk-raw-4k",
        about: "The raw test on a virtio disk and an NVMe namespace of 4 KiB sectors",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage raw vda nvme0n1",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: disk_raw,
        disks: RAW_DISKS_4K,
        second: None,
    },
    Case {
        name: "disk-check",
        about: "The disk check of a real machine on an NVMe disk it finds by its serial number, with the benchmarks; then Linux's side reads back on the host what it wrote",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep selftest=bench init-after-selftest -- --no-volumes --posix \
                  /system/tests/posix/disktest --yes-erase dukovo-test-0 --bench \
                  /system/tests/posix/bench",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: disk_check,
        disks: CHECK_DISKS,
        second: None,
    },
    Case {
        name: "disk-errors",
        about: "QEMU fails reads and writes of a SATA and an NVMe disk: one that fails once goes again, one that always fails ends in EIO",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage errors sda nvme0n1",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: disk_errors,
        disks: ERROR_DISKS,
        second: None,
    },
    Case {
        name: "disk-health",
        about: "on a normal boot Spark's diskhealth service logs that QEMU's SATA disk and NVMe drive pass; diskhealth at the console shows them, and the virtio disk keeping no record",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        // No program: Spark starts the services, as on a normal boot.
        cmdline: "console-sign-in=off",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: disk_health,
        disks: HEALTH_DISKS,
        second: None,
    },
    Case {
        name: "storage-tools",
        about: "lsblk, blkid and findmnt at the console name a FAT32 partition in a GPT and a Btrfs disk, their labels and kinds, and where Spark mounted them; fstrim trims the Btrfs, which btrfs check then passes, and iostat shows the disk's requests, its discards counted",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        // No program: Spark mounts the disks, as on a normal boot.
        cmdline: "console-sign-in=off",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: storage_tools,
        disks: TOOLS_DISKS,
        second: None,
    },
    Case {
        name: "btrfs",
        about: "Spark mounts two Btrfs disks Linux's mkfs.btrfs made, compressed with zstd and zlib, read-only, one as the disk is and one as Radix does not write its holes; every file, link and subvolume reads as the host wrote it, and a damaged sector fails",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage list",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: btrfs_list,
        disks: BTRFS_DISKS,
        second: None,
    },
    Case {
        name: "btrfs-write",
        about: "The storage test on two Btrfs disks Linux's mkfs.btrfs made, past their first data chunk, one compressing with zstd: Linux's btrfs check passes after each boot, and btrfs restore reads back what was written, and dump-tree the owners and modes given",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- --compress nvme0n1 /system/tests/storage write",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: storage_write,
        disks: BTRFS_WRITE_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- --compress nvme0n1 /system/tests/storage check",
            before: storage_host,
            script: storage_check,
        }),
    },
    Case {
        name: "btrfs-stress",
        about: "The long filesystem stress run: torture's four workers make random changes for two minutes, at once, on two Btrfs disks Linux's mkfs.btrfs made, one compressing with zstd, each checking what it did; Linux's btrfs check passes, and after a reboot every file, link and directory is as torture works out again, and goes, and btrfs check passes once more",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- --compress nvme0n1 --posix /system/tests/posix/torture 0x5eed 120 \
                  /volumes/nvme0n1 /volumes/vda",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: torture_run,
        disks: BTRFS_WRITE_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- --compress nvme0n1 --posix /system/tests/posix/torture --check \
                      0x5eed /volumes/nvme0n1 /volumes/vda",
            before: nothing_between,
            script: torture_check,
        }),
    },
    Case {
        name: "btrfs-power-cut",
        about: "The power is cut while files are written, synced, renamed and removed on Btrfs: Linux's btrfs check passes, every synced file is whole, and the disk mounts and is written again",
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
        disks: POWER_CUT_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage churned",
            before: power_cut_host,
            script: power_cut_check,
        }),
    },
    Case {
        name: "btrfs-log",
        about: "Spark mounts a Btrfs disk Linux left with changes in its log of fsyncs alone, which the mount replays: every file, link and directory lists as Linux lists them after its own replay, the disk takes a write, and Linux's btrfs check passes",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage replayed",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: btrfs_log,
        disks: LOG_DISKS,
        second: None,
    },
    Case {
        name: "btrfs-raid",
        about: "Spark mounts a Btrfs RAID1 of two disks Linux's mkfs.btrfs made once, from the first, and the storage test writes it; with the second pulled out it mounts from the first alone, warns, and reads every byte back, then turns simple quotas on, puts a third disk in the place of the second, adds a fourth, balances, and takes the first out, reading it all again after each: Linux's btrfs check passes after each boot, from every disk alone too, and counts every quota group again",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage write",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: raid_write,
        disks: RAID_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage check --no-host-file --quota --replace 2 \
                      vdc --add vdd --balance --remove 1",
            before: raid_pull,
            script: raid_check,
        }),
    },
    Case {
        name: "btrfs-raid-pull",
        about: "One disk of a Btrfs RAID1 of two fails at a random write, as if pulled out, while the storage test writes it: the writes go on to the other, and the kernel warns; back after a reboot, the disk is read last until a scrub catches it up, and every byte reads back. Linux's btrfs check passes from each disk alone after the first boot, and from both after the second",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage write",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: raid_pulled,
        disks: RAID_PULL_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage check --no-host-file --scrub",
            before: raid_back,
            script: raid_caught_up,
        }),
    },
    Case {
        name: "ext4-write",
        about: "The storage test on two ext4 disks Linux's mkfs.ext4 made, with blocks of 4 KiB and 1 KiB: e2fsck passes after each boot, with the journal as Radix left it and replayed, and debugfs reads back what was written and the owners and modes given",
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
        disks: EXT4_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage check",
            before: storage_host,
            script: storage_check,
        }),
    },
    Case {
        name: "ext4-power-cut",
        about: "The power is cut while files are written, synced, renamed and removed on ext4: e2fsck passes once the journal is replayed, every synced file is whole, and Radix replays the journal and writes the disk again",
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
        disks: EXT4_CUT_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage churned",
            before: power_cut_host,
            script: power_cut_check,
        }),
    },
    Case {
        name: "root",
        about: "Dukovo's root on a copy of the test image as a virtio disk: Spark mounts the subvolumes of its Btrfs, /system read-only, and what is written to the others is there after a reboot, as is a read-only snapshot of /users, and a scrub finds no errors; the system's files are root's; Linux's btrfs check passes after each boot",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- /system/tests/storage root",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: root_write,
        disks: ROOT_DISKS,
        second: Some(SecondBoot {
            cmdline: "lockdep -- /system/tests/storage rooted",
            before: power_cut_host,
            script: root_read,
        }),
    },
    Case {
        name: "posix",
        about: "C programs on mlibc, a Rust std one and the ported utilities: fork, exec, signals, pipes, terminals, files, threads",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- --posix --network /system/tests/posix/run",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: posix,
        disks: &[],
        second: None,
    },
    Case {
        name: "oom",
        about: "out of memory stops an app, then a service its supervisor restarts",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=oom",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &[],
        script: oom,
        disks: &[],
        second: None,
    },
    Case {
        name: "net",
        about: "behind QEMU's Intel IOMMU, a virtio network card takes the address of the ip= and gateway= flags, and QEMU's router answers its pings",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit lockdep selftest=net ip=10.0.2.15/24 gateway=10.0.2.2",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &["-device", "intel-iommu,intremap=on", "-netdev", "user,id=n0", "-device", NET_CARD],
        script: net,
        disks: &[],
        second: None,
    },
    Case {
        name: "sockets",
        about: "a program connects and listens over TCP and UDP on the loopback interface and a virtio network card, as far as its network capabilities allow",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep ip=10.0.2.15/24 gateway=10.0.2.2 -- --network /system/tests/sockets virtio",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &["-netdev", ECHO_NETDEV, "-device", "virtio-net-pci,netdev=n0"],
        script: sockets,
        disks: &[],
        second: None,
    },
    Case {
        name: "interfaces",
        about: "a virtio network card with no address: a socket bound to it sends a DHCPDISCOVER from 0.0.0.0 and receives QEMU's offer, ip gives it the address and router offered and shows them, a connection goes through it, ss lists it, and ip takes them away again",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- --posix --network /system/tests/posix/interfaces",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &["-netdev", ECHO_NETDEV, "-device", "virtio-net-pci,netdev=n0"],
        script: interfaces,
        disks: &[],
        second: None,
    },
    Case {
        name: "dhcp",
        about: "netd sets a virtio network card with no address up by DHCP: QEMU's address, router and DNS server, a lease kept and gone on with when netd starts again, port 68 shared with sockets on other interfaces, an administrator's address kept, and the settings file's address in place of DHCP's and back",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- --service-dir /system/tests/services/dhcp --posix --network /system/tests/posix/dhcp",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        qemu: &["-netdev", ECHO_NETDEV, "-device", "virtio-net-pci,netdev=n0"],
        script: dhcp,
        disks: &[],
        second: None,
    },
];

/// `ob test`.
pub fn command(ctx: &Ctx, args: &mut Args) -> Result<()> {
    // The options first, so that the suite is not taken from a value of one.
    let timeout = match args.value("--timeout")? {
        Some(value) => value.parse::<u64>().ok().filter(|&secs| secs > 0).ok_or_else(|| {
            Error::new(format!("--timeout takes a number of seconds, not `{value}`"))
        })?,
        None => 300,
    };
    let mut selected = Vec::new();
    while let Some(name) = args.value("--case")? {
        selected.push(name);
    }
    if args.flag("--list") {
        args.finish()?;
        for case in all_cases() {
            println!("{:<24} {}", case.name, case.about);
        }
        return Ok(());
    }
    let shard = match args.value("--shard")? {
        Some(value) => Some(parse_shard(&value)?),
        None => None,
    };
    let profile = args.profile(&ctx.config)?;
    let which = args.positional();
    args.finish()?;
    let (unit, boot) = match which.as_deref() {
        None if selected.is_empty() => (true, true),
        None | Some("boot") => (false, true),
        Some("unit") if selected.is_empty() && shard.is_none() => (true, false),
        Some("kani") if selected.is_empty() => return kani(ctx),
        Some("tla") if selected.is_empty() => return tla::check(ctx),
        Some("corpus") => return corpus::test(ctx, &selected),
        Some(suite @ ("unit" | "kani" | "tla")) => {
            crate::bail!("--case and --shard select boot tests; they cannot be used with `{suite}`")
        }
        Some(other) => crate::bail!(
            "unknown test suite `{other}`; use `unit`, `boot`, `kani`, `tla` or `corpus`"
        ),
    };
    if shard.is_some() && which.as_deref() != Some("boot") {
        crate::bail!("--shard splits the boot tests; give `boot` with it");
    }
    let mut cases: Vec<&Case> = if selected.is_empty() {
        all_cases().collect()
    } else {
        let mut cases = Vec::new();
        for name in &selected {
            match all_cases().find(|case| case.name == name) {
                Some(case) => cases.push(case),
                None => {
                    let names: Vec<_> = all_cases().map(|case| case.name).collect();
                    crate::bail!("no boot test `{name}`; the tests are {}", names.join(", "));
                }
            }
        }
        cases
    };
    if let Some((part, parts)) = shard {
        cases = in_shard(cases, part, parts);
    }

    if unit {
        unit_tests(ctx)?;
    }
    if boot {
        let built = Built {
            kernel: kernel::build(ctx, profile)?,
            programs: userspace::build(ctx, profile)?,
        };
        boot_tests(ctx, &built, &cases, Duration::from_secs(timeout))?;
    }
    Ok(())
}

fn unit_tests(ctx: &Ctx) -> Result<()> {
    step("Running unit tests for the build tool");
    let mut cmd = process::cargo(ctx);
    cmd.args(["test", "--workspace"]);
    process::run(ctx, &mut cmd)?;

    step("Running unit tests for the kernel libraries on the host");
    let mut cmd = kernel::cargo(ctx, "test")?;
    for library in KERNEL_LIBRARIES {
        cmd.args(["-p", library]);
    }
    if let Some(progs) = btrfs::for_tests(ctx)? {
        cmd.env("RADIX_BTRFS_PROGS", progs);
    }
    process::run(ctx, &mut cmd)?;

    step("Running unit tests for the programs on Rust's standard library on the host");
    let mut cmd = userspace::host_cargo(ctx, "test");
    process::run(ctx, &mut cmd)
}

/// The kernel libraries with Kani proof harnesses (ADR 0019).
pub const PROVED_LIBRARIES: &[&str] = &[
    "radix-mm",
    "radix-cap",
    "radix-sched",
    "radix-ipc",
    "radix-block",
    "radix-vfs",
    "radix-fat",
    "radix-exfat",
    "radix-ext4",
    "radix-ahci",
    "radix-nvme",
    "radix-net",
];

/// `ob test kani`: checks the Kani proof harnesses. They take minutes each,
/// so plain `ob test` leaves them to this suite (and to their CI job).
fn kani(ctx: &Ctx) -> Result<()> {
    let installed = doctor::kani_version();
    match installed.as_deref() {
        Some(version) if version == doctor::KANI_VERSION => {}
        Some(version) => {
            return Err(Error::new(format!(
                "Kani {version} is installed, but the proofs are checked with {}",
                doctor::KANI_VERSION
            ))
            .hint(doctor::kani_install()));
        }
        None => return Err(Error::new("Kani is not installed").hint(doctor::kani_install())),
    }
    for library in PROVED_LIBRARIES {
        step(&format!("Checking the Kani proofs of {library}"));
        let mut cmd = kernel::cargo(ctx, "kani")?;
        // Kani builds with its own toolchain, so it gets a directory of its
        // own instead of sharing the kernel's.
        cmd.arg("--target-dir").arg(ctx.target.join("kani"));
        cmd.args(["-p", library, "--output-format", "terse"]);
        process::run(ctx, &mut cmd)?;
    }
    Ok(())
}

/// Made-up microcode: an update for the emulated CPU and one for another.
pub fn test_microcode() -> Vec<u8> {
    let mut file = vec![0u8; 4096];
    let mut len = 0;
    for (revision, signature, platforms) in
        [(MICROCODE_REVISION, MICROCODE_SIGNATURE, 0xff), (0x10, 0x806ea, 0xc0)]
    {
        len += radix_microcode::build::update(
            revision,
            signature,
            platforms,
            16,
            &[],
            &mut file[len..],
        );
    }
    file.truncate(len);
    file
}

/// The kernel and user programs a test image holds.
pub struct Built {
    pub kernel: PathBuf,
    pub programs: userspace::Programs,
}

/// Builds a test image whose kernel command line is a placeholder slot.
pub fn build_test_image(ctx: &Ctx, built: &Built, dir: &Path, kind: TestImage) -> Result<Vec<u8>> {
    let name = match kind {
        TestImage::Plain => "dukovo-test",
        TestImage::Microcode => "dukovo-test-microcode",
    };
    let output = dir.join(format!("{name}.img"));
    image::build(
        ctx,
        &ImageSpec {
            kernel: &built.kernel,
            timeout: 0,
            cmdline: &cmdline_slot(),
            microcode: (kind == TestImage::Microcode).then(test_microcode),
            programs: Some(&built.programs),
            disk_check: None,
            root: None,
            staging: dir.join(format!("{name}-root")),
            output: output.clone(),
        },
    )?;
    std::fs::read(&output).map_err(|err| Error::new(format!("reading {}: {err}", output.display())))
}

/// The placeholder for the kernel command line in the test images.
fn cmdline_slot() -> String {
    format!("{:-<CMDLINE_SLOT$}", "ob-test-cmdline")
}

/// A copy of a test image with `cmdline` in its command-line slot.
pub fn with_cmdline(image: &[u8], cmdline: &str) -> Result<Vec<u8>, String> {
    let slot = cmdline_slot();
    let found: Vec<usize> = image
        .windows(slot.len())
        .enumerate()
        .filter(|(_, window)| *window == slot.as_bytes())
        .map(|(at, _)| at)
        .collect();
    let [at] = found[..] else {
        return Err(format!(
            "the test image holds the command-line slot {} times, not once",
            found.len()
        ));
    };
    if cmdline.len() > CMDLINE_SLOT {
        return Err(format!("the command line is longer than {CMDLINE_SLOT} bytes"));
    }
    // Padded with spaces by bytes: a character may take more than one.
    let mut copy = image.to_vec();
    let field = &mut copy[at..at + CMDLINE_SLOT];
    field.fill(b' ');
    field[..cmdline.len()].copy_from_slice(cmdline.as_bytes());
    Ok(copy)
}

fn boot_tests(ctx: &Ctx, built: &Built, cases: &[&Case], timeout: Duration) -> Result<()> {
    qemu::require_qemu()?;
    let dir = ctx.out.join("test");
    process::create_dir(&dir)?;
    let plain = build_test_image(ctx, built, &dir, TestImage::Plain)?;
    let microcode = if cases.iter().any(|case| case.image == TestImage::Microcode) {
        Some(build_test_image(ctx, built, &dir, TestImage::Microcode)?)
    } else {
        None
    };

    let root = if cases.iter().any(|case| case.disks.iter().any(|disk| disk.fat == ROOT)) {
        let path = dir.join("root.btrfs");
        image::make_root(ctx, &built.programs.bootfs, &dir.join("root-staging"), &path, None)?;
        Some(path)
    } else {
        None
    };

    let kvm = qemu::kvm_available();
    step(&format!(
        "Running {} boot tests in QEMU ({})",
        cases.len(),
        if kvm { "KVM" } else { "no KVM, so each boot takes longer" }
    ));
    let mut failures = 0;
    for case in cases {
        let base = match case.image {
            TestImage::Plain => &plain,
            TestImage::Microcode => microcode.as_ref().expect("built when a case needs it"),
        };
        let started = Instant::now();
        let outcome = run_case(ctx, case, base, root.as_deref(), &dir, kvm, timeout);
        let secs = started.elapsed().as_secs_f32();
        match outcome {
            Ok(()) => eprintln!("    PASS  {:<15} {} ({secs:.1} s)", case.name, case.about),
            Err(reason) => {
                failures += 1;
                // A case that failed after its reboot failed in the second
                // boot, whose log is its own.
                let name = if reason.starts_with(AFTER_THE_REBOOT) {
                    format!("{}-reboot", case.name)
                } else {
                    case.name.to_string()
                };
                let log = dir.join(format!("boot-{name}.log"));
                eprintln!("    FAIL  {:<15} {} ({secs:.1} s)", case.name, case.about);
                eprintln!("          {reason}");
                eprintln!("          serial log: {}", ctx.display(&log));
                let text = clean_log(&std::fs::read_to_string(&log).unwrap_or_default());
                for line in failure_lines(&text) {
                    eprintln!("          | {line}");
                }
            }
        }
    }
    if let Some(path) = &root {
        let _ = std::fs::remove_file(path);
    }
    if failures > 0 {
        crate::bail!("{failures} of {} boot tests failed", cases.len());
    }
    step(&format!("All {} boot tests passed", cases.len()));
    Ok(())
}

/// How the reason a case's second boot failed starts.
const AFTER_THE_REBOOT: &str = "after the reboot";

fn run_case(
    ctx: &Ctx,
    case: &Case,
    base: &[u8],
    root: Option<&Path>,
    dir: &Path,
    kvm: bool,
    timeout: Duration,
) -> Result<(), String> {
    let image = dir.join(format!("{}.img", case.name));
    let qmp = dir.join(format!("{}.qmp", case.name));
    let disks = make_disks(ctx, dir, case, base, root)?;
    let mut extra: Vec<String> = case.qemu.iter().map(ToString::to_string).collect();
    for arg in &mut extra {
        let (name, bytes) = match arg.as_str() {
            POWER_TABLE => ("power.aml", POWER_AML.to_vec()),
            THERMAL_TABLE => ("thermal.aml", THERMAL_AML.to_vec()),
            CROSEC_TABLE => ("crosec.aml", CROSEC_AML.to_vec()),
            GPIO_TABLE => ("gpio.aml", GPIO_AML.to_vec()),
            DDR4_DEVICE => ("ddr4.smbios", ddr4_device()),
            _ => continue,
        };
        let path = dir.join(name);
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        *arg = format!("file={}", qemu::option_value(&path));
    }
    if case.disks.iter().any(|disk| disk.bus == Bus::Sata) {
        extra.extend(["-device".into(), "ich9-ahci,id=sata".into()]);
    }
    if case.disks.iter().any(|disk| matches!(disk.bus, Bus::Usb(_) | Bus::Uas(..)))
        && case.media != Media::Usb
    {
        return Err("a case with USB disks boots from USB, whose controller they are on".into());
    }
    for port in uas_devices(case.disks) {
        extra.extend(["-device".into(), format!("usb-uas,bus=xhci.0,port={port},id=uas{port}")]);
    }
    // Each controller before its namespaces, in order, so that the
    // kernel finds them in that order.
    for n in nvme_controllers(case.disks) {
        extra.extend(["-device".into(), format!("nvme,id=nvme{n},serial=dukovo-test-{n}")]);
    }
    let mut namespaces = [0u32; 256];
    let mut port = 0;
    for (i, (disk, image)) in case.disks.iter().zip(&disks).enumerate() {
        let mut file = qemu::option_value(&image.path);
        if disk.errors.is_some() {
            let rules = image.path.with_extension("blkdebug");
            file = format!("blkdebug:{}:{file}", qemu::option_value(&rules));
        }
        // Discards reach the disk's file as holes, which read as zeros.
        extra.push("-drive".into());
        let ro = if disk.read_only { ",readonly=on" } else { "" };
        extra.push(format!("file={file},if=none,id=disk{i},format=raw,discard=unmap{ro}"));
        extra.push("-device".into());
        let device = match disk.sector {
            512 => disk.device.to_string(),
            size => format!(",logical_block_size={size},physical_block_size={size}{}", disk.device),
        };
        extra.push(match disk.bus {
            Bus::Virtio => format!("virtio-blk-pci,drive=disk{i}{device}"),
            Bus::Sata => {
                port += 1;
                format!("ide-hd,drive=disk{i},bus=sata.{}{device}", port - 1)
            }
            Bus::Nvme(n) => {
                namespaces[usize::from(n)] += 1;
                let nsid = namespaces[usize::from(n)];
                format!("nvme-ns,drive=disk{i},bus=nvme{n},nsid={nsid}{device}")
            }
            Bus::Usb(port) => {
                format!("usb-storage,drive=disk{i},bus=xhci.0,port={port},id=stick{i}{device}")
            }
            Bus::Uas(port, lun) => {
                format!("scsi-hd,drive=disk{i},bus=uas{port}.0,scsi-id=0,lun={lun}{device}")
            }
        });
    }
    let ovmf = match case.firmware {
        Firmware::Uefi => Some(
            qemu::prepare_ovmf(ctx, &format!("test-{}", case.name)).map_err(|e| e.to_string())?,
        ),
        Firmware::Bios => None,
    };
    let machine = Machine {
        firmware: case.firmware,
        media: case.media,
        ovmf,
        image: image.clone(),
        memory: case.memory.into(),
        cpus: case.cpus,
        kvm: kvm && case.cpu_model.is_none(),
        cpu_model: case.cpu_model.map(str::to_string),
        serial: Serial::Pipe,
        display: false,
        reboot: case.reboot,
        gdb: false,
        qmp: Some(qmp.clone()),
        extra,
        direct: None,
    };
    let boot = |name: &str, cmdline: &str, script: Script| -> Result<(), String> {
        std::fs::write(&image, with_cmdline(base, cmdline)?)
            .map_err(|err| format!("writing {}: {err}", image.display()))?;
        let _ = std::fs::remove_file(&qmp);
        let mut vm = Vm::start(ctx, &machine, dir, name, timeout)?;
        let result = script(&mut vm, case).and_then(|()| kernel_complaints(&vm, case));
        vm.stop();
        result?;
        let log = vm.log();
        disks.iter().try_for_each(|disk| (disk.check)(disk, &log))
    };
    let mut result = boot(case.name, case.cmdline, case.script);
    if let (Ok(()), Some(second)) = (&result, &case.second) {
        let name = format!("{}-reboot", case.name);
        result = (second.before)(case, &disks)
            .and_then(|()| boot(&name, second.cmdline, second.script))
            .map_err(|err| {
                let log = dir.join(format!("boot-{name}.log"));
                format!("{AFTER_THE_REBOOT}: {err} (serial log: {})", ctx.display(&log))
            });
    }
    let _ = std::fs::remove_file(&image);
    if result.is_ok() {
        for disk in &disks {
            let _ = std::fs::remove_file(&disk.path);
            let _ = std::fs::remove_file(disk.path.with_extension("blkdebug"));
        }
    }
    result
}

/// The first sector of a partitioned test disk's filesystem: 1 MiB in.
const PARTITION_START: u64 = 2048;

/// Makes the case's disks afresh in `dir`: with `mkfs.fat` or
/// `mkfs.btrfs`, filled with [`storage_big_byte`]'s bytes, or as the
/// test image `base` with the Dukovo root's filesystem in the file `root`
/// after its partitions; and the configuration files of their errors.
fn make_disks(
    ctx: &Ctx,
    dir: &Path,
    case: &Case,
    base: &[u8],
    root: Option<&Path>,
) -> Result<Vec<DiskImage>, String> {
    let mut made = Vec::new();
    let mut raid = Vec::new();
    for (i, disk) in case.disks.iter().enumerate() {
        let path = dir.join(format!("{}-disk{i}.img", case.name));
        let _ = std::fs::remove_file(&path);
        let image = DiskImage {
            path,
            offset: 0,
            bytes: disk.kib * 1024,
            sector: disk.sector,
            check: disk.check,
            btrfs: None,
        };
        if let Some(rules) = disk.errors {
            let at = image.path.with_extension("blkdebug");
            std::fs::write(&at, rules(i))
                .map_err(|err| format!("writing {}: {err}", at.display()))?;
        }
        if disk.fat == 0 {
            std::fs::write(&image.path, raw_bytes(image.bytes))
                .map_err(|err| format!("writing {}: {err}", image.path.display()))?;
            made.push(image);
            continue;
        }
        if disk.fat == SWAP {
            make_swap(disk, &image.path)?;
            made.push(image);
            continue;
        }
        if disk.fat == ROOT {
            let root = root.ok_or("the Dukovo root's filesystem was not made")?;
            std::fs::write(&image.path, base)
                .map_err(|err| format!("writing {}: {err}", image.path.display()))?;
            let first = image::append_root(&image.path, root).map_err(|err| err.to_string())?;
            let tool = btrfs::tool(ctx).map_err(|err| err.to_string())?;
            made.push(DiskImage {
                offset: first * 512,
                bytes: image::ROOT_BYTES,
                btrfs: Some(tool),
                ..image
            });
            continue;
        }
        if disk.btrfs() {
            let tool = btrfs::tool(ctx).map_err(|err| err.to_string())?;
            let image = DiskImage { btrfs: Some(tool), ..image };
            if disk.fat == BTRFS_LOG {
                let kept = ctx.root.join(fixture::FIXTURES).join("btrfs-log.sparse");
                std::fs::read(&kept)
                    .and_then(|bytes| fixture::write_sparse(&bytes, &image.path))
                    .map_err(|err| {
                    format!("writing {} from {}: {err}", image.path.display(), kept.display())
                })?;
                made.push(image);
                continue;
            }
            if disk.fat == BTRFS_RAID {
                // Made with the case's others, below.
                std::fs::File::create(&image.path)
                    .and_then(|file| file.set_len(image.bytes))
                    .map_err(|err| format!("making {}: {err}", image.path.display()))?;
                raid.push(image.path.clone());
                made.push(image);
                continue;
            }
            make_btrfs(ctx, disk, &image)?;
            made.push(image);
            continue;
        }
        if disk.exfat() {
            make_exfat(disk, &image.path)?;
            made.push(image);
            continue;
        }
        if disk.ext4() {
            make_ext4(disk, &image.path)?;
            made.push(image);
            continue;
        }
        let path = &image.path;
        let mut mkfs = std::process::Command::new(fat_tool("mkfs.fat"));
        // A fixed volume ID, so that runs match.
        mkfs.args(["-F", &disk.fat.to_string(), "-n", disk.label, "-i", "4f525448"]);
        mkfs.args(["-S", &disk.sector.to_string()]);
        if disk.sector != 512 {
            // A sector a cluster: of the sectors a cluster `mkfs.fat` picks
            // for the size, 4 KiB ones would leave FAT16 too few clusters.
            mkfs.args(["-s", "1"]);
            if disk.fat != 32 {
                // A root directory of whole sectors, which `mkfs.fat` leaves
                // a floppy's 224 entries short of: Linux refuses a volume
                // whose root directory ends inside a sector, and mtools
                // reads one otherwise than the specification.
                mkfs.args(["-r", "512"]);
            }
        }
        let offset = if disk.table == Table::None {
            mkfs.arg("-C").arg(path).arg(disk.kib.to_string());
            0
        } else {
            let file = std::fs::File::create(path)
                .map_err(|err| format!("making {}: {err}", path.display()))?;
            file.set_len(disk.kib * 1024)
                .map_err(|err| format!("sizing {}: {err}", path.display()))?;
            if disk.table == Table::Gpt {
                write_gpt(path, disk.kib * 2, disk.sector)?;
            } else {
                write_mbr(path, disk.kib * 2)?;
            }
            // In the filesystem's sectors.
            mkfs.args(["--offset", &(PARTITION_START * 512 / disk.sector).to_string()]);
            mkfs.arg(path).arg((partition_sectors(disk) / 2).to_string());
            PARTITION_START * 512
        };
        host_tool(&mut mkfs)?;
        made.push(DiskImage { offset, ..image });
    }
    if let Some(first) = case.disks.iter().find(|disk| disk.fat == BTRFS_RAID) {
        make_btrfs_raid(ctx, first.label, &raid)?;
    }
    Ok(made)
}

/// What a raw test disk holds before the test: [`storage_big_byte`]'s
/// bytes.
fn raw_bytes(bytes: u64) -> Vec<u8> {
    (0..bytes as usize).map(storage_big_byte).collect()
}

/// Checks that a raw disk holds what it held before the boot.
fn raw_unchanged(disk: &DiskImage, _log: &str) -> Result<(), String> {
    raw_same(disk, &raw_bytes(disk.bytes))
}

/// Checks a disk of the `disk-raw` test: what [`RAW_WRITES`] wrote, the
/// write at the end, zeros where [`RAW_DISCARD`] discarded, and the bytes
/// it held before everywhere else.
fn raw_check(disk: &DiskImage, _log: &str) -> Result<(), String> {
    let mut want = raw_bytes(disk.bytes);
    let end = (disk.bytes - 10, 10);
    for &(offset, len) in RAW_WRITES.iter().chain([&end]) {
        for byte in &mut want[offset as usize..(offset + len) as usize] {
            *byte = !*byte;
        }
    }
    let (offset, len) = RAW_DISCARD;
    want[offset as usize..(offset + len) as usize].fill(0);
    raw_same(disk, &want)
}

/// Checks that a raw disk holds `want`, naming the first byte that
/// differs.
fn raw_same(disk: &DiskImage, want: &[u8]) -> Result<(), String> {
    let got = std::fs::read(&disk.path)
        .map_err(|err| format!("reading {}: {err}", disk.path.display()))?;
    match got.iter().zip(want).position(|(a, b)| a != b) {
        Some(at) => Err(format!(
            "{}: byte {at} is {:#04x}, not {:#04x}",
            disk.path.display(),
            got[at],
            want[at]
        )),
        None if got.len() != want.len() => {
            Err(format!("{} is {} bytes, not {}", disk.path.display(), got.len(), want.len()))
        }
        None => Ok(()),
    }
}

/// The size in sectors of a test disk's partition, which [`Table`] says.
fn partition_sectors(disk: &Disk) -> u64 {
    match disk.table {
        Table::None => 0,
        Table::Mbr => disk.kib * 2 - PARTITION_START,
        Table::Gpt => gpt_last(disk.kib * 2, disk.sector) + 1 - PARTITION_START,
    }
}

/// The name of a test disk's GPT partition.
const GPT_NAME: &str = "Dukovo test data";

/// The last sector of the GPT partition of a disk of `sectors` and of
/// logical blocks of `block` bytes: before the backup table (33 blocks of
/// 512 bytes, or 5 of 4 KiB), at the end of a whole MiB.
fn gpt_last(sectors: u64, block: u64) -> u64 {
    let backup = (1 + GPT_ENTRIES_BYTES / block) * (block / 512);
    (sectors - backup) / 2048 * 2048 - 1
}

/// The size of a GPT's 128 entries of 128 bytes.
const GPT_ENTRIES_BYTES: u64 = 128 * 128;

/// A GPT disk's tables (UEFI 2.10, section 5.3), for a disk of `sectors`
/// whose logical blocks, which a GPT counts in, are of `block` bytes: the
/// protective MBR, the primary header and its 128 entries (the first
/// blocks), and the backup entries and header (the last), with one basic
/// data partition from [`PARTITION_START`] to [`gpt_last`], named
/// [`GPT_NAME`], and fixed GUIDs so that runs match.
fn gpt_tables(sectors: u64, block: u64) -> (Vec<u8>, Vec<u8>) {
    let per_block = block / 512;
    let blocks = sectors / per_block;
    let table = GPT_ENTRIES_BYTES / block;
    let mut mbr = vec![0u8; block as usize];
    let entry = &mut mbr[446..462];
    entry[4] = 0xee;
    entry[8..12].copy_from_slice(&1u32.to_le_bytes());
    entry[12..16].copy_from_slice(&u32::try_from(blocks - 1).unwrap_or(u32::MAX).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xaa;
    let mut entries = vec![0u8; GPT_ENTRIES_BYTES as usize];
    // The basic data partition type, EBD0A0A2-B9E5-4433-87C0-68B6B72699C7,
    // in its mixed-endian byte order, and a unique GUID of the test's own.
    entries[0..16].copy_from_slice(&[
        0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99,
        0xc7,
    ]);
    entries[16..32].copy_from_slice(b"Dukovo test part");
    entries[32..40].copy_from_slice(&(PARTITION_START / per_block).to_le_bytes());
    entries[40..48]
        .copy_from_slice(&((gpt_last(sectors, block) + 1) / per_block - 1).to_le_bytes());
    for (i, c) in GPT_NAME.encode_utf16().enumerate() {
        entries[56 + 2 * i..58 + 2 * i].copy_from_slice(&c.to_le_bytes());
    }
    let entries_crc = crc32(&entries);
    let header = |at: u64, alternate: u64, entries_at: u64| {
        let mut h = vec![0u8; block as usize];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&at.to_le_bytes());
        h[32..40].copy_from_slice(&alternate.to_le_bytes());
        h[40..48].copy_from_slice(&(2 + table).to_le_bytes());
        h[48..56].copy_from_slice(&(blocks - 2 - table).to_le_bytes());
        h[56..72].copy_from_slice(b"Dukovo test disk");
        h[72..80].copy_from_slice(&entries_at.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };
    let mut primary = mbr;
    primary.extend(header(1, blocks - 1, 2));
    primary.extend(&entries);
    let mut backup = entries;
    backup.extend(header(blocks - 1, 1, blocks - 1 - table));
    (primary, backup)
}

/// Writes [`gpt_tables`] to a disk of `sectors` and of logical blocks of
/// `block` bytes.
fn write_gpt(path: &Path, sectors: u64, block: u64) -> Result<(), String> {
    let (primary, backup) = gpt_tables(sectors, block);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|err| format!("opening {}: {err}", path.display()))?;
    file.write_all(&primary).map_err(|err| format!("writing {}: {err}", path.display()))?;
    file.seek(std::io::SeekFrom::Start(sectors * 512 - backup.len() as u64))
        .and_then(|_| file.write_all(&backup))
        .map_err(|err| format!("writing {}: {err}", path.display()))
}

/// Checks a GPT disk: both of its tables as [`write_gpt`] wrote them, and
/// its filesystem with [`fsck`].
fn fsck_gpt(disk: &DiskImage, _log: &str) -> Result<(), String> {
    let bytes = std::fs::read(&disk.path).map_err(|err| format!("reading a disk: {err}"))?;
    let (primary, backup) = gpt_tables(disk.bytes / 512, disk.sector);
    let end = bytes.len();
    if bytes.len() as u64 != disk.bytes
        || bytes[..primary.len()] != primary[..]
        || bytes[end - backup.len()..] != backup[..]
    {
        return Err(format!("{}: its GPT is not the one written", disk.path.display()));
    }
    fsck(disk, "")
}

/// Writes an MBR with one FAT32 (LBA) partition from [`PARTITION_START`]
/// to the end of a disk of `sectors`.
fn write_mbr(path: &Path, sectors: u64) -> Result<(), String> {
    let mut mbr = [0u8; 512];
    let entry = &mut mbr[446..462];
    entry[4] = 0x0c;
    entry[8..12].copy_from_slice(&(PARTITION_START as u32).to_le_bytes());
    entry[12..16].copy_from_slice(&((sectors - PARTITION_START) as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xaa;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|err| format!("opening {}: {err}", path.display()))?;
    file.write_all(&mbr).map_err(|err| format!("writing {}: {err}", path.display()))
}

/// Where a dosfstools program is: in `/usr/sbin` or `/sbin`, which a
/// user's PATH may leave out, or else wherever PATH finds it.
fn fat_tool(name: &str) -> PathBuf {
    ["/usr/sbin", "/sbin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Makes a swap area of `disk`'s size at `path` with the host's `mkswap`,
/// with a fixed UUID so that runs match.
fn make_swap(disk: &Disk, path: &Path) -> Result<(), String> {
    let file =
        std::fs::File::create(path).map_err(|err| format!("making {}: {err}", path.display()))?;
    file.set_len(disk.kib * 1024).map_err(|err| format!("sizing {}: {err}", path.display()))?;
    drop(file);
    host_tool(
        std::process::Command::new(fat_tool("mkswap"))
            .args(["-q", "-L", disk.label, "-U", SWAP_UUID])
            .arg(path),
    )
    .map(drop)
}

/// Runs a host program on a test disk; fails with what it said.
fn host_tool(cmd: &mut std::process::Command) -> Result<Vec<u8>, String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let output = cmd.env("MTOOLS_SKIP_CHECK", "1").output().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            format!("`{program}` is not installed; run `./ob doctor` to see what is missing")
        } else {
            format!("cannot run `{program}`: {err}")
        }
    })?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        return Err(format!("`{program}` failed ({}): {}", output.status, said.trim()));
    }
    Ok(output.stdout)
}

/// Makes an exFAT test disk with exfatprogs' `mkfs.exfat`.
fn make_exfat(disk: &Disk, path: &Path) -> Result<(), String> {
    let file =
        std::fs::File::create(path).map_err(|err| format!("making {}: {err}", path.display()))?;
    file.set_len(disk.kib * 1024).map_err(|err| format!("sizing {}: {err}", path.display()))?;
    drop(file);
    let mut mkfs = std::process::Command::new(fat_tool("mkfs.exfat"));
    mkfs.args(["-q", "-L", disk.label, "-c", &disk.cluster.to_string()]).arg(path);
    host_tool(&mut mkfs).map(drop)
}

/// Checks an exFAT disk with exfatprogs' `fsck.exfat`, changing nothing,
/// and that the volume is not left marked dirty, which `fsck.exfat` does
/// not mind: the program syncs what it wrote, and reads write nothing.
fn exfat_checked(disk: &DiskImage, _log: &str) -> Result<(), String> {
    host_tool(std::process::Command::new(fat_tool("fsck.exfat")).arg("-n").arg(&disk.path))
        .map_err(|err| format!("{} fails exfatprogs' fsck.exfat: {err}", disk.path.display()))?;
    let mut flags = [0u8; 2];
    std::fs::File::open(&disk.path)
        .and_then(|mut file| {
            file.seek(std::io::SeekFrom::Start(106))?;
            file.read_exact(&mut flags)
        })
        .map_err(|err| format!("reading {}: {err}", disk.path.display()))?;
    // VolumeFlags' VolumeDirty bit.
    if u16::from_le_bytes(flags) & 2 != 0 {
        return Err(format!("{} was left marked dirty", disk.path.display()));
    }
    Ok(())
}

/// Makes an ext4 disk with `mkfs.ext4`, holding only the host's file
/// ([`STORAGE_FROM_HOST_NAME`]): no tool on Linux writes a file to ext4
/// without mounting it, which takes the administrator, but `mkfs.ext4 -d`
/// copies a directory in. A fixed UUID, so that runs match.
fn make_ext4(disk: &Disk, path: &Path) -> Result<(), String> {
    let tree = path.with_extension("tree");
    if tree.exists() {
        std::fs::remove_dir_all(&tree)
            .map_err(|err| format!("removing {}: {err}", tree.display()))?;
    }
    std::fs::create_dir(&tree).map_err(|err| format!("making {}: {err}", tree.display()))?;
    let host = tree.join(STORAGE_FROM_HOST_NAME);
    std::fs::write(&host, STORAGE_FROM_HOST)
        .map_err(|err| format!("writing {}: {err}", host.display()))?;
    let file =
        std::fs::File::create(path).map_err(|err| format!("making {}: {err}", path.display()))?;
    file.set_len(disk.kib * 1024).map_err(|err| format!("sizing {}: {err}", path.display()))?;
    drop(file);
    let mut mkfs = std::process::Command::new(fat_tool("mkfs.ext4"));
    mkfs.args(["-q", "-F", "-L", disk.label, "-b", &disk.cluster.to_string()]);
    mkfs.args(["-U", "4f525448-0000-4000-8000-000000000001", "-E", "root_owner=0:0", "-d"]);
    let made = host_tool(mkfs.arg(&tree).arg(path)).map(drop);
    let _ = std::fs::remove_dir_all(&tree);
    made
}

/// Checks an ext4 disk with e2fsprogs' `e2fsck`, changing nothing, both
/// as it is and with its journal replayed as Linux would at its next
/// mount: Radix keeps the journal's last transaction there while the
/// volume is mounted, as Linux does, and after a sync its blocks are home
/// too.
fn ext4_checked(disk: &DiskImage, _log: &str) -> Result<(), String> {
    ext4_fsck(disk, &disk.path)?;
    ext4_replayed_fsck(disk)
}

/// Checks an ext4 disk with `e2fsck` after its journal is replayed only:
/// cut off while a commit's blocks were going home, as the disk is after
/// a power cut, it is whole only so, on Linux as on Radix.
fn ext4_replayed_fsck(disk: &DiskImage) -> Result<(), String> {
    let replayed = ext4_replayed(disk)?;
    let result = ext4_fsck(disk, &replayed);
    let _ = std::fs::remove_file(&replayed);
    result
}

/// Runs `e2fsck -fn` on `path`, the image of `disk` or a copy of it.
fn ext4_fsck(disk: &DiskImage, path: &Path) -> Result<(), String> {
    host_tool(std::process::Command::new(fat_tool("e2fsck")).arg("-fn").arg(path))
        .map(drop)
        .map_err(|err| format!("{} fails e2fsprogs' e2fsck: {err}", disk.path.display()))
}

/// A copy of an ext4 disk beside it, its journal replayed by `e2fsck` as
/// Linux would at its next mount, and nothing else changed.
fn ext4_replayed(disk: &DiskImage) -> Result<PathBuf, String> {
    let copy = disk.path.with_extension("replayed");
    std::fs::copy(&disk.path, &copy)
        .map_err(|err| format!("copying {}: {err}", disk.path.display()))?;
    let mut e2fsck = std::process::Command::new(fat_tool("e2fsck"));
    if let Err(err) = host_tool(e2fsck.args(["-y", "-E", "journal_only"]).arg(&copy)) {
        let _ = std::fs::remove_file(&copy);
        return Err(format!("{}: e2fsck cannot replay its journal: {err}", disk.path.display()));
    }
    Ok(copy)
}

/// An ext4 disk's files, copied out beside it by `debugfs` (`rdump`) after
/// its journal is replayed, but for `lost+found`.
fn ext4_dump(disk: &DiskImage) -> Result<PathBuf, String> {
    let replayed = ext4_replayed(disk)?;
    let to = disk.path.with_extension("restored");
    let result = (|| {
        if to.exists() {
            std::fs::remove_dir_all(&to)
                .map_err(|err| format!("removing {}: {err}", to.display()))?;
        }
        std::fs::create_dir(&to).map_err(|err| format!("making {}: {err}", to.display()))?;
        let mut debugfs = std::process::Command::new(fat_tool("debugfs"));
        debugfs.arg("-R").arg(format!("rdump / {}", to.display())).arg(&replayed);
        host_tool(debugfs.stdin(Stdio::null()))?;
        std::fs::remove_dir_all(to.join("lost+found"))
            .map_err(|err| format!("{}: no lost+found: {err}", disk.path.display()))
    })();
    let _ = std::fs::remove_file(&replayed);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&to);
    }
    result.map(|()| to)
}

/// Checks a disk's filesystem with `fsck.fat`, changing nothing: a
/// partition is copied out first, as `fsck.fat` reads whole files only.
fn fsck(disk: &DiskImage, _log: &str) -> Result<(), String> {
    let checked = if disk.offset == 0 {
        disk.path.clone()
    } else {
        let bytes = std::fs::read(&disk.path).map_err(|err| format!("reading a disk: {err}"))?;
        let part = disk.path.with_extension("part");
        std::fs::write(&part, &bytes[disk.offset as usize..])
            .map_err(|err| format!("writing {}: {err}", part.display()))?;
        part
    };
    let result =
        host_tool(std::process::Command::new(fat_tool("fsck.fat")).arg("-n").arg(&checked));
    if checked != disk.path {
        let _ = std::fs::remove_file(&checked);
    }
    result.map(drop).map_err(|err| format!("{} is damaged: {err}", disk.path.display()))
}

/// The mtools name of a test disk's filesystem.
fn mtools_image(disk: &DiskImage) -> String {
    if disk.offset == 0 {
        disk.path.display().to_string()
    } else {
        format!("{}@@{}", disk.path.display(), disk.offset)
    }
}

/// A running test machine.
pub struct Vm {
    child: Child,
    stdin: Option<ChildStdin>,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
    qmp: Option<PathBuf>,
    /// End of the last match in the cleaned log.
    cursor: usize,
    deadline: Instant,
    timeout: Duration,
    stderr_log: PathBuf,
    screenshot: PathBuf,
    /// Whether QEMU runs the guest with KVM.
    kvm: bool,
}

impl Vm {
    pub fn start(
        ctx: &Ctx,
        machine: &Machine,
        dir: &Path,
        name: &str,
        timeout: Duration,
    ) -> Result<Vm, String> {
        let log_path = dir.join(format!("boot-{name}.log"));
        let stderr_log = dir.join(format!("boot-{name}.qemu.log"));
        let stderr = std::fs::File::create(&stderr_log)
            .map_err(|err| format!("creating {}: {err}", stderr_log.display()))?;
        let mut log = std::fs::File::create(&log_path)
            .map_err(|err| format!("creating {}: {err}", log_path.display()))?;
        let mut cmd = machine.command();
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(stderr);
        if ctx.verbose {
            eprintln!("+ {}", process::render(&cmd));
        }
        let mut child = cmd.spawn().map_err(|err| format!("cannot start QEMU: {err}"))?;
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = stdout.read(&mut buf) {
                let _ = log.write_all(&buf[..n]);
                sink.lock().expect("the log lock is not poisoned").extend_from_slice(&buf[..n]);
            }
        });
        Ok(Vm {
            stdin: child.stdin.take(),
            child,
            output,
            reader: Some(reader),
            qmp: machine.qmp.clone(),
            cursor: 0,
            deadline: Instant::now() + timeout,
            timeout,
            stderr_log,
            screenshot: dir.join(format!("{name}.ppm")),
            kvm: machine.kvm,
        })
    }

    /// The serial output so far, cleaned of escape sequences.
    pub fn log(&self) -> String {
        clean_log(&String::from_utf8_lossy(&self.output.lock().expect("not poisoned")))
    }

    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    fn qemu_stderr(&self) -> String {
        let text = std::fs::read_to_string(&self.stderr_log).unwrap_or_default();
        let text = text.trim();
        if text.is_empty() { String::new() } else { format!("; QEMU said: {text}") }
    }

    /// Waits until `needle` appears after the previous match.
    pub fn expect(&mut self, needle: &str) -> Result<(), String> {
        loop {
            let log = self.log();
            if let Some(at) = log.get(self.cursor..).and_then(|rest| rest.find(needle)) {
                self.cursor += at + needle.len();
                return Ok(());
            }
            if let Some(status) = self.exited() {
                // Let the reader collect the last output, then look again.
                if let Some(reader) = self.reader.take() {
                    let _ = reader.join();
                    continue;
                }
                return Err(format!(
                    "QEMU exited ({status}) before printing \"{needle}\"{}",
                    self.qemu_stderr()
                ));
            }
            if Instant::now() >= self.deadline {
                return Err(format!("no \"{needle}\" within {} s", self.timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until `needle` appears anywhere in the log, for what a thread
    /// that runs alongside the boot says in an order of its own. The place
    /// [`Vm::expect`] reads from stays where it is.
    pub fn expect_anywhere(&mut self, needle: &str) -> Result<(), String> {
        let cursor = self.cursor;
        self.cursor = 0;
        let found = self.expect(needle);
        self.cursor = cursor;
        found
    }

    /// Waits until `needle` appears after the previous match, but leaves
    /// the place [`Vm::expect`] reads from where it was: for a line that
    /// may come after what another CPU printed meanwhile, which the next
    /// expectations read. Spark says it started a program once the program
    /// runs, and on another CPU the program may have printed lines by then.
    pub fn expect_ahead(&mut self, needle: &str) -> Result<(), String> {
        let cursor = self.cursor;
        let found = self.expect(needle);
        self.cursor = cursor;
        found
    }

    /// Types `text` on the serial port.
    pub fn send(&mut self, text: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("QEMU's input is closed")?;
        // A terminal sends what is typed a byte at a time; so does this, a
        // little apart, so the guest's 16-byte receive FIFO never overflows.
        for byte in text.bytes() {
            stdin.write_all(&[byte]).and_then(|()| stdin.flush()).map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    /// Presses keys on the PS/2 keyboard through QMP's `send-key`: each
    /// of `combos` is QEMU key codes held together, joined by `-`, such as
    /// `alt-f2` or `shift-a`.
    pub fn press(&mut self, combos: &[&str]) -> Result<(), String> {
        let socket = self.qmp.clone().ok_or("no QMP socket")?;
        let mut qmp = Qmp::connect(&socket)?;
        for combo in combos {
            let keys: Vec<String> = combo
                .split('-')
                .map(|key| format!(r#"{{"type":"qcode","data":"{key}"}}"#))
                .collect();
            qmp.execute(&format!(
                r#"{{"execute":"send-key","arguments":{{"keys":[{}],"hold-time":20}}}}"#,
                keys.join(",")
            ))?;
            // The next key after this one's release, as a typist would.
            std::thread::sleep(Duration::from_millis(40));
        }
        Ok(())
    }

    /// Sends QEMU input events (QMP's InputEvent, as JSON) to the devices
    /// bound to the display `display`, through QMP's `input-send-event`;
    /// `send-key` reaches only the devices bound to none, the PS/2 ones.
    pub fn input_events(&mut self, display: &str, events: &[String]) -> Result<(), String> {
        let socket = self.qmp.clone().ok_or("no QMP socket")?;
        Qmp::connect(&socket)?.execute(&format!(
            r#"{{"execute":"input-send-event","arguments":{{"device":"{display}","events":[{}]}}}}"#,
            events.join(",")
        ))
    }

    /// Holds down, or lets go of, `keys` (QEMU key codes) on the keyboard
    /// bound to `display`.
    pub fn keys_on(&mut self, display: &str, keys: &[&str], down: bool) -> Result<(), String> {
        let events: Vec<String> = keys
            .iter()
            .map(|key| {
                format!(
                    r#"{{"type":"key","data":{{"down":{down},"key":{{"type":"qcode","data":"{key}"}}}}}}"#
                )
            })
            .collect();
        self.input_events(display, &events)
    }

    /// Types `text` on the keyboard bound to `display`, a key at a time on
    /// a US layout, as [`Vm::type_text`] does on the PS/2 one.
    pub fn type_on(&mut self, display: &str, text: &str) -> Result<(), String> {
        for c in text.chars() {
            let combo = key_combo(c)?;
            let keys: Vec<&str> = combo.split('-').collect();
            self.keys_on(display, &keys, true)?;
            std::thread::sleep(Duration::from_millis(20));
            let up: Vec<&str> = keys.iter().rev().copied().collect();
            self.keys_on(display, &up, false)?;
            std::thread::sleep(Duration::from_millis(40));
        }
        Ok(())
    }

    /// Gives QEMU the raw disk image `file` as the block node `node`, for
    /// a device plugged in with it: QMP's `blockdev-add`. The file is not
    /// locked: QEMU lets go of a device's file a while after it is
    /// unplugged, and the same file may be plugged in again before.
    pub fn add_disk_file(&mut self, node: &str, file: &Path) -> Result<(), String> {
        let socket = self.qmp.clone().ok_or("no QMP socket")?;
        Qmp::connect(&socket)?.execute(&format!(
            r#"{{"execute":"blockdev-add","arguments":{{"driver":"raw","node-name":{},"file":{{"driver":"file","filename":{},"locking":"off"}}}}}}"#,
            crate::bench::json_string(node),
            crate::bench::json_string(&file.display().to_string())
        ))
    }

    /// The directory of the run's logs, and of its cases' disks.
    pub fn dir(&self) -> &Path {
        self.stderr_log.parent().unwrap_or(Path::new("."))
    }

    /// Presses the machine's power button: QMP's `system_powerdown`, which
    /// raises ACPI's power button event.
    pub fn press_power_button(&mut self) -> Result<(), String> {
        let socket = self.qmp.clone().ok_or("no QMP socket")?;
        Qmp::connect(&socket)?.execute(r#"{"execute":"system_powerdown"}"#)
    }

    /// Runs a command of QEMU's human monitor. QEMU prints nothing for
    /// the commands the tests use unless it refuses them, so anything
    /// printed is an error.
    pub fn monitor(&mut self, command: &str) -> Result<(), String> {
        let socket = self.qmp.clone().ok_or("no QMP socket")?;
        let reply = Qmp::connect(&socket)?.execute_reply(&format!(
            r#"{{"execute":"human-monitor-command","arguments":{{"command-line":{}}}}}"#,
            crate::bench::json_string(command)
        ))?;
        match reply.trim() {
            r#"{"return": ""}"# => Ok(()),
            other => Err(format!("QEMU's monitor, asked to `{command}`: {other}")),
        }
    }

    /// Runs a human monitor command with `cpu` as the monitor's CPU, and
    /// returns what it printed.
    pub fn monitor_output(&mut self, command: &str, cpu: u32) -> Result<String, String> {
        let socket = self.qmp.clone().ok_or("no QMP socket")?;
        let reply = Qmp::connect(&socket)?.execute_reply(&format!(
            r#"{{"execute":"human-monitor-command","arguments":{{"command-line":{},"cpu-index":{cpu}}}}}"#,
            crate::bench::json_string(command)
        ))?;
        returned_string(&reply)
            .ok_or_else(|| format!("QEMU's monitor, asked to `{command}`: {}", reply.trim()))
    }

    /// Types `text` on the PS/2 keyboard, a key at a time on a US layout.
    pub fn type_text(&mut self, text: &str) -> Result<(), String> {
        let combos = text.chars().map(key_combo).collect::<Result<Vec<_>, _>>()?;
        self.press(&combos.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// Runs a `kdb` command and waits for each of `expected`, then for the
    /// next prompt.
    pub fn kdb(&mut self, command: &str, expected: &[&str]) -> Result<(), String> {
        self.send(command)?;
        self.send("\r")?;
        for needle in expected {
            self.expect(needle).map_err(|err| format!("kdb `{command}`: {err}"))?;
        }
        self.expect("kdb> ").map_err(|err| format!("kdb `{command}`: {err}"))
    }

    /// Waits for QEMU to exit and checks its status.
    pub fn expect_exit(&mut self, code: i32) -> Result<(), String> {
        loop {
            if let Some(status) = self.exited() {
                return match status.code() {
                    Some(c) if c == code => Ok(()),
                    Some(QEMU_EXIT_PANIC) => Err("the kernel panicked".into()),
                    _ => Err(format!("QEMU exited with {status}{}", self.qemu_stderr())),
                };
            }
            if Instant::now() >= self.deadline {
                return Err(format!("QEMU still running after {} s", self.timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until the framebuffer console shows each of `needles`. The
    /// serial port gets every message before the screen does, and drawing
    /// is slow under emulation, so this takes screenshots until the screen
    /// catches up or 15 seconds pass. It waits as long for the console's
    /// size in the log, which a line of another service's may come before.
    pub fn expect_screen(&mut self, needles: &[&str]) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if screen::Geometry::from_log(&self.log()).is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let screen = self.screen()?;
            match screen_shows(&screen, needles) {
                Ok(()) => return Ok(()),
                Err(err) if Instant::now() >= deadline => return Err(err),
                Err(_) => std::thread::sleep(Duration::from_millis(500)),
            }
        }
    }

    /// Waits until no line of the framebuffer console shows `needle`, for
    /// up to 15 seconds.
    pub fn expect_screen_without(&mut self, needle: &str) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let screen = self.screen()?;
            if !screen.iter().any(|line| line.contains(needle)) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("the screen still shows \"{needle}\""));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Takes a screenshot and reads the framebuffer console's text.
    pub fn screen(&mut self) -> Result<Vec<String>, String> {
        let geometry = screen::Geometry::from_log(&self.log())
            .ok_or("the kernel did not report a framebuffer console")?;
        let socket = self.qmp.clone().ok_or("no QMP socket")?;
        let _ = std::fs::remove_file(&self.screenshot);
        let mut qmp = Qmp::connect(&socket)?;
        qmp.execute(&format!(
            r#"{{"execute":"screendump","arguments":{{"filename":{}}}}}"#,
            crate::bench::json_string(&self.screenshot.to_string_lossy())
        ))?;
        let bytes = std::fs::read(&self.screenshot)
            .map_err(|err| format!("reading {}: {err}", self.screenshot.display()))?;
        let image = screen::Image::parse_ppm(&bytes)?;
        screen::read_text(&image, geometry)
    }

    fn stop(&mut self) {
        if self.exited().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.stdin = None;
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // QEMU leaves its QMP socket behind.
        if let Some(socket) = &self.qmp {
            let _ = std::fs::remove_file(socket);
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A connection to QEMU's machine protocol.
struct Qmp {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Qmp {
    fn connect(path: &Path) -> Result<Qmp, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match UnixStream::connect(path) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(err) => return Err(format!("connecting to QMP at {}: {err}", path.display())),
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut qmp = Qmp { stream, reader };
        qmp.read_line()?; // the greeting
        qmp.execute(r#"{"execute":"qmp_capabilities"}"#)?;
        Ok(qmp)
    }

    fn read_line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Err("QMP closed the connection".into()),
            Ok(_) => Ok(line),
            Err(err) => Err(format!("reading from QMP: {err}")),
        }
    }

    /// Sends a command and waits for its reply, skipping events.
    fn execute(&mut self, command: &str) -> Result<(), String> {
        self.execute_reply(command).map(drop)
    }

    /// As [`Qmp::execute`], giving the reply's line.
    fn execute_reply(&mut self, command: &str) -> Result<String, String> {
        writeln!(self.stream, "{command}").map_err(|err| format!("writing to QMP: {err}"))?;
        loop {
            let line = self.read_line()?;
            if line.contains(r#""return""#) {
                return Ok(line);
            }
            if line.contains(r#""error""#) {
                return Err(format!("QMP: {}", line.trim()));
            }
        }
    }
}

/// The string a QMP reply returns, unescaped: `{"return": "..."}`.
fn returned_string(reply: &str) -> Option<String> {
    let (_, rest) = reply.split_once(r#""return": ""#)?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let code: String = chars.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&code, 16).ok()?)?);
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// The QEMU key codes (`send-key`'s qcodes) that type `c` on a US
/// keyboard, joined by `-`.
fn key_combo(c: char) -> Result<String, String> {
    const PLAIN: [(char, &str); 15] = [
        (' ', "spc"),
        ('\n', "ret"),
        ('\t', "tab"),
        ('-', "minus"),
        ('=', "equal"),
        ('[', "bracket_left"),
        (']', "bracket_right"),
        (';', "semicolon"),
        ('\'', "apostrophe"),
        ('`', "grave_accent"),
        ('\\', "backslash"),
        (',', "comma"),
        ('.', "dot"),
        ('/', "slash"),
        ('\u{8}', "backspace"),
    ];
    // Each shifted character and the key it is on.
    const SHIFTED: [(char, &str); 21] = [
        ('!', "1"),
        ('@', "2"),
        ('#', "3"),
        ('$', "4"),
        ('%', "5"),
        ('^', "6"),
        ('&', "7"),
        ('*', "8"),
        ('(', "9"),
        (')', "0"),
        ('_', "minus"),
        ('+', "equal"),
        ('{', "bracket_left"),
        ('}', "bracket_right"),
        (':', "semicolon"),
        ('"', "apostrophe"),
        ('~', "grave_accent"),
        ('|', "backslash"),
        ('<', "comma"),
        ('>', "dot"),
        ('?', "slash"),
    ];
    if c.is_ascii_lowercase() || c.is_ascii_digit() {
        return Ok(c.to_string());
    }
    if c.is_ascii_uppercase() {
        return Ok(format!("shift-{}", c.to_ascii_lowercase()));
    }
    if let Some((_, key)) = PLAIN.iter().find(|&&(p, _)| p == c) {
        return Ok(key.to_string());
    }
    match SHIFTED.iter().find(|&&(s, _)| s == c) {
        Some((_, key)) => Ok(format!("shift-{key}")),
        None => Err(format!("no key types {c:?}")),
    }
}

/// Removes carriage returns and terminal escape sequences from a serial log.
/// The lines of a failed case's serial log worth printing: its last 15,
/// or, when the kernel panicked, the whole panic report and the lines
/// that led to it, so that a failure on CI says what panicked without
/// the log, which only the job's artifacts keep.
fn failure_lines(log: &str) -> Vec<&str> {
    const TAIL: usize = 15;
    const BEFORE: usize = 10;
    const MOST: usize = 100;
    let lines: Vec<&str> = log.lines().collect();
    // The first report: the replay of one kept from the boot before has
    // the log's prefix in front of each line.
    let banner = lines.iter().position(|line| line.starts_with(PANIC_BANNER));
    let from = match banner {
        Some(banner) => banner.saturating_sub(BEFORE),
        None => lines.len().saturating_sub(TAIL),
    };
    lines[from..lines.len().min(from + MOST)].to_vec()
}

pub fn clean_log(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {}
            '\u{1b}' => {
                // CSI sequences end with a byte in 0x40..=0x7e.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// Fails unless some line of `screen` contains each of `needles`.
fn screen_shows(screen: &[String], needles: &[&str]) -> Result<(), String> {
    for needle in needles {
        if !screen.iter().any(|line| line.contains(needle)) {
            let shown: Vec<&str> =
                screen.iter().map(String::as_str).filter(|l| !l.is_empty()).collect();
            let tail = shown[shown.len().saturating_sub(8)..].join(" | ");
            return Err(format!("the screen does not show \"{needle}\"; its last lines: {tail}"));
        }
    }
    Ok(())
}

/// Checks the `Random:` line (after "Random: "). RDSEED and RDRAND must
/// pass their health tests when the CPU has them (`hardware`). Timing
/// jitter must pass under QEMU's emulation, whose counter always varies
/// enough. Under KVM the counter is the host's, and some hosts keep the
/// work's timing so steady that the jitter fails its test (about half the
/// samples were stuck on an AMD EPYC 9V45), so there a failed jitter is
/// accepted as long as the line says so. Mixing it in does no harm either
/// way (see "Seeding" in docs/design/random.md).
fn check_random(line: &str, hardware: bool, kvm: bool) -> Result<(), String> {
    let (passed, failed) = if hardware {
        (
            "from RDSEED, RDRAND and timing jitter (",
            "from RDSEED and RDRAND; failed its health test: timing jitter (",
        )
    } else {
        (
            "from timing jitter (",
            ", but no source passed its health test; failed its health test: timing jitter (",
        )
    };
    if line.contains(passed) || kvm && line.contains(failed) {
        return Ok(());
    }
    let or_failed = if kvm { format!(" or \"{failed}...)\"") } else { String::new() };
    Err(format!("the Random line should say \"{passed}...)\"{or_failed}, but it says: {line}"))
}

// ---- The test scripts ----

/// The checks every normal boot passes, in the order the kernel logs them,
/// up to the self-test (if any) that runs before "Boot complete.".
fn boot_report(vm: &mut Vm, case: &Case) -> Result<(), String> {
    vm.expect(&format!("Radix {VERSION} ({RELEASE} {VERSION})"))?;
    vm.expect(&format!("Firmware: {}", case.firmware))?;
    vm.expect("Memory: ")?;
    vm.expect(" MiB usable in ")?;
    vm.expect("Random: ChaCha20 seeded before KASLR")?;
    if case.cmdline.split_ascii_whitespace().any(|flag| flag == "nokaslr") {
        vm.expect("KASLR: off (nokaslr)")?;
    } else {
        vm.expect("KASLR: kernel moved by 0x")?;
    }
    // The whole `Random:` line has arrived now. The CPUs the cases use have
    // RDSEED and RDRAND unless the case emulates a model of its own.
    let log = vm.log();
    let random = log.split("Random: ").nth(1).and_then(|rest| rest.lines().next());
    check_random(random.unwrap_or_default(), case.cpu_model.is_none(), vm.kvm)?;
    vm.expect("Layout: kernel at 0x")?;
    vm.expect("Paging: W^X holds")?;
    vm.expect("CPU: ")?;
    vm.expect("ACPI: RSDP")?;
    vm.expect("ACPI:   APIC at ")?;
    vm.expect("Memory: frame allocator took ")?;
    vm.expect("APIC: ")?;
    vm.expect("Time: TSC at ")?;
    vm.expect(" in the frame allocator's zones:")?;
    vm.expect(
        "Scheduler: EEVDF fair class (3 ms slices) and FIFO and round-robin real-time classes",
    )?;
    let flags: Vec<&str> = case.cmdline.split_ascii_whitespace().collect();
    if flags.contains(&"lockdep") || flags.contains(&"lockdep=on") {
        vm.expect("Lock checker: on; every spin lock and mutex is checked")?;
    } else if flags.contains(&"lockdep=off") {
        vm.expect("Lock checker: off")?;
    } else {
        vm.expect("Lock checker: ")?;
    }
    vm.expect("Capabilities: ready; 21 object kinds, 13 rights")?;
    vm.expect("User mode: SYSCALL/SYSRET entry, user half ")?;
    vm.expect("Files: the root is a tmpfs; Spark mounts the rest")?;
    // Every test image carries the user test programs.
    vm.expect("User: the usertests image is ")?;
    let cpus = case.cpus;
    vm.expect(&format!("CPUs: {cpus} of {cpus} online:"))?;
    for cpu in 0..cpus {
        vm.expect(&format!("cpu{cpu}: APIC ID "))?;
    }
    vm.expect(&format!("Scheduler: {cpus} CPUs run threads, on "))?;
    // After the PCI scan (ADR 0200).
    if flags.contains(&"aml=off") {
        vm.expect("ACPI: the firmware's AML is not run (`aml=off`)")?;
    } else {
        vm.expect("ACPI: uACPI 7.0.0 runs the firmware's AML")?;
    }
    // QEMU's CPUs pick no speed of their own, even under KVM (ADR 0202).
    vm.expect(
        "CPU frequency: left to the firmware: the CPUs have neither Intel's HWP nor AMD's CPPC",
    )?;
    vm.expect(&format!("Timer: all {cpus} CPUs take timer interrupts at 100 Hz"))?;
    if cpus > 1 {
        vm.expect(&format!("IPI: all {} other CPUs answered a ping", cpus - 1))?;
    }
    Ok(())
}

/// Fails if the kernel logged a problem no case looks for: a lock-checker
/// report (but in the case that seeds them), a thread that slept where
/// it must not, or an error of uACPI's.
fn kernel_complaints(vm: &Vm, case: &Case) -> Result<(), String> {
    let log = vm.log();
    let complaint = log.lines().find(|line| {
        let checker = line.contains("Lock checker: ")
            && !line.contains("Lock checker: on; ")
            && !line.contains("Lock checker: off");
        (checker && case.name != "lockdep")
            || line.contains("with preemption held off")
            || line.contains("KiB kernel stack left")
            || line.contains("ACPI: error: ")
    });
    match complaint {
        Some(line) => Err(format!("the kernel complained: {}", line.trim())),
        None => Ok(()),
    }
}

/// Waits for the end of boot and checks that self-test `name` passed.
fn selftest_passed(vm: &mut Vm, name: &str) -> Result<(), String> {
    vm.expect("Boot complete.")?;
    let log = vm.log();
    if log.contains(&format!("selftest {name}: passed")) {
        return Ok(());
    }
    let prefix = format!("selftest {name}: ");
    let said: Vec<&str> = log.lines().filter(|line| line.contains(&prefix)).collect();
    Err(format!("selftest {name} did not pass: {}", said.join(" | ")))
}

fn boot(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    // KVM always offers x2APIC, which QEMU 8's TCG lacks, so with KVM these
    // cases cover the x2APIC path; make sure the kernel took it.
    if vm.kvm && !vm.log().contains("APIC: x2APIC mode") {
        return Err("the local APIC is not in x2APIC mode under KVM".into());
    }
    if case.cmdline.contains("nopcid") && !vm.log().contains("PCID off") {
        return Err("PCIDs are in use despite nopcid".into());
    }
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn boot_xapic(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    if !vm.log().contains("APIC: xAPIC mode") {
        return Err("the local APIC is not in xAPIC mode despite nox2apic".into());
    }
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn screen_log(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_screen(&["Timer: all 2 CPUs take timer interrupts", "Boot complete."])
}

/// A normal boot: Spark starts consoled, which takes the screen and shows
/// a shell on tty1. Types a command on the PS/2 keyboard and reads its
/// answer off the screen, switches to tty2, restarts consoled with
/// service(8), and powers the machine off from the shell.
fn consoles(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect("spark: Spark v2 is process ")?;
    vm.expect("spark: listening at /system/live/spark/control")?;
    vm.expect("spark: 7 services in /system/settings/spark/services, 7 enabled")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect("consoled: console of ")?;
    vm.expect("consoled: started /system/commands/nukesh on tty1 as process ")?;
    vm.expect("consoled: 6 consoles; tty1 is shown")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    vm.type_text("echo $((6 * 7)) consoles\n")?;
    vm.expect_screen(&["# echo $((6 * 7)) consoles", "42 consoles"])?;
    // notifyd's notifications on the bottom line (ADR 0212): one for its
    // time, a critical one until a key is pressed, and both in the list.
    vm.expect_anywhere("consoled: showing notifications from notifyd")?;
    vm.type_text("notify-send -t 4000 -a backup 'Backup done' 'All 3 disks'\n")?;
    vm.expect_screen(&["notify-send (backup): Backup done - All 3 disks"])?;
    vm.expect_screen_without("Backup done - All")?;
    vm.type_text("notify-send -u critical 'Disk full'\n")?;
    vm.expect_screen(&["notify-send: Disk full"])?;
    vm.press(&["shift"])?;
    vm.expect_screen_without("notify-send: Disk full")?;
    vm.type_text("notifications\n")?;
    vm.expect_screen(&[
        "  notify-send (backup): Backup done",
        "    All 3 disks",
        "  notify-send [critical]: Disk full",
    ])?;
    vm.press(&["alt-f2"])?;
    vm.expect("consoled: started /system/commands/nukesh on tty2 as process ")?;
    vm.expect_screen(&["(tty2)"])?;
    vm.type_text("service --status-all; service consoled status\n")?;
    vm.expect_screen(&[
        " [ + ]  consoled",
        "consoled - Virtual consoles",
        "Active: active (running)",
    ])?;
    // The German layout (ADR 0149), at once: Y and Z swap places, a dead
    // acute goes on the E after it, and AltGr+Q is @. The line is typed
    // before it takes effect.
    vm.type_text("keymap de; read -r line; printf '%s\\n' \"$line\" | xxd -p\n")?;
    vm.expect("spark: asked consoled to reload")?;
    vm.expect("consoled: the keys type in German (de)")?;
    vm.expect_screen(&["The consoles type in German (de) now."])?;
    vm.press(&["y", "equal", "e", "alt_r-q", "ret"])?;
    vm.expect_screen(&["7ac3a9400a"])?;
    // Its shells end with it, and Spark starts it again, in the layout
    // the setting names; this line has no Y or Z.
    vm.type_text("service consoled restart\n")?;
    vm.expect("spark: asked to restart consoled")?;
    vm.expect("spark: consoled (process ")?;
    vm.expect(" was ended by signal 15")?;
    vm.expect("spark: starting consoled again in ")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect("consoled: the keys type in German (de)")?;
    vm.expect("consoled: 6 consoles; tty1 is shown")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    // `keymap us`, typed on the German layout.
    vm.press(&["k", "e", "z", "m", "a", "p", "spc", "u", "s", "ret"])?;
    vm.expect("consoled: the keys type in English (US) (us)")?;
    vm.type_text("poweroff\n")?;
    vm.expect("spark: asked to power off the machine")?;
    vm.expect("spark: every service has stopped")?;
    vm.expect("Power: `spark` asked to power off the machine; syncing the filesystems")?;
    // QEMU ends when the guest enters ACPI's S5.
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// A normal boot with [`HEALTH_DISKS`]: Spark starts diskhealth(8) as a
/// service, after consoled, and it logs each disk's verdict, in the
/// order of their names; at the console, diskhealth shows them in a
/// table, then the attributes of the SATA disk (ADR 0157).
fn disk_health(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect("spark: started diskhealth (/system/commands/diskhealth) as process ")?;
    vm.expect("diskhealth: nvme0n1: PASSED")?;
    vm.expect("diskhealth: sda: PASSED")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    // The sum, so that the needle is not in the typed line.
    vm.type_text("diskhealth; echo \"status $(( $? + 100 ))\"\n")?;
    vm.expect_screen(&[
        "nvme0n1    NVMe  PASSED",
        "sda        SATA  PASSED",
        "vda        -",
        "status 100",
    ])?;
    vm.type_text("clear; diskhealth -a sda nvme0n1 | head -8; diskhealth -a nvme0n1 | tail -14\n")?;
    vm.expect_screen(&["Health: PASSED", "Raw read error rate", "Percentage used:"])?;
    vm.type_text("poweroff\n")?;
    vm.expect("spark: every service has stopped")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// A normal boot with [`TOOLS_DISKS`]: at the console, lsblk(8),
/// blkid(8) and findmnt(8) name the disks, the partition table, the
/// filesystems and their mounts; fstrim(8) trims the Btrfs (ADR 0158),
/// not the FAT, and iostat(1) shows the Btrfs disk, whose `stat` has
/// counted the discards. Each command names the test's disks, so that the
/// boot stick does not change the columns. The host's `btrfs check` then
/// checks the trimmed disk.
fn storage_tools(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    // The sum, so that the needle is not in the typed line.
    vm.type_text(
        "lsblk -l -o NAME,TYPE,FSTYPE,LABEL,MOUNTPOINTS vda vdb; echo \"status $(( $? + 100 ))\"\n",
    )?;
    vm.expect_screen(&[
        "NAME TYPE FSTYPE LABEL      MOUNTPOINTS",
        "vda1 part vfat   RADIXPART  /volumes/vda1",
        "vdb  disk btrfs  RADIXBTRFS /volumes/vdb",
        "status 100",
    ])?;
    vm.type_text(
        "clear; blkid -s LABEL -s TYPE vda1 vdb; blkid -o value -s PTTYPE vda; blkid -L RADIXBTRFS; echo \"status $(( $? + 101 ))\"\n",
    )?;
    vm.expect_screen(&[
        "/devices/vda1: LABEL=\"RADIXPART\" TYPE=\"vfat\"",
        "/devices/vdb: LABEL=\"RADIXBTRFS\" TYPE=\"btrfs\"",
        "gpt",
        "/devices/vdb",
        "status 101",
    ])?;
    vm.type_text(
        "clear; findmnt -n -o SOURCE,FSTYPE,OPTIONS /volumes/vdb; findmnt -n -o TARGET -S vda1; findmnt -t xfs; echo \"status $(( $? + 102 ))\"\n",
    )?;
    vm.expect_screen(&["/devices/vdb btrfs rw", "/volumes/vda1", "status 103"])?;
    vm.type_text(
        "clear; fstrim -v /volumes/vdb; fstrim /volumes/vda1; echo \"status $(( $? + 104 ))\"\n",
    )?;
    vm.expect_screen(&[
        "/volumes/vdb: ",
        " bytes) trimmed",
        "fstrim: /volumes/vda1: the discard operation is not supported",
        "status 105",
    ])?;
    // replace(8) takes the control right too, as fstrim does: the kernel
    // gets as far as refusing a disk that is mounted.
    vm.type_text(
        "clear; replace start 1 /devices/vda1 /volumes/vdb; echo \"status $(( $? + 105 ))\"\n",
    )?;
    vm.expect_screen(&["replace: /devices/vda1 is mounted", "status 106"])?;
    vm.type_text(
        "clear; iostat -d vdb; set -- $(cat /kernel/block/vdb/stat); echo \"discards $(( ${12} > 0 ))\"\n",
    )?;
    vm.expect_screen(&[
        "Device             tps    kB_read/s    kB_wrtn/s",
        "vdb           ",
        "discards 1",
    ])?;
    vm.type_text("poweroff\n")?;
    vm.expect("spark: every service has stopped")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// QEMU with [`POWER_AML`] (ADR 0201): the kernel logs the battery, the
/// adapter, the lid and both power buttons; at the console, Linux's
/// `/sys/class/power_supply` and `/proc/acpi/button/lid` show them; the
/// fixed power button still powers off.
fn acpi_power(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    // Said after uACPI's line, before the boot report ends.
    for line in [
        "Power: BAT0: a Li-ion battery of 48000 mWh (50000 mWh new), 75 % charged, discharging",
        "Power: ADP1: an AC adapter, unplugged",
        "Lid: LID0 is open",
        "Power: the ACPI power button and PWRB (through uACPI) asks Spark to power the machine off",
    ] {
        vm.expect_anywhere(line)?;
    }
    vm.expect("Boot complete.")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    // `is` between name and value, so that no needle is in a typed line.
    vm.type_text(
        "cd /sys/class/power_supply; for f in BAT0/status BAT0/capacity BAT0/energy_now \
         BAT0/power_now ADP1/type ADP1/online; do echo \"$f is $(cat $f)\"; done\n",
    )?;
    vm.expect_screen(&[
        "BAT0/status is Discharging",
        "BAT0/capacity is 75",
        "BAT0/energy_now is 36000000",
        "BAT0/power_now is 9000000",
        "ADP1/type is Mains",
        "ADP1/online is 0",
    ])?;
    vm.type_text(
        "clear; for f in type technology cycle_count model_name manufacturer serial_number \
         voltage_now capacity_level energy_full energy_full_design; do \
         echo \"$f is $(cat BAT0/$f)\"; done; cat /proc/acpi/button/lid/LID0/state\n",
    )?;
    vm.expect_screen(&[
        "type is Battery",
        "technology is Li-ion",
        "cycle_count is 42",
        "model_name is DUKOVO TEST",
        "manufacturer is Dukovo",
        "serial_number is 0042",
        "voltage_now is 12100000",
        "capacity_level is Normal",
        "energy_full is 48000000",
        "energy_full_design is 50000000",
        "state:      open",
    ])?;
    vm.press_power_button()?;
    vm.expect("spark: the power button was pressed; powering off the machine")?;
    vm.expect("Power: powering off (ACPI S5, through uACPI)")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// QEMU with [`THERMAL_AML`] (ADR 0203): the kernel logs the zone, Linux's
/// `/sys/class/thermal` and sensors(1) show it at the console, and once the console has
/// read its temperature twice it warms 5 °C a read: past its passive trip
/// point, where QEMU's CPUs cannot be slowed, its hot one, and to its
/// critical one, where Spark powers the machine off.
fn acpi_thermal(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect_anywhere(
        "Thermal: TZ00 is at 45 °C; critical at 80 °C; hot at 70 °C; passive at 60 °C; \
         looked at when the firmware says it changed",
    )?;
    vm.expect("Boot complete.")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    // `is` between name and value, so that no needle is in a typed line.
    vm.type_text(
        "cd /sys/class/thermal/thermal_zone0; for f in type temp mode policy \
         trip_point_0_type trip_point_0_temp trip_point_1_type trip_point_1_temp \
         trip_point_2_type trip_point_2_temp trip_point_2_hyst; do echo \"$f is $(cat $f)\"; \
         done\n",
    )?;
    vm.expect_screen(&[
        "type is acpitz",
        "temp is 45000",
        "mode is enabled",
        "policy is step_wise",
        "trip_point_0_type is critical",
        "trip_point_0_temp is 80000",
        "trip_point_1_type is hot",
        "trip_point_1_temp is 70000",
        "trip_point_2_type is passive",
        "trip_point_2_temp is 60000",
        "trip_point_2_hyst is 0",
    ])?;
    // The zone as hwmon's `acpitz` shows it, through sensors(1): its
    // fourth read. The console's font has no degree sign.
    vm.type_text("clear; sensors\n")?;
    vm.expect_screen(&[
        "acpitz-acpi-0",
        "Adapter: ACPI interface",
        "temp1:        +45.0",
        "(crit = +80.0",
    ])?;
    // The zone's fifth read: it says it changed, and warms from there, so
    // fast that the machine may be off before a screenshot.
    vm.type_text("clear; cat /sys/class/thermal/thermal_zone0/temp\n")?;
    let slow = "the firmware picks the CPUs' speed, so the kernel cannot slow them";
    for line in [
        format!(
            "Thermal: TZ00 is at 60 °C, at or past its passive trip point, 60 °C: {slow} (1 of 3)"
        ),
        format!(
            "Thermal: TZ00 is at 65 °C, at or past its passive trip point, 60 °C: {slow} (2 of 3)"
        ),
        "Thermal: TZ00 is at 70 °C, at or past its hot trip point, 70 °C".into(),
        format!(
            "Thermal: TZ00 is at 70 °C, at or past its passive trip point, 60 °C: {slow} (3 of 3)"
        ),
        "Thermal: TZ00 is at 80 °C, at or past its critical trip point, 80 °C: powering off".into(),
        "Power: TZ00 is too hot; asking Spark to power the machine off".into(),
        "spark: the power button was pressed; powering off the machine".into(),
        "Power: `spark` asked to power off the machine; syncing the filesystems".into(),
        "Power: powering off (ACPI S5, through uACPI)".into(),
    ] {
        vm.expect(&line)?;
    }
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// Spark powers the machine off when the ACPI power button is pressed:
/// heard through uACPI, which then powers off (ADR 0200), or with
/// `aml=off` by the kernel itself (ADR 0142).
fn power_button(vm: &mut Vm, case: &Case) -> Result<(), String> {
    let uacpi = !case.cmdline.split_ascii_whitespace().any(|flag| flag == "aml=off");
    boot_report(vm, case)?;
    // Said before the boot report ends.
    let (button, off) = if uacpi {
        ("the ACPI power button (through uACPI)", "Power: powering off (ACPI S5, through uACPI)")
    } else {
        ("the ACPI power button", "Power: powering off (ACPI S5, sleep types ")
    };
    if !vm.log().contains(&format!("Power: {button} asks Spark to power the machine off")) {
        return Err(format!("the kernel does not say it hears {button}"));
    }
    vm.expect("Boot complete.")?;
    vm.expect("spark: listening at /system/live/spark/control")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect("consoled: 6 consoles; tty1 is shown")?;
    vm.press_power_button()?;
    vm.expect("Power: the power button was pressed")?;
    vm.expect("spark: the power button was pressed; powering off the machine")?;
    vm.expect("spark: every service has stopped")?;
    vm.expect("Power: `spark` asked to power off the machine; syncing the filesystems")?;
    vm.expect(off)?;
    // QEMU ends when the guest enters ACPI's S5.
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// QEMU's xHCI controller, with the boot stick on its first port, a
/// keyboard on the second, and a hub on the third with a mouse behind it
/// and a second hub with a keyboard behind that. QEMU gives each of its
/// four ports a USB 3 port (1-4) and a USB 2 one (5-8): the stick takes
/// the USB 3 one, the rest the USB 2 ones; its hub is a full-speed one.
/// lsusb(8) at the console lists them and their tree. A tablet is plugged in on the last root port, and another on the first
/// hub, and pulled out again; pulling out the second hub takes its
/// keyboard along. The power button, which halts the controller, ends the
/// test. The controller's thread runs alongside the boot, so what it says
/// at boot is looked for anywhere in the log.
fn usb_devices(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    for line in [
        "USB: bus 1 is the xHCI 1.0 controller at 0000:00:02.0: 8 ports (USB 3 on 1-4, USB 2 on 5-8), 64 device slots, MSI",
        "USB: bus 1 has no Debug Capability, so the console is not on it",
        "USB: 1-1: QEMU USB HARDDRIVE (46f4:0001, QEMU), mass storage, SuperSpeed",
        "USB: 1-6: QEMU USB Keyboard (0627:0001, QEMU), keyboard, high speed",
        "USB: 1-7: QEMU USB Hub (0409:55aa, QEMU), hub, full speed",
        "USB: 1-7: a hub of 8 ports",
        "USB: 1-7.1: QEMU USB Mouse (0627:0001, QEMU), mouse, full speed",
        "USB: 1-7.8: QEMU USB Hub (0409:55aa, QEMU), hub, full speed",
        "USB: 1-7.8.4: QEMU USB Keyboard (0627:0001, QEMU), keyboard, full speed",
    ] {
        vm.expect_anywhere(line)?;
    }
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    // lsusb(8), from /kernel/bus/usb: each device numbered as it was set
    // up, named from usb.ids, else by its own strings; then the tree.
    vm.type_text("lsusb\n")?;
    vm.expect_screen(&[
        "Bus 001 Device 002: ID 46f4:0001 QEMU QEMU USB HARDDRIVE",
        "Bus 001 Device 003: ID 0627:0001 Adomax Technology Co., Ltd QEMU Tablet",
        "Bus 001 Device 004: ID 0409:55aa NEC Corp. Hub",
        "Bus 001 Device 005: ID 0627:0001 Adomax Technology Co., Ltd QEMU Tablet",
        "Bus 001 Device 006: ID 0409:55aa NEC Corp. Hub",
        "Bus 001 Device 007: ID 0627:0001 Adomax Technology Co., Ltd QEMU Tablet",
    ])?;
    vm.type_text("clear; lsusb -t\n")?;
    vm.expect_screen(&[
        "/:  Bus 001.Port 001: Dev 001, Class=root_hub, Driver=xhci_hcd/8p, 5000M",
        "    |__ Port 001: Dev 002, If 0, Class=Mass Storage, Driver=usb-storage, 5000M",
        "    |__ Port 006: Dev 003, If 0, Class=Human Interface Device, Driver=usbhid, 480M",
        "    |__ Port 007: Dev 004, If 0, Class=Hub, Driver=hub/8p, 12M",
        "        |__ Port 001: Dev 005, If 0, Class=Human Interface Device, Driver=usbhid, 12M",
        "        |__ Port 008: Dev 006, If 0, Class=Hub, Driver=hub/8p, 12M",
        "            |__ Port 004: Dev 007, If 0, Class=Human Interface Device, Driver=usbhid, \
         12M",
    ])?;
    vm.monitor("device_add usb-tablet,bus=xhci.0,port=4,id=tablet")?;
    vm.expect("USB: 1-8: QEMU USB Tablet (0627:0001, QEMU), HID, high speed")?;
    vm.monitor("device_del tablet")?;
    vm.expect("USB: 1-8: QEMU USB Tablet was unplugged")?;
    vm.monitor("device_add usb-tablet,bus=xhci.0,port=3.5,id=tablet")?;
    vm.expect("USB: 1-7.5: QEMU USB Tablet (0627:0001, QEMU), HID, full speed")?;
    vm.monitor("device_del tablet")?;
    vm.expect("USB: 1-7.5: QEMU USB Tablet was unplugged")?;
    vm.monitor("device_del hub2")?;
    vm.expect("USB: 1-7.8: QEMU USB Hub was unplugged")?;
    vm.expect("USB: 1-7.8.4: QEMU USB Keyboard was unplugged")?;
    vm.press_power_button()?;
    vm.expect("Power: the power button was pressed")?;
    vm.expect("spark: every service has stopped")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// The display the USB devices of `usb-input` are bound to.
const VIDEO: &str = "video0";

/// How a line of `/devices/mouse`'s records reads through hexdump(1): the
/// time in hex, then dx, dy, wheel, pan, x, y, buttons and flags.
const MOTIONS: &str = r#"'2/4 "%x" 8/4 " %d" "\n"'"#;

/// A USB keyboard and mouse beside the PS/2 keyboard, on a normal boot.
/// Each USB keyboard's keys go where the PS/2 one's do: the test types on
/// the screen's shell with both. A key held on the USB keyboard repeats,
/// as the kernel repeats it, and stops when the keyboard is pulled out
/// with the key still down. A shell reads `/devices/mouse` a motion at a
/// time, with dd(1) and hexdump(1), while the mouse moves and clicks, and
/// while a tablet plugged in then points. QEMU hands a button to a pointer bound to the
/// display before any other, so the tablet comes after the mouse's
/// clicks. The controller's thread runs alongside the boot, so what it
/// says at boot is looked for anywhere in the log.
fn usb_input(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    for line in [
        "USB: 1-6: interface 0: a keyboard, read into /devices/kbd",
        "USB: 1-7: interface 0: a mouse, read into /devices/mouse",
    ] {
        vm.expect_anywhere(line)?;
    }
    vm.expect("consoled: 6 consoles; tty1 is shown")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    vm.type_on(VIDEO, "echo $((6 * 7)) usb\n")?;
    vm.expect_screen(&["# echo $((6 * 7)) usb", "42 usb"])?;
    vm.type_text("echo $((6 * 8)) ps2\n")?;
    vm.expect_screen(&["# echo $((6 * 8)) ps2", "48 ps2"])?;
    // Held for a second: down, then repeated after 250 ms and every 33 ms,
    // some 20 times; emulated, fewer, but more than ten.
    vm.type_on(VIDEO, "echo ")?;
    vm.keys_on(VIDEO, &["x"], true)?;
    std::thread::sleep(Duration::from_millis(1000));
    vm.keys_on(VIDEO, &["x"], false)?;
    vm.type_on(VIDEO, " held\n")?;
    vm.expect_screen(&["xxxxxxxxxx held"])?;
    // The mouse moves, then its left button goes down and up; a tablet
    // plugged in points at the right edge, halfway down.
    // A subshell opens the device before it says so, and what it says is
    // a product it works out, which the command echoed does not show: a
    // motion made before the device is opened is not read. dd(1) reads
    // one motion at a time, so each shows as it comes. The shell's own
    // descriptors from 3 up are its line editor's.
    let one = format!("dd bs=40 count=1 status=none | hexdump -v -e {MOTIONS}");
    vm.type_text(&format!(
        "(echo $((5 * 8)) opened; for i in 1 2 3 4; do {one}; done) </devices/mouse\n"
    ))?;
    vm.expect_screen(&["40 opened"])?;
    let rel = |axis: &str, value: i32| {
        format!(r#"{{"type":"rel","data":{{"axis":"{axis}","value":{value}}}}}"#)
    };
    let abs = |axis: &str, value: i32| {
        format!(r#"{{"type":"abs","data":{{"axis":"{axis}","value":{value}}}}}"#)
    };
    let button =
        |down: bool| format!(r#"{{"type":"btn","data":{{"down":{down},"button":"left"}}}}"#);
    // Time between them, for the device to report each on its own.
    vm.input_events(VIDEO, &[rel("x", 10), rel("y", -5)])?;
    std::thread::sleep(Duration::from_millis(200));
    vm.input_events(VIDEO, &[button(true)])?;
    std::thread::sleep(Duration::from_millis(200));
    vm.input_events(VIDEO, &[button(false)])?;
    vm.monitor("device_add usb-tablet,bus=xhci.0,port=4,id=tablet,display=video0")?;
    vm.expect("USB: 1-8: interface 0: a tablet, read into /devices/mouse")?;
    vm.input_events(VIDEO, &[abs("x", 0x7fff), abs("y", 0x4000)])?;
    // 0x4000 of QEMU's 0x7fff is 32768 of POSITION_MAX, 65535.
    vm.expect_screen(&[
        " 10 -5 0 0 0 0 0 0",
        " 0 0 0 0 0 0 1 0",
        " 0 0 0 0 0 0 0 0",
        " 0 0 0 0 65535 32768 0 1",
    ])?;
    // Pulled out with a key down: the key goes up, and stops repeating.
    vm.type_on(VIDEO, "echo ")?;
    vm.keys_on(VIDEO, &["y"], true)?;
    std::thread::sleep(Duration::from_millis(500));
    vm.monitor("device_del usbkbd")?;
    vm.expect("USB: 1-6: QEMU USB Keyboard was unplugged")?;
    let count = |vm: &mut Vm| -> Result<usize, String> {
        Ok(vm.screen()?.iter().map(|line| line.matches('y').count()).sum())
    };
    std::thread::sleep(Duration::from_millis(500));
    let before = count(vm)?;
    std::thread::sleep(Duration::from_millis(1000));
    if count(vm)? != before {
        return Err("the key of the keyboard pulled out went on repeating".into());
    }
    vm.type_text(" gone\n")?;
    vm.expect_screen(&["y gone"])?;
    vm.type_text("poweroff\n")?;
    vm.expect("spark: asked to power off the machine")?;
    vm.expect("spark: every service has stopped")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// QEMU's `mce` monitor command fills a bank as a CPU would. A corrected
/// error raises nothing: the kernel's next read of the banks finds it. An
/// uncorrected one with the processor's state corrupt raises #MC and the
/// kernel panics, restarting at once (`panic=0`); the bank keeps the
/// error across the restart, as on real machines, and the next boot finds
/// it. The power button ends the test.
fn machine_check(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    // Said before the boot report ends, with QEMU's 10 banks.
    if !vm.log().contains("Machine check: 10 banks on each CPU, read at boot and every 1 s") {
        return Err("the kernel did not turn machine-check reporting on".into());
    }
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    // CPU 1, bank 2: VAL, EN, MISCV and ADDRV; a level 0 data cache error
    // on a data read.
    vm.monitor("mce 1 2 0x9c00000000000134 0 0x12345000 0x86")?;
    vm.expect(
        "Machine check: CPU 1 bank 2: corrected data read error in the level 0 data cache, \
         address 0x12345000 (status 0x9c00000000000134, misc 0x86)",
    )?;
    // Memory controllers' errors are counted (ADR 0169): a corrected read
    // error on channel 1 of bank 3 from each CPU, and one of bank 4 that
    // names no channel.
    for cpu in [0, 1] {
        vm.monitor(&format!("mce {cpu} 3 0x9c00000000000091 0 0x20000000 0x86"))?;
        vm.expect(&format!(
            "Machine check: CPU {cpu} bank 3: corrected memory read error on channel 1, address \
             0x20000000 (status 0x9c00000000000091, misc 0x86)"
        ))?;
    }
    vm.monitor("mce 0 4 0x9c0000000000009f 0 0x20001000 0x86")?;
    vm.expect(
        "Machine check: CPU 0 bank 4: corrected memory read error, address 0x20001000 (status \
         0x9c0000000000009f, misc 0x86)",
    )?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    vm.type_text(
        "cd /kernel/devices/system/edac/mc/mc0; echo edac $(cat ce_count ce_noinfo_count \
         ue_count dimm0/dimm_ce_count dimm0/dimm_label) $(ls | wc -l) $(ls -d dimm*)\n",
    )?;
    vm.expect_screen(&["edac 3 1 0 2 bank 3 channel 1 7 dimm0"])?;
    // CPU 0, bank 1: VAL, UC, EN, ADDRV and PCC, a memory read error, with
    // only MCIP in MCG_STATUS: the interrupted code cannot restart.
    let fatal = "bank 1: uncorrected memory read error, address 0x9876000; the processor's \
                 state is corrupt (status 0xb60000000000009f)";
    vm.monitor("mce 0 1 0xb60000000000009f 0x4 0x9876000 0")?;
    vm.expect("Radix panic on CPU 0")?;
    vm.expect("Machine check (#MC) at 0x")?;
    vm.expect(&format!("{fatal}; the interrupted code cannot restart"))?;
    vm.expect("Crash log: boot 2; the previous boot (1) ended with a kernel panic")?;
    vm.expect(&format!("Machine check: CPU 0 {}", fatal.replacen(':', ", at boot:", 1)))?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.press_power_button()?;
    vm.expect("spark: the power button was pressed; powering off the machine")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// The `machine-check-program` test (ADR 0168): memfail
/// (userspace/posix/memfail.c) reads a page of its own in a loop. With
/// the machine stopped where a CPU runs it in user mode, QEMU puts an
/// uncorrected data load error naming that page's physical address in a
/// bank of that CPU, as software error recovery reports one that requires
/// an action (SRAR), and raises #MC. The kernel logs the error, retires
/// the page, takes it out of memfail's memory and sends memfail SIGBUS
/// with BUS_MCEERR_AR; memfail's handler reads the page again, which
/// faults without another machine check, and memfail ends. A memory
/// scrubbing error the CPU went on after (SRAO), polled from its bank, has
/// its page retired too.
fn machine_check_program(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_ahead("spark: started /system/tests/posix/memfail as process ")?;
    vm.expect("memfail: hardware_corrupted: 0 KiB")?;
    let reading = program_line(vm, "memfail: reading ")?;
    let (virt, main) = reading.split_once(" from ").ok_or(format!("memfail: reading {reading}"))?;
    let hex = |text: &str| u64::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok();
    let (page, main) =
        hex(virt).zip(hex(main)).ok_or(format!("memfail: reading {reading}: not two addresses"))?;
    // An SRAO on CPU 0, bank 2: VAL, UC, EN, MISCV, ADDRV and S, a memory
    // scrubbing error, at a physical address valid to the cache line.
    vm.monitor("mce 0 2 0xbd000000000000cf 0x5 0x30000000 0x86")?;
    vm.expect(
        "Machine check: CPU 0 bank 2: uncorrected memory scrubbing error, address 0x30000000; \
         the CPU went on (status 0xbd000000000000cf, misc 0x86)",
    )?;
    let retired = program_line(vm, "Machine check: the page at 0x30000000 ")?;
    if retired != "is out of use for good" && retired != "leaves use for good once freed" {
        return Err(format!("the scrubbed page: {retired}"));
    }
    let cpu = stop_in(vm, case.cpus, main..main + 0x1000)?;
    let gpa = vm.monitor_output(&format!("gva2gpa {page:#x}"), cpu)?;
    let phys = gpa
        .trim()
        .strip_prefix("gpa: ")
        .and_then(hex)
        .ok_or(format!("QEMU's gva2gpa {page:#x}: {}", gpa.trim()))?;
    // VAL, UC, EN, MISCV, ADDRV, S and AR: a data load in the level 0
    // data cache, with EIPV and MCIP: the instruction met it.
    vm.monitor(&format!("mce {cpu} 1 0xbd80000000000134 0x6 {phys:#x} 0x86"))?;
    vm.monitor("cont")?;
    vm.expect(&format!(
        "Machine check: CPU {cpu} bank 1: uncorrected data read error in the level 0 data \
         cache, address {phys:#x}; the code that met it must not go on (status \
         0xbd80000000000134, misc 0x86)"
    ))?;
    vm.expect(&format!("Machine check: the page at {phys:#x} leaves use for good once freed"))?;
    // As memfail prints the page's address.
    let virt = virt.trim();
    vm.expect(&format!("memfail: SIGBUS, code 4, address {virt}, lsb 12"))?;
    vm.expect(&format!("memfail: SIGBUS again, code 4, address {virt}"))?;
    vm.expect("memfail: hardware_corrupted: 8 KiB")?;
    let code = program_line(vm, "(/system/tests/posix/memfail) exited with code ")?;
    if code != "0x0" {
        return Err(format!("memfail exited with code {code}"));
    }
    if vm.log().contains("Radix panic") {
        return Err("the kernel panicked".into());
    }
    vm.expect("spark: nothing more to do; waiting")
}

/// Stops the machine where one of its `cpus` runs code at `pc` in user
/// mode (code selector 0x2b), trying for up to a minute: that CPU.
fn stop_in(vm: &mut Vm, cpus: u32, pc: std::ops::Range<u64>) -> Result<u32, String> {
    for _ in 0..600 {
        vm.monitor("stop")?;
        for cpu in 0..cpus {
            let registers = vm.monitor_output("info registers", cpu)?;
            let rip = registers
                .split_once("RIP=")
                .and_then(|(_, rest)| u64::from_str_radix(rest.get(..16)?, 16).ok());
            if registers.contains("CS =002b") && rip.is_some_and(|rip| pc.contains(&rip)) {
                return Ok(cpu);
            }
        }
        vm.monitor("cont")?;
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("no CPU ran the code at {:#x} in user mode", pc.start))
}

fn watchdog_esb(vm: &mut Vm, case: &Case) -> Result<(), String> {
    watchdog_restarts(vm, case, "the Intel 6300ESB watchdog, with timeouts from 1 to 2082 s")
}

fn watchdog_tco(vm: &mut Vm, case: &Case) -> Result<(), String> {
    watchdog_restarts(vm, case, "the Intel ICH9 TCO timer, with timeouts from 4 to 1227 s")
}

/// Spark, asked to stop responding, leaves the watchdog `found` unpetted,
/// and it restarts the machine. With `kdb` on the command line, kdb waits
/// at the end of each boot, and in the first pets the watchdog for twice
/// its timeout, until it is told to continue. The next boot says the
/// watchdog restarted it, Spark pets the watchdog for twice its timeout,
/// and the power button powers off.
fn watchdog_restarts(vm: &mut Vm, case: &Case, found: &str) -> Result<(), String> {
    let name = found.split(", with").next().unwrap_or(found);
    let started =
        format!("Watchdog: `spark` started {name}: the machine restarts unless petted every 12 s");
    let kdb = case.cmdline.starts_with("kdb ");
    for boot in 1..=2 {
        boot_report(vm, case)?;
        // Said before the boot report ends.
        let log = vm.log();
        if !log.contains(&format!("Watchdog: {found}; Spark starts it")) {
            return Err(format!("boot {boot}: the kernel did not find {name}"));
        }
        vm.expect("Boot complete.")?;
        if kdb {
            // kdb stops kmain's CPU; Spark runs on the other.
            vm.expect("kdb on CPU ")?;
        }
        vm.expect(&started)?;
        if boot == 1 {
            vm.expect("spark: not responding, as --test-hang asks, until the watchdog restarts")?;
        } else {
            vm.expect(
                "spark: the watchdog restarted the machine: the last boot stopped responding",
            )?;
        }
        if kdb {
            if boot == 1 {
                std::thread::sleep(Duration::from_secs(24));
                if vm.log().matches("Boot complete.").count() != 1 {
                    return Err("the watchdog restarted the machine while kdb petted it".into());
                }
            }
            vm.kdb("", &[])?;
            vm.send("continue\r")?;
            vm.expect("kdb: continuing")?;
        }
    }
    let restarted =
        format!("Watchdog: {name} restarted the machine: the boot before this one hung");
    if !vm.log().contains(&restarted) {
        return Err(format!(
            "the kernel does not say the watchdog restarted it: no \"{restarted}\""
        ));
    }
    // Petted, it lets the machine run on.
    std::thread::sleep(Duration::from_secs(24));
    let log = vm.log();
    if log.matches("Boot complete.").count() != 2 {
        return Err("the watchdog restarted a machine whose Spark petted it".into());
    }
    if !log.contains("consoled: 6 consoles; tty1 is shown") {
        return Err("the second boot did not start the consoles".into());
    }
    vm.press_power_button()?;
    vm.expect("Power: `spark` asked to power off the machine; syncing the filesystems")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

fn mm_stress(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    let log = vm.log();
    for needle in ["ACPI: SRAT: 4 CPUs", "in 2 proximity domains", "ACPI: SLIT", "on 2 NUMA nodes"]
    {
        if !log.contains(needle) {
            return Err(format!("the NUMA topology is not reported: no \"{needle}\""));
        }
    }
    vm.expect("selftest mm-stress: ")?;
    vm.expect(" objects allocated and ")?;
    selftest_passed(vm, "mm-stress")?;
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn tlb(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    selftest_passed(vm, "tlb")?;
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

/// QEMU with [`CROSEC_AML`] (ADR 0207): the kernel finds the EC's ACPI
/// device, and the memory map it reads at 0xe00 is the empty bus's.
fn framework_ec(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect_anywhere(
        "EC: the firmware names FRMWC004, but its memory map does not say EC at port 0xe00; \
         it is not used",
    )?;
    vm.expect("Boot complete.")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    // consoled's file asks for admin.hardware, so root's shell holds the
    // platform capability: the kernel itself answers that there is no EC,
    // where a shell without it would be told it needs admin.hardware.
    vm.type_text("ectool fanduty 50\n")?;
    vm.expect_screen(&["ectool: the kernel drives no embedded controller"])?;
    vm.type_text("ectool pwmgetnumfans\n")?;
    vm.expect_screen(&["Number of fans = 0"])?;
    vm.type_text("poweroff\n")?;
    vm.expect("spark: asked to power off the machine")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

fn smbus(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    // Said after the PCI scan, among the lines the boot report reads past.
    vm.expect_anywhere(
        "SMBus: Intel's controller (8086:2930) at port 0x700 (SMBus I801 adapter at 0700); \
         memory modules' EEPROMs answer at 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57",
    )?;
    // SeaBIOS keeps the SMBIOS entry point in its area (ADR 0231).
    vm.expect_anywhere("DMI: QEMU Standard PC (Q35 + ICH9, 2009), BIOS ")?;
    // The tables' memory device says DDR4, so its module's EEPROM is an
    // EE1004 (ADR 0232).
    vm.expect_anywhere("SMBus: the DDR4 memory modules' SPD EEPROMs (ee1004) at 0x50")?;
    vm.expect("Boot complete.")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    // `is` between name and value, so that no needle is in a typed line.
    vm.type_text("echo \"name is $(cat /sys/bus/i2c/devices/i2c-0/name)\"\n")?;
    vm.expect_screen(&["name is SMBus I801 adapter at 0700"])?;
    // Read for the first time: both pages of QEMU's 256-byte EEPROM.
    vm.type_text(
        "d=/sys/bus/i2c/devices/0-0050; echo \"spd is $(cat $d/name), $(wc -c < $d/eeprom) \
         bytes\"\n",
    )?;
    vm.expect_screen(&["spd is ee1004, 512 bytes"])?;
    vm.type_text("clear; hwinfo memory\n")?;
    vm.expect_screen(&[
        "Memory",
        "  DIMM 0:           1 GiB DDR4 SODIMM, 2666 MT/s, Dukovo DK-DDR4-1",
        "  SPD at 0x50:      blank",
    ])?;
    vm.type_text("poweroff\n")?;
    vm.expect("spark: asked to power off the machine")?;
    vm.expect("Power: `spark` asked to power off the machine; syncing the filesystems")?;
    // QEMU ends when the guest enters ACPI's S5.
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// An SMBIOS memory device (type 17, of SMBIOS 2.8's 40 bytes): a 1 GiB
/// DDR4-2666 SO-DIMM in slot `DIMM 0`, made by `Dukovo`, part
/// `DK-DDR4-1`, of QEMU's memory array (handle 1000h).
fn ddr4_device() -> Vec<u8> {
    let mut bytes = vec![17, 0x28, 0x00, 0x11, 0x00, 0x10, 0xfe, 0xff];
    // Widths, the size in MiB, the form (SO-DIMM), the set, the locators,
    // the kind (DDR4) and its details (synchronous).
    bytes.extend_from_slice(&[64, 0, 64, 0, 0x00, 0x04, 0x0d, 0, 1, 0, 0x1a, 0x80, 0]);
    // The speed, the maker, serial number, asset tag and part number, the
    // rank, the extended size, the speed set, and the voltages (1.2 V).
    bytes.extend_from_slice(&2666u16.to_le_bytes());
    bytes.extend_from_slice(&[2, 0, 0, 3, 1, 0, 0, 0, 0]);
    bytes.extend_from_slice(&2666u16.to_le_bytes());
    for _ in 0..3 {
        bytes.extend_from_slice(&1200u16.to_le_bytes());
    }
    for string in ["DIMM 0", "Dukovo", "DK-DDR4-1"] {
        bytes.extend_from_slice(string.as_bytes());
        bytes.push(0);
    }
    bytes.push(0);
    bytes
}

fn gpio(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect_anywhere("GPIO: AMD's controller \\_SB_.GPIO: 184 pins, interrupt 5 (rising edge)")?;
    vm.expect_anywhere(
        "GPIO: Intel Sunrise Point-LP's controller \\_SB_.GPI0: 152 pins, interrupt 6 (rising edge)",
    )?;
    vm.expect_anywhere("GPIO: the firmware's events on \\_SB_.GPIO: pins 9 (_E09), 42 (_EVT)")?;
    vm.expect_anywhere("GPIO: the firmware's events on \\_SB_.GPI0: pins 53 (_L35)")?;
    selftest_passed(vm, "gpio")?;
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn smbios(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    // Said before the scheduler starts, among the lines the boot report
    // reads past.
    vm.expect_anywhere("SMBIOS 3.0.0 present.")?;
    vm.expect_anywhere("DMI: Dukovo Test Machine/Test Board, BIOS ")?;
    vm.expect_anywhere("DMI: Memory slots populated: 1/1")?;
    // QEMU's own memory device is of kind "RAM" (ADR 0232).
    vm.expect_anywhere("SMBus: RAM memory (kind 0x07) has no SPD EEPROM the kernel reads")?;
    vm.expect("Boot complete.")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    // `is` between name and value, so that no needle is in a typed line.
    vm.type_text("echo \"product is $(cat /sys/class/dmi/id/product_name)\"\n")?;
    vm.expect_screen(&["product is Test Machine"])?;
    vm.type_text("test -e /sys/class/dmi/id/product_serial || echo \"no serial\" file\n")?;
    vm.expect_screen(&["no serial file"])?;
    // consoled's file asks for admin.hardware, so root's shell holds the
    // platform capability, and hwinfo reads the tables whole.
    vm.type_text("clear; hwinfo -s system board\n")?;
    vm.expect_screen(&[
        "System",
        "  Manufacturer:     Dukovo",
        "  Product:          Test Machine",
        "  Version:          2.0",
        "  Family:           Testers",
        "  SKU:              DK-T1",
        "  Serial number:    DK-0042",
        "Board",
        "  Product:          Test Board",
        "  Serial number:    DB-0042",
    ])?;
    vm.type_text("clear; hwinfo memory\n")?;
    vm.expect_screen(&[
        "Memory",
        "  Installed:        1 GiB in 1 of 1 slots",
        "  DIMM 0:           1 GiB RAM DIMM, 3200 MT/s, QEMU DK-RAM-1",
    ])?;
    vm.type_text("poweroff\n")?;
    vm.expect("spark: asked to power off the machine")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

fn msr(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    if vm.kvm {
        selftest_passed(vm, "msr")?;
    } else {
        // QEMU's emulation reads MSRs it does not know as 0, so only KVM,
        // as on CI, makes the read fault.
        vm.expect("selftest msr: skipped; MSR 2FFF_FFFFh, which no processor has, read as 0x0")?;
        vm.expect("Boot complete.")?;
    }
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn caps(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("selftest caps: ")?;
    vm.expect(" forgeries and ")?;
    vm.expect(" derivations by 3 other CPUs during a ")?;
    selftest_passed(vm, "caps")?;
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn net(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    selftest_passed(vm, "net")?;
    // The card is found during the PCI scan, among lines boot_report
    // reads past.
    let log = vm.log();
    let card = log
        .lines()
        .find_map(|line| line.find("Net: net0 is a virtio network card at ").map(|at| &line[at..]))
        .ok_or("the kernel never found the network card")?;
    let described =
        card.contains(", MAC 52:54:00:12:34:56, 256 receive and 256 transmit slots, MSI-X vector ");
    if !described {
        return Err(format!("the kernel described the card as \"{card}\""));
    }
    // Behind the IOMMU, as ACCESS_PLATFORM lets it be.
    let address = card.split(" at ").nth(1).and_then(|rest| rest.split(',').next());
    let confined = address.map(|address| format!("IOMMU: {address} reaches only what is mapped"));
    let mut needles = vec![
        confined.ok_or("the card's line names no address")?,
        "Net: net0 has the address 10.0.2.15/24, through the router 10.0.2.2".to_string(),
    ];
    needles.extend((1..=4).map(|seq| format!("Net: 10.0.2.2 answered ping {seq} in ")));
    needles.push("selftest net: passed; the router answered 4 pings, the fastest in ".into());
    for needle in &needles {
        if !log.contains(needle.as_str()) {
            return Err(format!("the kernel never said \"{needle}\""));
        }
    }
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

/// The `sockets` test (userspace/sockets): its program's steps over lo,
/// through the card and refused, and every network socket's memory back
/// in the kernel's.
/// The `interfaces` test (userspace/posix/interfaces.c): the card comes
/// up with no address, and ip(8) sets it up with what QEMU's DHCP server
/// offered.
fn interfaces(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_ahead("spark: started /system/tests/posix/interfaces as process ")?;
    if let Some(line) = vm.log().lines().find(|line| line.contains("Net: net0 has the address")) {
        return Err(format!("the card had an address before ip gave it one: {}", line.trim()));
    }
    vm.expect("interfaces: 10.0.2.2 offered 10.0.2.15 to net0, which had no address")?;
    vm.expect("interfaces: a connection from 10.0.2.15 to 10.0.2.100 port 7 echoed what it sent")?;
    let code = program_line(vm, "(/system/tests/posix/interfaces) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> =
        log.lines().filter(|line| line.contains("interfaces: FAILED: ")).collect();
    if !failed.is_empty() || !log.contains("interfaces: every check passed") || code != "0x0" {
        return Err(format!("the interfaces test failed ({code}): {}", failed.join("; ")));
    }
    // ip's changes, each as the kernel logs it: the address, the router,
    // the router gone and the address gone.
    let set = |what: &str| log.lines().filter(|line| line.trim_end().ends_with(what)).count();
    for (what, times) in [
        ("Net: net0 is set to 1 address", 2),
        ("Net: net0 is set to 1 address, with a router", 1),
        ("Net: net0 is set to 0 addresses", 1),
    ] {
        if set(what) != times {
            return Err(format!("the kernel logged \"{what}\" {} times, not {times}", set(what)));
        }
    }
    vm.expect("spark: nothing more to do; waiting")
}

/// Spark starts netd (userspace/spark/tests/services/dhcp) and
/// userspace/posix/dhcp.c beside it, on a virtio card with no address
/// (ADR 0191): netd's lease and what it said of it, netd started again
/// going on with the lease, and the settings file's address and DHCP's
/// again; the program checks what the card has at each step.
fn dhcp(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect("spark: 1 services in /system/tests/services/dhcp, 1 enabled")?;
    // netd and the program run at once: their lines are read ahead.
    for line in [
        "netd: net0: asking for an address by DHCP",
        "netd: net0 has 10.0.2.15/24 from 10.0.2.2 for 1 day, router 10.0.2.2, DNS 10.0.2.3",
        "dhcp: netd gave net0 10.0.2.15/24, the router and the DNS server",
        "netd: net0 goes on with its lease of 10.0.2.15/24 from 10.0.2.2",
        "netd: reading /system/settings/network again",
        "dhcp: the settings set net0 up, and DHCP again once they said nothing",
    ] {
        vm.expect_ahead(line)?;
    }
    let code = program_line(vm, "(/system/tests/posix/dhcp) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> = log.lines().filter(|line| line.contains("dhcp: FAILED: ")).collect();
    if !failed.is_empty() || !log.contains("dhcp: every check passed") || code != "0x0" {
        return Err(format!("the dhcp test failed ({code}): {}", failed.join("; ")));
    }
    Ok(())
}

fn sockets(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("spark: built the hierarchy of 9 directories and mounted 9 filesystems")?;
    // Both come during the PCI scan, among lines boot_report reads past.
    let log = vm.log();
    for needle in [
        "Net: lo is the loopback interface, at 127.0.0.1/8 and every card's address",
        "Net: net0 has the address 10.0.2.15/24, through the router 10.0.2.2",
    ] {
        if !log.contains(needle) {
            return Err(format!("no \"{needle}\" in the boot log"));
        }
    }
    vm.expect("sockets: the network capability covers 65536 ports from 0")?;
    vm.expect(
        "sockets: derived one capability to connect out and one to listen on ports 8000-8009",
    )?;
    vm.expect("sockets: tcp: connected from port ")?;
    vm.expect(" to port 7000 over lo, and the connection taken")?;
    vm.expect(&format!("sockets: tcp: {} bytes went one way and came in order", 1 << 20))?;
    vm.expect("sockets: tcp: each end shut down in turn, and each read the end")?;
    vm.expect("sockets: tcp: a socket that does not wait connected, and said when it had")?;
    vm.expect("sockets: udp: datagrams went each way over lo, kept their bounds, and said where they came from")?;
    vm.expect("sockets: rights: each method refused what its capability does not allow")?;
    vm.expect(
        "sockets: virtio: a connection to 10.0.2.100 port 7 echoed what went through the card",
    )?;
    let verdict = program_line(vm, "sockets: ")?;
    let log = vm.log();
    let failed: Vec<&str> = log.lines().filter(|line| line.contains("sockets: FAILED: ")).collect();
    if !failed.is_empty() {
        return Err(format!("the sockets program failed: {}", failed.join("; ")));
    }
    if verdict != "every check passed" {
        return Err(format!("the sockets program did not pass: {verdict}"));
    }
    vm.expect("(/system/tests/sockets) exited with code 0x0")?;
    vm.expect("spark: nothing more to do; waiting")?;
    if let Some(line) = vm.log().lines().find(|line| line.contains("memory did not come back")) {
        return Err(format!("the kernel lost a socket's memory: {line}"));
    }
    Ok(())
}

fn stack_overflow(vm: &mut Vm, _case: &Case) -> Result<(), String> {
    vm.expect("selftest stack-overflow: recursing until the stack guard page")?;
    vm.expect("Radix panic on CPU 0")?;
    vm.expect("Double fault (#DF)")?;
    vm.expect("a stack guard page: the kernel stack overflowed")?;
    vm.expect("Backtrace:")?;
    vm.expect("radix::selftest::recurse+0x")?;
    vm.expect_exit(QEMU_EXIT_PANIC)
}

/// Checks a SMEP or SMAP fault: `access` names the access to the test page.
fn user_page_fault(vm: &mut Vm, name: &str, access: &str, why: &str) -> Result<(), String> {
    vm.expect(&format!("selftest {name}: "))?;
    vm.expect("Radix panic on CPU 0")?;
    vm.expect("Page fault (#PF)")?;
    vm.expect(&format!("while {access} 0x40000000: the page does not allow this access"))?;
    vm.expect(why)?;
    vm.expect_exit(QEMU_EXIT_PANIC)
}

fn smep(vm: &mut Vm, _case: &Case) -> Result<(), String> {
    user_page_fault(
        vm,
        "smep",
        "executing",
        "SMEP stopped the kernel from running code in a user page",
    )
}

fn smap(vm: &mut Vm, _case: &Case) -> Result<(), String> {
    user_page_fault(vm, "smap", "reading", "SMAP stopped the kernel from touching a user page")
}

fn page_fault(vm: &mut Vm, _case: &Case) -> Result<(), String> {
    vm.expect("selftest page-fault: touching an unmapped address")?;
    vm.expect("Radix panic on CPU 0")?;
    vm.expect("Page fault (#PF)")?;
    vm.expect("while writing 0xdead0000: the page is not mapped")?;
    vm.expect("Registers:")?;
    vm.expect("cr2 00000000dead0000")?;
    vm.expect("Backtrace:")?;
    vm.expect("self_test_fault")?;
    vm.expect("radix::selftest::fault+0x")?;
    vm.expect("radix_kernel_main+0x")?;
    vm.expect("Press a key to restart")?;
    vm.expect_screen(&[
        "RADIX KERNEL PANIC",
        "Radix panic on CPU 0",
        "Page fault (#PF)",
        "radix::selftest::fault+0x",
        "Recent kernel log:",
        "Press a key to restart",
    ])?;
    // A key on the serial port restarts the machine, which ends QEMU.
    vm.send("\r")?;
    vm.expect_exit(QEMU_EXIT_RESET)
}

fn crash_reboot(vm: &mut Vm, case: &Case) -> Result<(), String> {
    // The log goes at the first 2 MiB boundary clear of what the kernel
    // reserved before it, from 4 GiB when there is RAM there, otherwise
    // from 64 MiB. Under BIOS, GRUB loads the boot archive below 64 MiB,
    // and the archive reaches past it.
    let floor: u64 = if case.memory == "1G" { 64 << 20 } else { 1 << 32 };
    let first = reservations(vm)?;
    let (place, before) = first.split_last().ok_or("the kernel reserved no crash log")?;
    let mut start = floor;
    while let Some(&(_, end)) = before.iter().find(|&&(s, e)| s < start + (1 << 20) && start <= e) {
        start = (end + 1).next_multiple_of(2 << 20);
    }
    if *place != (start, start + 0xf_ffff) {
        return Err(format!("the crash log is at {:#x}, not at {start:#x}", place.0));
    }
    vm.expect("Crash log: boot 1;")?;
    vm.expect("selftest crash-reboot: panicking now")?;
    vm.expect("Radix panic on CPU 0")?;
    vm.expect("selftest crash-reboot: deliberate panic")?;
    // The machine restarts and boots again, and finds the log where it
    // left it.
    if reservations(vm)? != first {
        return Err("the second boot reserved other memory, so its crash log moved".into());
    }
    vm.expect("Crash log: boot 2; the previous boot (1) ended with a kernel panic")?;
    vm.expect("| [")?;
    vm.expect("selftest crash-reboot: deliberate panic")?;
    vm.expect("selftest crash-reboot: passed")?;
    vm.expect("Boot complete.")?;
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

/// The kernel's list of the memory it reserved at boot, up to the crash
/// log, which is last: each range's start and its last byte.
fn reservations(vm: &mut Vm) -> Result<Vec<(u64, u64)>, String> {
    vm.expect("Memory: reserved by the kernel:")?;
    let from = vm.cursor;
    vm.expect("  crash log")?;
    let hex = |text: &str| u64::from_str_radix(text.strip_prefix("0x")?, 16).ok();
    let log = vm.log();
    log[from..vm.cursor]
        .lines()
        .filter_map(|line| line.split_whitespace().find(|word| word.starts_with("0x")))
        .map(|range| {
            range
                .split_once('-')
                .and_then(|(start, end)| Some((hex(start)?, hex(end)?)))
                .ok_or_else(|| format!("a reserved range reads `{range}`"))
        })
        .collect()
}

fn kdb(vm: &mut Vm, _case: &Case) -> Result<(), String> {
    vm.expect("selftest breakpoint: running a breakpoint instruction")?;
    vm.expect("kdb on CPU 0, breakpoint at ")?;
    vm.expect("kdb> ")?;
    vm.kdb("regs", &["rip", "rflags", "cr3"])?;
    vm.kdb("bt", &["radix::selftest::breakpoint", "radix_kernel_main"])?;
    vm.send("continue\r")?;
    vm.expect("kdb: continuing")?;
    vm.expect("selftest breakpoint: passed")?;
    vm.expect("Boot complete.")?;
    vm.expect("kdb on CPU 0, boot complete")?;
    vm.expect("kdb> ")?;

    let names: Vec<&str> = radix_kdb::COMMANDS.iter().map(|c| c.name).collect();
    vm.kdb("help", &names)?;
    vm.kdb("cpus", &["2 CPUs", "cpu0: APIC ID", "cpu1: APIC ID", "timer interrupts"])?;
    vm.kdb(
        "mem",
        &[
            "Memory map:",
            "usable",
            "Reserved by the kernel:",
            "kernel image",
            "Frame allocator zones:",
            "node 0 DMA32",
            "KASLR: kernel moved by",
            "Paging: W^X holds",
        ],
    )?;
    vm.kdb("heap", &["Heap: ", " objects in use"])?;
    vm.kdb("caps", &["Capabilities: 0 spaces with 0 slots, 0 capabilities to 0 objects"])?;
    vm.kdb("threads", &[" threads (id, name, CPU, state", "kmain", "idle/0", "reaper", "work/1"])?;
    vm.kdb("sched", &["Scheduler: ", "grace periods", "cpu0: ", "cpu1: "])?;
    vm.kdb(
        "domains",
        &["Domains (class, CPU weight", "system", "Out of memory: 0 domains stopped"],
    )?;
    vm.kdb("locks", &["Lock checker: "])?;
    vm.kdb("acpi", &["ACPI: RSDP", "APIC"])?;
    vm.kdb("log 4", &["Boot complete."])?;
    vm.kdb("trace", &["boot.entry", "events shown"])?;
    vm.kdb("regs", &["no saved registers"])?;
    vm.kdb("sym radix_kernel_main", &["radix_kernel_main is at 0x"])?;
    let log = vm.log();
    let address = log
        .rsplit("radix_kernel_main is at 0x")
        .next()
        .and_then(|rest| rest.split_whitespace().next())
        .ok_or("no address in the `sym` answer")?
        .to_string();
    vm.kdb(&format!("sym 0x{address}"), &["is radix_kernel_main"])?;
    vm.kdb(&format!("x 0x{address} 32"), &[&format!("{address:0>16} ")])?;
    vm.kdb("x 0xdead0000 16", &["not mapped"])?;
    vm.kdb("frobnicate", &["unknown command `frobnicate`"])?;
    vm.send("continue\r")?;
    vm.expect("kdb: continuing")?;
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

/// A scheduler self-test: the boot report, `what` in the result, a pass.
fn sched_case(vm: &mut Vm, case: &Case, name: &str, what: &str) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect(&format!("selftest {name}: "))?;
    selftest_passed(vm, name)?;
    if !vm.log().contains(what) {
        return Err(format!("selftest {name} passed, but its report lacks \"{what}\""));
    }
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn fpu(vm: &mut Vm, case: &Case) -> Result<(), String> {
    sched_case(vm, case, "fpu", "threads kept their vector registers through ")
}

fn sched_fair(vm: &mut Vm, case: &Case) -> Result<(), String> {
    sched_case(vm, case, "sched-fair", "4 equal threads got ")
}

fn sched_rt(vm: &mut Vm, case: &Case) -> Result<(), String> {
    sched_case(vm, case, "sched-rt", "wake-ups preempt by priority in 200 of 200 rounds")
}

fn sched_pi(vm: &mut Vm, case: &Case) -> Result<(), String> {
    sched_case(vm, case, "sched-pi", "the holder was lent priority 30")
}

fn sched_balance(vm: &mut Vm, case: &Case) -> Result<(), String> {
    sched_case(vm, case, "sched-balance", "threads started on CPU 3 spread ")
}

fn lockdep(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("selftest lockdep: seeding an AB-BA deadlock between spin locks")?;
    vm.expect("Lock checker: possible deadlock: two locks are taken in both orders")?;
    vm.expect("Lock checker:   taking ")?;
    vm.expect("Lock checker:   while holding ")?;
    vm.expect("selftest lockdep: seeding an AB-BA deadlock between mutexes")?;
    vm.expect("Lock checker: possible deadlock: two locks are taken in both orders")?;
    vm.expect("selftest lockdep: seeding a mutex taken under a spin lock")?;
    vm.expect("Lock checker: sleeping where it is not allowed")?;
    selftest_passed(vm, "lockdep")?;
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn user(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    selftest_passed(vm, "user")?;
    // The kernel reports each fault as it ends the program that made it.
    let log = vm.log();
    for fault in [
        "(fault) ended: page fault reading 0x0 (not mapped)",
        "(fault) ended: page fault writing ",
        "(fault) ended: page fault executing ",
        "(fault) ended: protection fault",
        "(fault) ended: invalid instruction",
        "(fault) ended: breakpoint",
        "(fault) ended: division error",
    ] {
        if !log.contains(fault) {
            return Err(format!("no fault report \"{fault}\""));
        }
    }
    // The probe's log lines: control characters and bad UTF-8 shown as
    // `?`, and a long line cut at 256 bytes.
    let cut = "]: probe: a line with a tab?and a bell?, cut past 256 bytes ";
    let Some(line) = log.lines().find(|line| line.contains(cut)) else {
        return Err("the probe's line with control characters is missing or unescaped".into());
    };
    let text = line.split_once("]: ").map_or("", |(_, text)| text);
    if text.len() != 256 {
        return Err(format!("the probe's long line is {} bytes, not cut at 256", text.len()));
    }
    if !log.contains("]: probe: bad UTF-8 ?? here") {
        return Err("the probe's line with bad UTF-8 is missing or unescaped".into());
    }
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn ipc(vm: &mut Vm, case: &Case) -> Result<(), String> {
    sched_case(vm, case, "ipc", "10 pairs of programs ping-ponged")
}

fn fuzz(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    let passed = selftest_passed(vm, "fuzz");
    let log = vm.log();
    // The fuzzers' lines name the call (and a fuzzer stopped while waiting
    // cannot report through its exit status). Only a line that starts with
    // the marker counts: the fuzzers also log text of their own.
    for (marker, what) in [
        ("fuzz: broken promise", "a failed call returned more than an error"),
        ("fuzz: undocumented error", "a call failed with an error its documentation does not list"),
    ] {
        let starts =
            |line: &&str| line.split_once("]: ").is_some_and(|(_, text)| text.starts_with(marker));
        if let Some(line) = log.lines().find(starts) {
            return Err(format!("{what}: {}", line.trim()));
        }
    }
    passed?;
    if !log.contains("fuzzers got through their calls") {
        return Err("selftest fuzz passed, but its report lacks the fuzzers' count".into());
    }
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

/// A program's line that starts with `start`: the rest of it, once the
/// line has ended.
fn program_line(vm: &mut Vm, start: &str) -> Result<String, String> {
    vm.expect(start)?;
    let from = vm.cursor;
    vm.expect("\n")?;
    Ok(vm.log()[from..vm.cursor].trim().to_string())
}

/// A program's bias from its "moved by 0x..." line: it must put the
/// program, linked at 0, in the image window of radix-startup(2).
fn image_bias(rest: &str, who: &str) -> Result<u64, String> {
    const IMAGE_BASE: u64 = 0x1000_0000_0000;
    const WINDOW: u64 = 0x100_0000_0000;
    let hex = rest.rsplit("0x").next().unwrap_or_default();
    let bias = u64::from_str_radix(hex.trim(), 16).map_err(|_| format!("{who}: bias `{rest}`"))?;
    if !(IMAGE_BASE..IMAGE_BASE + WINDOW).contains(&bias) || bias % 4096 != 0 {
        return Err(format!("{who} was placed at {bias:#x}, outside the image window"));
    }
    Ok(bias)
}

fn spark(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect("Init: the boot archive is ")?;
    vm.expect("Init: /system/commands/spark is process ")?;
    let rest = program_line(vm, "spark: Spark v2 is process ")?;
    let spark = image_bias(&rest, "Spark")?;
    vm.expect("spark: boot archive of ")?;
    vm.expect(" KiB with ")?;
    vm.expect(" files at 0x")?;
    vm.expect("spark: built the hierarchy of 9 directories and mounted 9 filesystems")?;
    // Spark's line and hello's first come in either order: hello may run
    // on another CPU before Spark logs that it started it.
    let from = vm.cursor;
    let number = program_line(vm, "spark: started /system/tests/hello as process ")?;
    vm.cursor = from;
    let args = program_line(vm, "hello: process ")?;
    if args != format!("{number}, arguments: /system/tests/hello one two") {
        return Err(format!("hello got `{args}`, not process {number} with its arguments"));
    }
    let rest = program_line(vm, "hello: placed at a random address, moved by ")?;
    let hello = image_bias(&rest, "hello")?;
    if hello == spark {
        return Err(format!("Spark and hello were both placed at {hello:#x}"));
    }
    vm.expect("hello: the heap holds 1000 numbers summing to 500500 and 458752 bytes of sevens")?;
    vm.expect("hello: goodbye")?;
    vm.expect(&format!("spark: process {number} (/system/tests/hello) exited with code 0x0"))?;
    vm.expect(&format!("spark: reaped process {number}; all of its memory came back"))?;
    vm.expect("spark: nothing more to do; waiting")
}

/// The POSIX test programs of userspace/posix: Spark starts their runner
/// on a pseudo-terminal, which runs each as a job of its own.
fn posix(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_ahead("spark: started /system/tests/posix/run as process ")?;
    vm.expect(", on a pseudo-terminal")?;
    let code = program_line(vm, "(/system/tests/posix/run) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> =
        log.lines().filter(|line| line.contains(": FAILED")).map(str::trim).collect();
    if !failed.is_empty() {
        return Err(format!("POSIX tests failed: {}", failed.join("; ")));
    }
    let summary = log
        .lines()
        .find_map(|line| line.split("posix: ").nth(1).filter(|rest| rest.ends_with("tests passed")))
        .ok_or("the runner never said how many tests passed")?;
    let counts: Vec<&str> = summary.split(' ').collect();
    if counts.len() != 5 || counts[0] != counts[2] || code != "0x0" {
        return Err(format!("the runner said \"{summary}\" and exited with code {code}"));
    }
    vm.expect("spark: nothing more to do; waiting")
}

/// The kernel's lines for a case's disks, up to the end of the boot.
/// The disks are found while the other CPUs start, so their lines come
/// among those `boot_report` checks: looked for anywhere before here.
fn disks_found(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    let log = vm.log();
    let line_of = |start: &str| {
        log.lines()
            .find_map(|line| line.find(start).map(|at| &line[at..]))
            .ok_or_else(|| format!("the kernel never said \"{start}...\""))
    };
    let lanes = case.cpus.clamp(1, 16);
    for n in nvme_controllers(case.disks) {
        let line = line_of(&format!("Disk: nvme{n} is an NVMe 1.4 controller at "))?;
        // QEMU's controller takes shadow doorbells and scatter gather
        // lists.
        let queues = if lanes == 1 {
            ", 1 I/O queue of 63 commands, one MSI-X vector, shadow doorbells, scatter gather \
             lists"
                .to_string()
        } else {
            format!(
                ", {lanes} I/O queues of 63 commands, an MSI-X vector for each of {lanes} CPUs, \
                 shadow doorbells, scatter gather lists"
            )
        };
        if !line.contains(": QEMU NVMe Ctrl, firmware ") || !line.ends_with(&queues) {
            return Err(format!("the kernel described nvme{n} as \"{line}\""));
        }
    }
    let mut port = 0;
    for (disk, name) in case.disks.iter().zip(disk_names(case.disks)) {
        let size = disk_size(disk.kib);
        // A disk of 512-byte sectors says nothing of them.
        let sectors = match disk.sector {
            512 => String::new(),
            size => format!(", {size}-byte sectors"),
        };
        let ro = if disk.read_only { ", read-only" } else { "" };
        let (line, want) = match disk.bus {
            Bus::Virtio => (
                line_of(&format!("Disk: {name} is a virtio block device of {size} at "))?,
                format!(
                    ", {} bytes a block{sectors}{ro}, write cache, discards, MSI-X vector ",
                    disk.sector
                ),
            ),
            Bus::Sata => {
                port += 1;
                (
                    line_of(&format!(
                        "Disk: {name} is a SATA disk of {size} on AHCI port {}: QEMU HARDDISK, ",
                        port - 1
                    ))?,
                    "512 bytes a block, NCQ depth 32, write cache, TRIM".to_string(),
                )
            }
            Bus::Nvme(_) => (
                line_of(&format!("Disk: {name} is an NVMe namespace of {size}: QEMU NVMe Ctrl, "))?,
                format!("{} bytes a block{sectors}{ro}, write cache, deallocates", disk.sector),
            ),
            Bus::Usb(port) => (
                line_of(&format!(
                    "Disk: {name} is a USB disk of {size} on {}: QEMU QEMU HARDDISK ",
                    usb_place(port)
                ))?,
                format!("{} bytes a block{sectors}{ro}, write cache", disk.sector),
            ),
            Bus::Uas(port, lun) => (
                line_of(&format!(
                    "Disk: {name} is a USB disk of {size} on {}: QEMU QEMU HARDDISK ",
                    uas_place(case.disks, port, lun)
                ))?,
                format!(
                    "{} bytes a block{sectors}{ro}, write cache, USB Attached SCSI",
                    disk.sector
                ),
            ),
        };
        // Only a SATA disk's line says whether it is solid state.
        let says_solid = disk.bus == Bus::Sata && disk.solid();
        if !line.contains(&want) || line.ends_with(", solid state") != says_solid {
            return Err(format!("the kernel described {name} as \"{line}\""));
        }
        let part = partition_name(&name, 1);
        let size = disk_size(partition_sectors(disk) / 2);
        match disk.table {
            Table::None => {
                line_of(&format!("Disk: {name} has no partition table; it is used whole"))?;
            }
            Table::Mbr => {
                line_of(&format!("Disk: {name} has an MBR with 1 partition"))?;
                line_of(&format!("Disk: {part}: {size} from sector {PARTITION_START}, type 0x0c"))?;
            }
            Table::Gpt => {
                line_of(&format!("Disk: {name} has a GPT with 1 partition"))?;
                line_of(&format!(
                    "Disk: {part}: {size} from sector {PARTITION_START}, GPT \"{GPT_NAME}\""
                ))?;
            }
        }
    }
    let on = |bus: Bus| case.disks.iter().filter(|disk| disk.bus == bus).count();
    let plural =
        |count: usize, what: &str| format!("{count} {what}{}", if count == 1 { "" } else { "s" });
    line_of(&format!(
        "; {}, {}, {}",
        plural(on(Bus::Virtio), "virtio disk"),
        plural(on(Bus::Sata), "SATA disk"),
        plural(nvme_controllers(case.disks).len(), "NVMe controller")
    ))?;
    Ok(())
}

/// The kernel's and Spark's lines for the storage test's disks, up to
/// the start of the program.
fn storage_boot(vm: &mut Vm, case: &Case) -> Result<(), String> {
    volumes_mounted(vm, case)?;
    vm.expect_ahead("spark: started /system/tests/storage as process ")
}

/// The disks found, and Spark mounting each one's filesystem at
/// `/volumes/<name>`.
fn volumes_mounted(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    // Spark mounts the disks while it builds the hierarchy, to find
    // Dukovo's root among them (ADR 0137).
    for (disk, name) in case.disks.iter().zip(volume_names(case.disks)) {
        let label = disk.label;
        let (kind, said) = disk.kind();
        vm.expect(&format!(
            "Files: mounted {kind} from {name} on /volumes/{name}, labelled {label}"
        ))?;
        vm.expect(&format!("spark: mounted the {said} filesystem of {name} at /volumes/{name}"))?;
    }
    vm.expect("spark: built the hierarchy of 9 directories and mounted 9 filesystems")
}

/// The end of the storage program: no failed checks, and exit code 0.
/// Its verdict is read once its whole line has arrived: the serial port
/// may have brought only the start of it when that is first seen.
fn storage_end(vm: &mut Vm, mode: &str) -> Result<(), String> {
    let verdict = program_line(vm, &format!("storage: {mode}: "))?;
    let log = vm.log();
    let failed: Vec<&str> = log.lines().filter(|line| line.contains("storage: FAILED: ")).collect();
    if !failed.is_empty() {
        return Err(format!("the storage program failed: {}", failed.join("; ")));
    }
    if verdict != "every check passed" {
        return Err(format!("the storage program did not pass every check of `{mode}`: {verdict}"));
    }
    vm.expect("(/system/tests/storage) exited with code 0x0")?;
    vm.expect("spark: nothing more to do; waiting")
}

/// A disk's size as the kernel logs it: in the largest binary unit it
/// reaches, with a tenth when it is not a whole number of them.
fn disk_size(kib: u64) -> String {
    let (unit, shift) = if kib >= 1 << 20 { ("GiB", 20) } else { ("MiB", 10) };
    let (whole, tenths) = (kib >> shift, ((kib & ((1 << shift) - 1)) * 10) >> shift);
    if tenths == 0 { format!("{whole} {unit}") } else { format!("{whole}.{tenths} {unit}") }
}

/// The first boot of `btrfs-stress`: torture (userspace/posix/torture.c)
/// has two workers on each volume make random changes for two minutes,
/// all four at once, each checking what it did and syncing it.
fn torture_run(vm: &mut Vm, case: &Case) -> Result<(), String> {
    volumes_mounted(vm, case)?;
    vm.expect_ahead("spark: started /system/tests/posix/torture as process ")?;
    vm.expect("torture: 4 workers making changes for 120 s, seed 0x5eed")?;
    vm.expect("torture: done in ")?;
    torture_end(vm, case, "everything checked and synced")
}

/// The second boot of `btrfs-stress`: torture works out each worker's
/// changes again, checks that the volumes hold what they left, and
/// removes it all.
fn torture_check(vm: &mut Vm, case: &Case) -> Result<(), String> {
    volumes_mounted(vm, case)?;
    vm.expect_ahead("spark: started /system/tests/posix/torture as process ")?;
    torture_end(vm, case, "removed")
}

/// The end of torture: each worker's last line, which ends with `done`,
/// whatever order the workers ended in; no failed checks, and exit code
/// 0.
fn torture_end(vm: &mut Vm, case: &Case, done: &str) -> Result<(), String> {
    let code = program_line(vm, "(/system/tests/posix/torture) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> = log.lines().filter(|line| line.contains("torture: FAILED: ")).collect();
    if !failed.is_empty() {
        return Err(format!("torture failed: {}", failed.join("; ")));
    }
    for name in volume_names(case.disks) {
        for worker in 0..2 {
            let start = format!("torture: /volumes/{name}/torture/w{worker}: ");
            if !log.lines().any(|line| line.contains(&start) && line.trim_end().ends_with(done)) {
                return Err(format!("torture's worker did not say \"{start}...{done}\""));
            }
        }
    }
    if code != "0x0" || !log.contains("torture: every check passed") {
        return Err(format!("torture exited with code {code}"));
    }
    vm.expect("spark: nothing more to do; waiting")
}

/// Nothing to do between the boots.
fn nothing_between(_: &Case, _: &[DiskImage]) -> Result<(), String> {
    Ok(())
}

/// The first boot: the program writes its files to every volume.
fn storage_write(vm: &mut Vm, case: &Case) -> Result<(), String> {
    storage_boot(vm, case)?;
    let volumes = volume_names(case.disks);
    let filesystems: Vec<&Disk> = case.disks.iter().filter(|disk| disk.fat != 0).collect();
    for (name, disk) in volumes.iter().zip(&filesystems) {
        let (kind, _) = disk.kind();
        vm.expect(&format!("storage: /volumes/{name}: {kind}, "))?;
        vm.expect(&format!("storage: /volumes/{name}: wrote "))?;
    }
    vm.expect("storage: /tmp: a 10000-byte file written, read back and removed")?;
    vm.expect("storage: /devices: ")?;
    vm.expect(" of them disks; zero, random and null work")?;
    for (name, disk) in volumes.iter().zip(&filesystems) {
        vm.expect(&format!("storage: /kernel/block/{name}/size: "))?;
        let model = match disk.bus {
            Bus::Sata => "QEMU HARDDISK",
            Bus::Nvme(_) => "QEMU NVMe Ctrl",
            Bus::Usb(_) | Bus::Uas(..) => "QEMU QEMU HARDDISK",
            Bus::Virtio => "",
        };
        if disk.table == Table::None && !model.is_empty() {
            vm.expect(&format!("storage: /kernel/block/{name}/device: {model}, serial "))?;
        }
        if disk.table != Table::None {
            vm.expect(&format!(
                "storage: /kernel/block/{name}: partition 1 from sector {PARTITION_START}"
            ))?;
        }
    }
    vm.expect(&format!("storage: /kernel/version: Radix {VERSION} ({RELEASE} {VERSION})"))?;
    vm.expect("storage: /kernel/bus/pci/devices: ")?;
    let cpus = if case.cpus > 1 { format!("0-{}", case.cpus - 1) } else { "0".into() };
    vm.expect(&format!(
        "storage: /kernel/devices/system/cpu: possible {cpus}, present {cpus}, online {cpus}"
    ))?;
    // QEMU's CPUs pick no speed of their own, even under KVM.
    vm.expect("storage: /kernel/devices/system/cpu/cpufreq: 0 policies")?;
    vm.expect("storage: /processes lists ")?;
    vm.expect("1 spark (running)")?;
    vm.expect("storage (running)")?;
    // Spark's ten, and the volumes.
    let fat = filesystems.iter().filter(|disk| disk.kind().0 == "fat").count();
    vm.expect(&format!(
        "storage: /processes/mounts lists {} mounts, {fat} of them FAT",
        10 + volumes.len()
    ))?;
    vm.expect("storage: /processes/meminfo: ")?;
    storage_end(vm, "write")
}

/// Between the boots: the host reads what the program wrote, with
/// mtools from a FAT disk, to which it then writes a file of its own,
/// from beside the disks in out/test (not the shared temporary directory,
/// where another user could put a link in its place); from an exFAT disk
/// with The Sleuth Kit; from a Btrfs disk with Linux's `btrfs restore`,
/// whose file of the host's `mkfs.btrfs` put there; and from an ext4 disk
/// with `debugfs`, whose file `mkfs.ext4` put there.
fn storage_host(case: &Case, disks: &[DiskImage]) -> Result<(), String> {
    let Some(first) = disks.first() else { return Err("the case has no disks".into()) };
    let source = first.path.with_file_name("storage-from-host.txt");
    std::fs::write(&source, STORAGE_FROM_HOST)
        .map_err(|err| format!("writing {}: {err}", source.display()))?;
    let filesystems = case.disks.iter().filter(|disk| disk.fat != 0);
    let result = disks.iter().zip(filesystems).zip(volume_names(case.disks)).try_for_each(
        |((disk, made), name)| {
            if made.btrfs() {
                btrfs_written(disk, &name)
            } else if made.exfat() {
                exfat_written(disk, &name)
            } else if made.ext4() {
                ext4_written(disk, &name)
            } else {
                fat_written(disk, &name, &source)
            }
        },
    );
    let _ = std::fs::remove_file(&source);
    result
}

/// [`storage_host`] for a FAT disk.
fn fat_written(disk: &DiskImage, name: &str, source: &Path) -> Result<(), String> {
    let image = mtools_image(disk);
    let read = |path: &str| {
        host_tool(std::process::Command::new("mtype").args(["-i", &image, &format!("::/{path}")]))
    };
    let listing =
        host_tool(std::process::Command::new("mdir").args(["-i", &image, "-b", "::/many"]))?;
    // FAT names match without regard to case.
    let txt =
        |line: &&str| Path::new(line).extension().is_some_and(|e| e.eq_ignore_ascii_case("txt"));
    let many = String::from_utf8_lossy(&listing).lines().filter(txt).count();
    storage_written(&image, name, &read, many)?;
    let to = format!("::/{STORAGE_FROM_HOST_NAME}");
    host_tool(std::process::Command::new("mcopy").args(["-i", &image]).arg(source).arg(to))?;
    Ok(())
}

/// [`storage_host`] for an exFAT disk: its files as The Sleuth Kit, an
/// exFAT reader of its own, reads them (`fls` lists them and `icat`
/// copies each out), and none of those the program renamed or removed.
/// The host writes no file of its own there: no tool on Linux writes to
/// exFAT without mounting it, which takes the administrator.
fn exfat_written(disk: &DiskImage, name: &str) -> Result<(), String> {
    let image = disk.path.display().to_string();
    let mut fls = std::process::Command::new("fls");
    fls.args(["-r", "-p", "-u", "-F", "-f", "exfat"]).arg(&disk.path);
    let listing = String::from_utf8_lossy(&host_tool(&mut fls)?).into_owned();
    // `r/r 323:\tnotes/final.txt`: the entry's number, then its path.
    let files: BTreeMap<&str, &str> = listing
        .lines()
        .filter_map(|line| {
            let (number, path) = line.split_once(":\t")?;
            Some((path, number.rsplit(' ').next()?))
        })
        .collect();
    let read = |path: &str| {
        let number =
            files.get(path).ok_or_else(|| format!("{image}: The Sleuth Kit finds no {path}"))?;
        host_tool(
            std::process::Command::new("icat").args(["-f", "exfat"]).arg(&disk.path).arg(number),
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
    Ok(())
}

/// [`storage_host`] for an ext4 disk: its files as `debugfs` copies them
/// out, the host's file too, and none of those the program renamed or
/// removed.
fn ext4_written(disk: &DiskImage, name: &str) -> Result<(), String> {
    let restored = ext4_dump(disk)?;
    let image = disk.path.display().to_string();
    let read = |path: &str| {
        std::fs::read(restored.join(path))
            .map_err(|err| format!("{image}: {path}, as debugfs copies it out: {err}"))
    };
    let result = (|| {
        let many = std::fs::read_dir(restored.join("many"))
            .map_err(|err| format!("{image}: many/: {err}"))?
            .filter(|entry| {
                entry
                    .as_ref()
                    .is_ok_and(|e| Path::new(&e.file_name()).extension() == Some("txt".as_ref()))
            })
            .count();
        storage_written(&image, name, &read, many)?;
        if read(STORAGE_FROM_HOST_NAME)? != STORAGE_FROM_HOST {
            return Err(format!("{image}: the host's file is not as mkfs.ext4 wrote it"));
        }
        for gone in ["draft.txt", "scratch.txt"] {
            if restored.join(gone).exists() {
                return Err(format!("{image}: {gone} is still there"));
            }
        }
        storage_access(&image, &ext4_access(disk)?)
    })();
    let _ = std::fs::remove_dir_all(&restored);
    result
}

/// The mode, owner and group of each file the storage program gives one
/// (ADR 0213), as Linux's tools read them, and as the program set them:
/// its set-user-ID bit gone after the chown, and the set-group-ID
/// directory's group on what is made in it.
const STORAGE_ACCESS: [(&str, u32, u32, u32); 5] = [
    ("hello.txt", 0o644, 0, 0),
    ("notes/final.txt", 0o640, 1000, 100),
    ("cut.txt", 0o755, 1000, 0),
    ("many", 0o2755, 0, 100),
    ("many/file-07.txt", 0o644, 0, 100),
];

/// Checks that `found` has each of [`STORAGE_ACCESS`]'s files with its
/// mode, owner and group; `image` names the disk.
fn storage_access(image: &str, found: &BTreeMap<String, (u32, u32, u32)>) -> Result<(), String> {
    for (path, mode, owner, group) in STORAGE_ACCESS {
        match found.get(path) {
            Some(&got) if got == (mode, owner, group) => {}
            Some((m, o, g)) => {
                return Err(format!(
                    "{image}: {path} has mode {m:o}, owner {o} and group {g}, not {mode:o}, \
                     {owner} and {group}"
                ));
            }
            None => return Err(format!("{image}: Linux's tools find no {path}")),
        }
    }
    Ok(())
}

/// The permission bits, owner and group of each of [`STORAGE_ACCESS`]'s
/// files on an ext4 disk, as `debugfs`'s `stat` reads them once its
/// journal is replayed: `Mode:  0640` and `User:  1000   Group:   100`.
fn ext4_access(disk: &DiskImage) -> Result<BTreeMap<String, (u32, u32, u32)>, String> {
    let replayed = ext4_replayed(disk)?;
    let result = STORAGE_ACCESS
        .iter()
        .map(|&(path, ..)| {
            let mut debugfs = std::process::Command::new(fat_tool("debugfs"));
            debugfs.arg("-R").arg(format!("stat /{path}")).arg(&replayed);
            let said = host_tool(debugfs.stdin(Stdio::null()))?;
            let said = String::from_utf8_lossy(&said);
            let words: Vec<&str> = said.split_whitespace().collect();
            let after = |label: &str, radix: u32| {
                let at = words.iter().position(|word| *word == label)?;
                u32::from_str_radix(words.get(at + 1)?, radix).ok()
            };
            match (after("Mode:", 8), after("User:", 10), after("Group:", 10)) {
                (Some(mode), Some(owner), Some(group)) => {
                    Ok((path.to_string(), (mode, owner, group)))
                }
                _ => Err(format!("{}: debugfs's stat of {path} says {said}", disk.path.display())),
            }
        })
        .collect();
    let _ = std::fs::remove_file(&replayed);
    result
}

/// The permission bits, owner and group of the files on a Btrfs disk's
/// top subvolume, by name (a path for [`STORAGE_ACCESS`]'s), as `btrfs
/// inspect-internal dump-tree` prints its inode items and their names:
/// `key (258 INODE_ITEM 0)`, then `... mode 100640 links 1 uid 1000 gid
/// 100 ...`, and `key (258 INODE_REF 257)`, then `... name: final.txt`.
fn btrfs_access(disk: &DiskImage) -> Result<BTreeMap<String, (u32, u32, u32)>, String> {
    let tool = disk.btrfs.as_ref().ok_or("a Btrfs disk was made without the btrfs tool")?;
    let mut cmd = std::process::Command::new(tool);
    cmd.args(["inspect-internal", "dump-tree", "-t", "fs"]).arg(&disk.path);
    let dump = host_tool(cmd.stdin(Stdio::null()))?;
    let dump = String::from_utf8_lossy(&dump);
    let mut inodes: BTreeMap<u64, (u32, u32, u32)> = BTreeMap::new();
    let mut names: BTreeMap<String, u64> = BTreeMap::new();
    let mut key: Option<(u64, &str)> = None;
    for line in dump.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if let Some(at) = words.iter().position(|word| *word == "key") {
            let number = words.get(at + 1).and_then(|w| w.trim_start_matches('(').parse().ok());
            key = number.zip(words.get(at + 2).copied());
            continue;
        }
        let after = |label: &str, radix: u32| {
            let at = words.iter().position(|word| *word == label)?;
            u32::from_str_radix(words.get(at + 1)?, radix).ok()
        };
        match key {
            Some((ino, "INODE_ITEM")) => {
                if let (Some(mode), Some(uid), Some(gid)) =
                    (after("mode", 8), after("uid", 10), after("gid", 10))
                {
                    inodes.insert(ino, (mode & 0o7777, uid, gid));
                }
            }
            Some((ino, "INODE_REF")) => {
                if let Some(at) = line.find("name: ") {
                    names.insert(line[at + 6..].to_string(), ino);
                }
            }
            _ => {}
        }
    }
    // Each of the program's files by its last name, which is unique.
    Ok(STORAGE_ACCESS
        .iter()
        .filter_map(|&(path, ..)| {
            let name = path.rsplit('/').next()?;
            Some((path.to_string(), *inodes.get(names.get(name)?)?))
        })
        .collect())
}

/// [`storage_host`] for a Btrfs disk: its files as `btrfs restore` copies
/// them out, `grow.bin` and the host's file too, and none of those the
/// program renamed or removed.
fn btrfs_written(disk: &DiskImage, name: &str) -> Result<(), String> {
    let restored = btrfs_restore(disk)?;
    let image = disk.path.display().to_string();
    let read = |path: &str| {
        std::fs::read(restored.join(path))
            .map_err(|err| format!("{image}: {path}, as btrfs restore copies it out: {err}"))
    };
    let result = (|| {
        let many = std::fs::read_dir(restored.join("many"))
            .map_err(|err| format!("{image}: many/: {err}"))?
            .filter(|entry| {
                entry
                    .as_ref()
                    .is_ok_and(|e| Path::new(&e.file_name()).extension() == Some("txt".as_ref()))
            })
            .count();
        storage_written(&image, name, &read, many)?;
        let grow: Vec<u8> =
            (0..STORAGE_GROW).map(|at| storage_big_byte(at).wrapping_add(1)).collect();
        if read("grow.bin")? != grow {
            return Err(format!(
                "{image}: grow.bin as btrfs restore copies it out is not as written"
            ));
        }
        let words: Vec<u8> = (0..STORAGE_WORDS).map(storage_words_byte).collect();
        if read("words.txt")? != words {
            return Err(format!(
                "{image}: words.txt as btrfs restore copies it out is not as written"
            ));
        }
        if read(STORAGE_FROM_HOST_NAME)? != STORAGE_FROM_HOST {
            return Err(format!("{image}: the host's file is not as mkfs.btrfs wrote it"));
        }
        // The second name of hello.txt (`btrfs check` counts its names).
        if read("notes/hello-again.txt")? != STORAGE_HELLO {
            return Err(format!("{image}: hello.txt's second name is not as written"));
        }
        if btrfs_zstd(disk)? != (name == BTRFS_COMPRESSED) {
            return Err(format!(
                "{image}: the superblock's zstd feature is not set if and only if Spark had \
                 the disk compress"
            ));
        }
        for gone in ["draft.txt", "scratch.txt"] {
            if restored.join(gone).exists() {
                return Err(format!("{image}: {gone} is still there"));
            }
        }
        storage_access(&image, &btrfs_access(disk)?)
    })();
    let _ = std::fs::remove_dir_all(&restored);
    result
}

/// Checks what the storage program's `write` wrote to volume `name`, as
/// `read` reads a file of it on the host, with `many` text files in its
/// `many/`; `image` names the disk.
fn storage_written(
    image: &str,
    name: &str,
    read: &dyn Fn(&str) -> Result<Vec<u8>, String>,
    many: usize,
) -> Result<(), String> {
    let same = |path: &str, want: &[u8]| -> Result<(), String> {
        let got = read(path)?;
        if got != want {
            return Err(format!(
                "{image}: {path} holds {} bytes on the host, not the {} written",
                got.len(),
                want.len()
            ));
        }
        Ok(())
    };
    same("hello.txt", STORAGE_HELLO)?;
    same(&format!("notes/{STORAGE_LONG_NAME}"), format!("Saved on {name}.\n").as_bytes())?;
    same("notes/final.txt", b"final\n")?;
    same("log.txt", b"one\ntwo\n")?;
    same("cut.txt", &[b'x'; 1000])?;
    let big = read("big.bin")?;
    let want: Vec<u8> = (0..STORAGE_BIG).map(storage_big_byte).collect();
    if big != want {
        let at =
            big.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(big.len().min(want.len()));
        return Err(format!("{image}: big.bin as the host reads it differs at byte {at}"));
    }
    if many != STORAGE_MANY {
        return Err(format!("{image}: the host finds {many} files in many/, not {STORAGE_MANY}"));
    }
    Ok(())
}

/// Whether a Btrfs disk's superblock has the `COMPRESS_ZSTD` feature,
/// which Radix sets at its first extent compressed with zstd.
fn btrfs_zstd(disk: &DiskImage) -> Result<bool, String> {
    let mut features = [0; 8];
    std::fs::File::open(&disk.path)
        .and_then(|file| {
            std::os::unix::fs::FileExt::read_exact_at(&file, &mut features, 0x10000 + 0xbc)
        })
        .map_err(|err| format!("reading the superblock of {}: {err}", disk.path.display()))?;
    Ok(u64::from_le_bytes(features) & 1 << 4 != 0)
}

/// The byte at `offset` of the program's `words.txt`: lines of 64 bytes,
/// each its number in eight digits and [`STORAGE_WORDS_LINE`].
fn storage_words_byte(offset: usize) -> u8 {
    let (line, at) = (offset / 64, offset % 64);
    match at.checked_sub(8) {
        Some(at) => STORAGE_WORDS_LINE[at],
        None => b'0' + (line / 10usize.pow(7 - at as u32) % 10) as u8,
    }
}

/// The byte at `offset` of the program's `big.bin`, and of a raw test
/// disk before its test.
fn storage_big_byte(offset: usize) -> u8 {
    (offset as u32).wrapping_mul(2_654_435_761).wrapping_add((offset / 4096) as u32).to_le_bytes()
        [3]
}

/// The second boot: the program reads every file back.
fn storage_check(vm: &mut Vm, case: &Case) -> Result<(), String> {
    storage_boot(vm, case)?;
    for name in volume_names(case.disks) {
        vm.expect(&format!("storage: /volumes/{name}: "))?;
        vm.expect(" files read back after the reboot, every byte as written")?;
    }
    vm.expect("storage: /processes lists ")?;
    storage_end(vm, "check")
}

/// The second boot of `storage-usb`: the storage test's check, then the
/// first stick is pulled out, its filesystem still mounted, and plugged
/// in again: it goes, with its name, and comes back under the same name,
/// the lowest free.
fn storage_usb_check(vm: &mut Vm, case: &Case) -> Result<(), String> {
    storage_check(vm, case)?;
    let (disk, name) = (&case.disks[0], &disk_names(case.disks)[0]);
    let Bus::Usb(port) = disk.bus else { return Err("the first disk is not a USB one".into()) };
    let place = usb_place(port);
    vm.monitor("device_del stick0")?;
    vm.expect(&format!("USB: {place}: QEMU USB HARDDRIVE was unplugged"))?;
    vm.expect(&format!("Disk: {name} is gone with its USB device"))?;
    let file = vm.dir().join(format!("{}-disk0.img", case.name));
    vm.add_disk_file("again", &file)?;
    vm.monitor(&format!("device_add usb-storage,drive=again,bus=xhci.0,port={port},id=again"))?;
    vm.expect(&format!(
        "Disk: {name} is a USB disk of {} on {place}: QEMU QEMU HARDDISK ",
        disk_size(disk.kib)
    ))?;
    vm.expect(&format!("Disk: {name} has no partition table; it is used whole"))?;
    Ok(())
}

/// The second boot of `storage-uas`: the storage test's check, then the
/// device with two logical units is pulled out, their filesystems still
/// mounted: both disks go.
fn storage_uas_check(vm: &mut Vm, case: &Case) -> Result<(), String> {
    storage_check(vm, case)?;
    let Bus::Uas(port, _) = case.disks[0].bus else {
        return Err("the first disk is not a UAS one".into());
    };
    let names = disk_names(case.disks);
    vm.monitor(&format!("device_del uas{port}"))?;
    vm.expect(&format!("USB: 1-{port}: USB Attached SCSI HBA was unplugged"))?;
    for (disk, name) in case.disks.iter().zip(&names) {
        if matches!(disk.bus, Bus::Uas(p, _) if p == port) {
            vm.expect(&format!("Disk: {name} is gone with its USB device"))?;
        }
    }
    Ok(())
}

/// The `volumes` test: volumed(8) takes the stick Spark mounted at boot
/// as done; pulled out, its volume is unmounted, and plugged in again it
/// is mounted at its label, where volumetest (userspace/posix/volumetest.c)
/// writes a file and ejects it with eject(1). The host reads the file
/// back from the stick's image once it is pulled out again.
fn volumes(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    let (disk, name) = (&case.disks[0], &disk_names(case.disks)[0]);
    let Bus::Usb(port) = disk.bus else { return Err("the disk is not a USB one".into()) };
    let (place, label) = (usb_place(port), disk.label);
    vm.expect(&format!("spark: mounted the FAT filesystem of {name} at /volumes/{name}"))?;
    // The program may start waiting before volumed has looked.
    let waiting = format!("volumetest: waiting for /volumes/{label}");
    vm.expect("volumed: watching for volumes; ")?;
    if !vm.log().contains(&waiting) {
        vm.expect(&waiting)?;
    }
    vm.monitor("device_del stick0")?;
    vm.expect(&format!("USB: {place}: QEMU USB HARDDRIVE was unplugged"))?;
    vm.expect(&format!("volumed: {name} is gone; unmounted /volumes/{name}"))?;
    let file = vm.dir().join(format!("{}-disk0.img", case.name));
    vm.add_disk_file("again", &file)?;
    vm.monitor(&format!("device_add usb-storage,drive=again,bus=xhci.0,port={port},id=again"))?;
    vm.expect(&format!("volumed: mounted the FAT filesystem of {name} at /volumes/{label}"))?;
    vm.expect(&format!("volumetest: wrote /volumes/{label}/ejected.txt"))?;
    vm.expect(&format!("volumetest: ejected /volumes/{label}"))?;
    vm.expect("volumetest: every check passed")?;
    let code = program_line(vm, "(/system/tests/posix/volumetest) exited with code ")?;
    if code != "0x0" {
        return Err(format!("volumetest exited with code {code}"));
    }
    vm.monitor("device_del again")?;
    vm.expect(&format!("USB: {place}: QEMU USB HARDDRIVE was unplugged"))?;
    vm.expect(&format!("Disk: {name} is gone with its USB device"))?;
    let image = file.display().to_string();
    let read =
        host_tool(std::process::Command::new("mtype").args(["-i", &image, "::/ejected.txt"]))?;
    if read != VOLUME_TEXT {
        return Err(format!(
            "the stick's ejected.txt holds {:?}, not what was written",
            String::from_utf8_lossy(&read)
        ));
    }
    Ok(())
}

/// The `disk-raw` test: the program reads, writes and discards each disk
/// raw, and asks it Linux's block requests; the host then checks its
/// bytes ([`raw_check`]).
fn disk_raw(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    vm.expect_ahead("spark: started /system/tests/storage as process ")?;
    for (disk, name) in case.disks.iter().zip(disk_names(case.disks)) {
        let (rotational, sector) = (u8::from(!disk.solid()), disk.sector);
        vm.expect(&format!(
            "storage: /devices/{name}: {} bytes, {} sectors of {sector} bytes, physical blocks of \
             {sector}, minimum and optimal I/O {sector} and 0, alignment 0, read-only 0, \
             rotational {rotational}, discarded sectors read as zeros 0",
            disk.kib * 1024,
            disk.kib * 2
        ))?;
        vm.expect(&format!(
            "storage: /devices/{name}: raw reads at odd offsets read what it holds"
        ))?;
        vm.expect(&format!("storage: /devices/{name}: raw writes at odd offsets read back"))?;
        vm.expect(&format!(
            "storage: /devices/{name}: raw transfers straight between the disk and anonymous memory"
        ))?;
        vm.expect(&format!("storage: /devices/{name}: discards, flushes and refusals work"))?;
    }
    storage_end(vm, "raw")
}

/// The `iommu` test (ADR 0164, 0166): the `disk-raw` checks, every
/// disk's and the boot stick's DMA and interrupts going through QEMU's
/// VT-d unit, then what the kernel said of the unit and of the edu
/// device's self-test, whose two copies to a page it was not given the
/// unit stopped, as it did the interrupt edu raised with another
/// function's message. The power button's interrupt, through the I/O
/// APIC's entry, powers the machine off.
fn iommu(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disk_raw(vm, case)?;
    let log = vm.log();
    let unit = log
        .lines()
        .find_map(|line| line.find("IOMMU: VT-d ").map(|at| &line[at..]))
        .ok_or("the kernel never said it took over the IOMMU")?;
    // QEMU's DMAR names each device the unit translates. Under KVM the
    // entries name 32-bit APIC IDs.
    let described = unit.contains(" devices: 3-level page tables, ")
        && unit.contains(", queued invalidation, interrupt remapping");
    if !described {
        return Err(format!("the kernel described the IOMMU as \"{unit}\""));
    }
    for needle in ["selftest iommu: passed", " reaches only what is mapped for it (domain "] {
        if !log.contains(needle) {
            return Err(format!("the kernel never said \"{needle}\""));
        }
    }
    let stopped = log.matches(" was stopped writing to 0x").count();
    if stopped != 2 {
        return Err(format!("the IOMMU stopped {stopped} writes, not edu's 2"));
    }
    // edu's interrupt with the host bridge's message: its requester ID is
    // not the entry's.
    let blocked = log.matches(" was stopped raising an interrupt: ").count();
    if blocked != 1 {
        return Err(format!("the IOMMU stopped {blocked} interrupts, not edu's 1"));
    }
    power_off_by_button(vm)
}

/// Presses the power button, whose interrupt comes through the I/O APIC,
/// and waits for Spark to power the machine off.
fn power_off_by_button(vm: &mut Vm) -> Result<(), String> {
    vm.press_power_button()?;
    vm.expect("Power: the power button was pressed")?;
    vm.expect("spark: the power button was pressed; powering off the machine")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// The `iommu-amd` test (ADR 0165, 0166): the `iommu` test behind QEMU's
/// AMD-Vi IOMMU. QEMU's IOMMU logs no event for a page that is not mapped
/// or an interrupt that is blocked, so those show only in edu's
/// self-test, which reads the page back and counts the interrupts.
fn iommu_amd(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disk_raw(vm, case)?;
    let log = vm.log();
    let unit = log
        .lines()
        .find_map(|line| line.find("IOMMU: AMD-Vi ").map(|at| &line[at..]))
        .ok_or("the kernel never said it took over the IOMMU")?;
    let described = unit.starts_with("IOMMU: AMD-Vi unit 0 at 0xfed80000 translates ")
        && unit.contains(" device IDs: 4-level page tables, 65536 domains, command buffer ")
        && unit.contains(", interrupt remapping");
    if !described {
        return Err(format!("the kernel described the IOMMU as \"{unit}\""));
    }
    for needle in ["selftest iommu: passed", " reaches only what is mapped for it (domain "] {
        if !log.contains(needle) {
            return Err(format!("the kernel never said \"{needle}\""));
        }
    }
    power_off_by_button(vm)
}

/// The `disk-check` test: after the kernel's benchmarks, with no disk
/// mounted, disktest (userspace/posix/disktest.c, ADR 0122) finds the
/// NVMe disk by its serial number, writes its places, reads them back,
/// measures and saves its report, and then adds the kernel's and the
/// POSIX benchmarks' lines to it (ADR 0124); the host then runs Linux's
/// side.
fn disk_check(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    // The benchmarks end before `Boot complete.`, which disks_found waits
    // for.
    if !vm.log().contains("selftest bench: passed") {
        return Err("the kernel's benchmarks did not pass".into());
    }
    vm.expect("spark: leaving every disk's filesystems unmounted")?;
    vm.expect_ahead("spark: started /system/tests/posix/disktest as process ")?;
    vm.expect(&format!(
        "disktest: /devices/nvme0n1 on Dukovo: QEMU NVMe Ctrl, serial dukovo-test-0, {} bytes, \
         512-byte sectors, 512-byte blocks, solid state",
        CHECK_KIB * 1024
    ))?;
    vm.expect("disktest: a write of 3 bytes off a sector's boundary: passed")?;
    vm.expect("disktest: 64 of 64 places read back as written")?;
    vm.expect("disktest: 1 MiB write: ")?;
    vm.expect("lines of the kernel's benchmarks")?;
    vm.expect("posix-bench: done")?;
    vm.expect("disktest: passed; the report is saved on the disk")?;
    vm.expect("(/system/tests/posix/disktest) exited with code 0x0")?;
    Ok(())
}

/// Linux's side of the disk check, on the `disk-check` test's disk file:
/// built for the host from the same source, it must find Dukovo's report,
/// read back every place Dukovo wrote, and give the lines of Dukovo's
/// benchmarks, the kernel's and the POSIX ones, after its version.
fn linux_disk_check(disk: &DiskImage, _log: &str) -> Result<(), String> {
    let (program, source) = (disk.path.with_extension("disktest"), disk.path.with_extension("c"));
    let bench = disk.path.with_extension("bench");
    std::fs::write(&source, include_str!("../../../userspace/posix/disktest.c"))
        .map_err(|err| format!("writing {}: {err}", source.display()))?;
    let compiler = crate::ports::find_clang()
        .map(|(clang, _)| clang)
        .or_else(|| crate::process::find_program("cc"))
        .ok_or("no C compiler is installed, for Linux's side of the disk check")?;
    let built = host_tool(
        std::process::Command::new(compiler)
            .args(["-std=c11", "-D_GNU_SOURCE", "-O2", "-Wall", "-o"])
            .arg(&program)
            .arg(&source),
    );
    let _ = std::fs::remove_file(&source);
    built?;
    let ran = std::process::Command::new(&program)
        .args(["--yes-erase", "--bench-out"])
        .arg(&bench)
        .arg("--device")
        .arg(&disk.path)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|err| format!("running {}: {err}", program.display()));
    let _ = std::fs::remove_file(&program);
    let ran = ran?;
    let text = String::from_utf8_lossy(&ran.stdout);
    if !ran.status.success()
        || !text.contains("| Linux reads what Dukovo wrote, after a restart | 64 of 64 |")
    {
        return Err(format!("Linux's side of the disk check failed:\n{text}"));
    }
    let lines = std::fs::read_to_string(&bench);
    let _ = std::fs::remove_file(&bench);
    let lines = lines.map_err(|err| format!("reading {}: {err}", bench.display()))?;
    let version = concat!("dukovo: system Radix ", env!("CARGO_PKG_VERSION"), " ");
    for want in [version, "dukovo: BENCH name=syscall_null ", "dukovo: BENCH name=posix_signal "] {
        if !lines.lines().any(|line| line.starts_with(want)) {
            return Err(format!("Linux's side gave no line `{want}...` of Dukovo's benchmarks"));
        }
    }
    Ok(())
}

/// What a path of the `btrfs` test's tree is.
enum Made {
    Dir,
    File(Vec<u8>),
    /// A file of this many bytes, these pieces written into it and holes
    /// elsewhere.
    Sparse(u64, Vec<(u64, Vec<u8>)>),
    Link(&'static str),
    /// Another name of the file at this path, which is made first.
    Hard(&'static str),
    Pipe,
}

/// The file whose second sector [`make_btrfs`] damages on the disk, which
/// then fails to read.
const BTRFS_DAMAGED: &str = "damaged.bin";

/// The subvolume of the `btrfs` test's disks.
const BTRFS_SUBVOLUME: &str = "sub";

/// What the `btrfs` test's disks hold, from the top, each path after its
/// directory: files around the inline and sector sizes, a sparse one,
/// links of both kinds, a FIFO, deep and wide directories, a name that is
/// not ASCII, and a subvolume.
fn btrfs_files() -> Vec<(String, Made)> {
    let bytes = |from: usize, len: usize| (from..from + len).map(storage_big_byte).collect();
    let mut files = vec![
        ("hello.txt".to_string(), Made::File(b"Written by Linux's mkfs.btrfs.\n".to_vec())),
        ("empty".to_string(), Made::File(Vec::new())),
        ("inline.txt".to_string(), Made::File(b"inline ".repeat(15))),
        ("text.txt".to_string(), Made::File(btrfs_text())),
        ("big.bin".to_string(), Made::File(bytes(0, (1 << 20) + 123))),
        (
            "sparse.bin".to_string(),
            Made::Sparse(
                4 << 20,
                vec![(1 << 20, bytes(7, 4096)), ((4 << 20) - 100, bytes(9, 100))],
            ),
        ),
        (BTRFS_DAMAGED.to_string(), Made::File(btrfs_noise(12_345, 3 * 4096))),
        ("docs".to_string(), Made::Dir),
        ("docs/deep".to_string(), Made::Dir),
        ("docs/deep/deeper".to_string(), Made::Dir),
        ("docs/deep/deeper/note.txt".to_string(), Made::File(b"At the bottom.\n".to_vec())),
        ("hard.txt".to_string(), Made::Hard("docs/deep/deeper/note.txt")),
        ("link".to_string(), Made::Link("hello.txt")),
        ("dangling".to_string(), Made::Link("no/such/file")),
        ("na\u{ef}ve.txt".to_string(), Made::File(b"Not ASCII.\n".to_vec())),
        ("pipe".to_string(), Made::Pipe),
        ("many".to_string(), Made::Dir),
        (BTRFS_SUBVOLUME.to_string(), Made::Dir),
        (format!("{BTRFS_SUBVOLUME}/inside.txt"), Made::File(b"In a subvolume.\n".to_vec())),
        (format!("{BTRFS_SUBVOLUME}/nested"), Made::Dir),
        (format!("{BTRFS_SUBVOLUME}/nested/deep.txt"), Made::File(bytes(77, 5000))),
    ];
    for n in 0..100 {
        files.push((format!("many/file-{n:03}"), Made::File(format!("file {n}\n").into_bytes())));
    }
    files
}

/// `len` bytes of noise from `seed` (SplitMix64), which no other file of
/// the test holds and no compressor shrinks, so that [`make_btrfs`] finds
/// the damaged file's sector on the disk, as it is, wherever `mkfs.btrfs`
/// put the files.
fn btrfs_noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        out.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Text that compresses, of many extents.
fn btrfs_text() -> Vec<u8> {
    (0..8000)
        .flat_map(|n| format!("line {n}: Dukovo reads compressed Btrfs extents.\n").into_bytes())
        .collect()
}

/// A file's bytes, holes and all.
fn made_bytes(made: &Made) -> Vec<u8> {
    match made {
        Made::File(bytes) => bytes.clone(),
        Made::Sparse(len, pieces) => {
            let mut bytes = vec![0; *len as usize];
            for (at, piece) in pieces {
                bytes[*at as usize..*at as usize + piece.len()].copy_from_slice(piece);
            }
            bytes
        }
        _ => Vec::new(),
    }
}

/// Makes the `btrfs` test's tree of files in `dir`, afresh.
fn write_btrfs_tree(dir: &Path) -> Result<(), String> {
    let failed = |what: &Path, err: std::io::Error| format!("making {}: {err}", what.display());
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|err| failed(dir, err))?;
    }
    std::fs::create_dir_all(dir).map_err(|err| failed(dir, err))?;
    for (path, made) in btrfs_files() {
        let at = dir.join(&path);
        match made {
            Made::Dir => std::fs::create_dir(&at).map_err(|err| failed(&at, err))?,
            Made::File(_) | Made::Sparse(..) => {
                use std::io::SeekFrom;
                let mut file = std::fs::File::create(&at).map_err(|err| failed(&at, err))?;
                let pieces = match &made {
                    Made::Sparse(len, pieces) => {
                        file.set_len(*len).map_err(|err| failed(&at, err))?;
                        pieces.clone()
                    }
                    _ => vec![(0, made_bytes(&made))],
                };
                for (offset, piece) in pieces {
                    file.seek(SeekFrom::Start(offset)).map_err(|err| failed(&at, err))?;
                    file.write_all(&piece).map_err(|err| failed(&at, err))?;
                }
            }
            Made::Link(target) => {
                std::os::unix::fs::symlink(target, &at).map_err(|err| failed(&at, err))?;
            }
            Made::Hard(first) => {
                std::fs::hard_link(dir.join(first), &at).map_err(|err| failed(&at, err))?;
            }
            Made::Pipe => {
                host_tool(std::process::Command::new("mkfifo").arg(&at))?;
            }
        }
    }
    Ok(())
}

/// Makes a Btrfs test disk. One of the `btrfs` test ([`BTRFS`]):
/// `mkfs.btrfs` puts [`btrfs_files`] on it, with [`BTRFS_SUBVOLUME`] a
/// subvolume, compressed, as the disk's sectors say ([`BTRFS_DISKS`]),
/// the damaged file's noise kept as it is; then a byte of
/// [`BTRFS_DAMAGED`]'s second sector is changed on the disk, and the
/// disk's SHA-256 kept beside it for [`btrfs_unchanged`]. One to write
/// ([`BTRFS_NEW`]): `mkfs.btrfs` puts the host's file on it, the disk's
/// sectors choosing the rest as for the `btrfs` test.
fn make_btrfs(ctx: &Ctx, disk: &Disk, image: &DiskImage) -> Result<(), String> {
    let tree = image.path.with_extension("tree");
    if disk.fat == BTRFS {
        write_btrfs_tree(&tree)?;
    } else {
        if tree.exists() {
            std::fs::remove_dir_all(&tree)
                .map_err(|err| format!("removing {}: {err}", tree.display()))?;
        }
        std::fs::create_dir(&tree).map_err(|err| format!("making {}: {err}", tree.display()))?;
        let host = tree.join(STORAGE_FROM_HOST_NAME);
        std::fs::write(&host, STORAGE_FROM_HOST)
            .map_err(|err| format!("writing {}: {err}", host.display()))?;
    }
    let path = &image.path;
    let file =
        std::fs::File::create(path).map_err(|err| format!("making {}: {err}", path.display()))?;
    file.set_len(image.bytes).map_err(|err| format!("sizing {}: {err}", path.display()))?;
    let mkfs = btrfs::mkfs(ctx).map_err(|err| err.to_string())?;
    let mut cmd = std::process::Command::new(mkfs);
    cmd.args(["-q", "-f", "-L", disk.label, "--rootdir"]).arg(&tree);
    if disk.sector == 4096 {
        cmd.args(["--csum", "blake2", "--nodesize", "4096", "-m", "single"]);
    }
    if disk.fat == BTRFS {
        cmd.args(["--subvol", &format!("rw:{BTRFS_SUBVOLUME}")]);
        if disk.sector == 4096 {
            cmd.args(["--compress", "zstd", "-O", "^no-holes"]);
        } else {
            cmd.args(["--compress", "zlib"]);
        }
    }
    let made = host_tool(cmd.arg(path).stdin(Stdio::null()));
    let _ = std::fs::remove_dir_all(&tree);
    made?;
    if disk.fat == BTRFS_NEW {
        return Ok(());
    }
    let mut bytes =
        std::fs::read(path).map_err(|err| format!("reading {}: {err}", path.display()))?;
    let damaged = btrfs_files()
        .into_iter()
        .find(|(name, _)| name == BTRFS_DAMAGED)
        .map_or_else(Vec::new, |(_, made)| made_bytes(&made));
    let second = &damaged[4096..4096 + 64];
    let at = bytes.windows(second.len()).position(|w| w == second).ok_or_else(|| {
        format!("{BTRFS_DAMAGED}'s second sector is nowhere on {}", path.display())
    })?;
    if bytes[at + 1..].windows(second.len()).any(|w| w == second) {
        return Err(format!("{BTRFS_DAMAGED}'s second sector is on {} twice", path.display()));
    }
    bytes[at + 100] ^= 0x40;
    std::fs::write(path, &bytes).map_err(|err| format!("writing {}: {err}", path.display()))?;
    let sum = host_tool(std::process::Command::new("sha256sum").arg(path))?;
    let sums = path.with_extension("sha256");
    std::fs::write(&sums, sum).map_err(|err| format!("writing {}: {err}", sums.display()))
}

/// Checks a Btrfs disk with Linux's `btrfs check`, the data's checksums
/// too.
fn btrfs_checked(disk: &DiskImage, _log: &str) -> Result<(), String> {
    let tool = disk.btrfs.as_ref().ok_or("a Btrfs disk was made without the btrfs tool")?;
    let mut cmd = std::process::Command::new(tool);
    cmd.args(["check", "--check-data-csum"]).arg(&disk.path).stdin(Stdio::null());
    host_tool(&mut cmd)
        .map(drop)
        .map_err(|err| format!("{} fails Linux's btrfs check: {err}", disk.path.display()))
}

/// Checks the Dukovo root partition of the `root` test's disk with
/// Linux's `btrfs check`, as [`btrfs_checked`] does, on a copy of it
/// beside the disk.
fn root_checked(disk: &DiskImage, log: &str) -> Result<(), String> {
    let copy = DiskImage {
        path: disk.path.with_extension("root"),
        offset: 0,
        bytes: disk.bytes,
        sector: disk.sector,
        check: disk.check,
        btrfs: disk.btrfs.clone(),
    };
    let checked = (|| {
        let mut from = std::fs::File::open(&disk.path)?;
        let mut to = std::fs::File::create(&copy.path)?;
        to.set_len(disk.bytes)?;
        image::copy_sparse(&mut from, disk.offset, &mut to, 0, disk.bytes)
    })()
    .map_err(|err| format!("copying the root partition of {}: {err}", disk.path.display()))
    .and_then(|()| btrfs_checked(&copy, log));
    let _ = std::fs::remove_file(&copy.path);
    checked
}

/// The `root` test's first boot: Spark finds Dukovo's root on the disk,
/// mounts its subvolumes and copies the default settings in; the program
/// checks the mounts and that `/system` refuses changes, writes a file to
/// each of the others, and makes a read-only snapshot of `/users` that
/// keeps its file as it was (ADR 0138).
fn root_write(vm: &mut Vm, _: &Case) -> Result<(), String> {
    root_boot(vm, true)?;
    vm.expect("storage: /system: writes fail with read_only")?;
    vm.expect("storage: a snapshot of /users keeps its file as /users changes it")?;
    vm.expect("storage: a clone in /users took no room and kept its data")?;
    storage_end(vm, "root")
}

/// The `root` test's second boot: the same, with no default settings to
/// add, and the program reads the files back, the snapshot's too,
/// removes the snapshot, and scrubs the filesystem.
fn root_read(vm: &mut Vm, _: &Case) -> Result<(), String> {
    root_boot(vm, false)?;
    vm.expect("storage: /system: writes fail with read_only")?;
    vm.expect("storage: the snapshot of /users kept its file, and went")?;
    vm.expect("storage: a scrub of Dukovo's root found no errors")?;
    vm.expect("storage: a scrub through /system may not mend: missing_rights")?;
    storage_end(vm, "rooted")
}

/// What Spark says of the `root` test's disk: it mounts its Btrfs, then
/// again to compress, as nothing asked it to; then mounts the subvolumes,
/// adds the default settings the `first` boot only, and starts the
/// program.
fn root_boot(vm: &mut Vm, first: bool) -> Result<(), String> {
    let name = &disk_names(ROOT_DISKS)[0];
    // The image's EFI system partition is no volume.
    let efi = partition_name(name, 2);
    vm.expect(&format!("spark: {efi} is an EFI system partition; left alone"))?;
    let disk = partition_name(name, 5);
    let mounted = format!("spark: mounted the Btrfs filesystem of {disk} at /volumes/{disk}");
    vm.expect(&mounted)?;
    vm.expect(&format!("{mounted}, compressed (zstd)"))?;
    vm.expect(&format!("spark: /system is Dukovo's root on {disk}, compressed (zstd)"))?;
    vm.expect("spark: built the hierarchy of 9 directories and mounted 12 filesystems")?;
    let added = program_line(vm, "spark: added ")?;
    if (added == "0 default settings to /system/settings") == first {
        return Err(format!("Spark added {added}"));
    }
    vm.expect_ahead("spark: started /system/tests/storage as process ")
}

/// Copies the files of a Btrfs disk's top subvolume out with Linux's
/// `btrfs restore`, into a directory beside it made afresh, which the
/// caller removes.
fn btrfs_restore(disk: &DiskImage) -> Result<PathBuf, String> {
    let tool = disk.btrfs.as_ref().ok_or("a Btrfs disk was made without the btrfs tool")?;
    let to = disk.path.with_extension("restored");
    if to.exists() {
        std::fs::remove_dir_all(&to).map_err(|err| format!("removing {}: {err}", to.display()))?;
    }
    std::fs::create_dir(&to).map_err(|err| format!("making {}: {err}", to.display()))?;
    let mut cmd = std::process::Command::new(tool);
    cmd.arg("restore").arg(&disk.path).arg(&to).stdin(Stdio::null());
    if let Err(err) = host_tool(&mut cmd) {
        let _ = std::fs::remove_dir_all(&to);
        return Err(err);
    }
    Ok(to)
}

/// The size of the storage program's churn file of round `round`.
fn churn_size(round: usize) -> usize {
    1 + round * 37_813 % (192 * 1024)
}

/// The byte at `offset` of the churn file of round `round`.
fn churn_byte(round: usize, offset: usize) -> u8 {
    storage_big_byte(offset + round * 4099)
}

/// The round of a churn file's name, and whether it is the one before
/// the rename (`temp-N`).
fn churn_name(name: &str) -> Option<(usize, bool)> {
    let (number, temp) = match name.strip_prefix("file-") {
        Some(number) => (number, false),
        None => (name.strip_prefix("temp-")?, true),
    };
    Some((number.parse().ok()?, temp))
}

/// How many files the storage program's churn keeps, as it has it: each
/// round removes the file of the round this many before, after the
/// round's line `synced`.
const CHURN_KEEP: usize = 8;

/// The rounds of the churn's lines `storage: churn: <what> file-NNNN` in
/// `log`; a line the power cut short, without its four digits, is not
/// counted.
fn churn_rounds(log: &str, what: &str) -> Vec<usize> {
    let start = format!("storage: churn: {what} file-");
    log.lines()
        .filter_map(|line| line.split_once(start.as_str()))
        .map(|(_, number)| number.trim())
        .filter(|number| number.len() == 4)
        .filter_map(|number| number.parse().ok())
        .collect()
}

/// Checks the `btrfs-power-cut` test's disk after a boot: Linux's
/// `btrfs check` finds nothing wrong; and after the boot whose power was
/// cut, as `btrfs restore` copies the files out, each file the program
/// said it synced is whole unless it said it removed it, each it said it
/// removed has gone, every other `file-N` is whole and every `temp-N` a
/// start of itself. The file the churn removes after the last round it
/// said it synced may have gone too, with no line to say so: the power
/// may go after the removal's sync and before or during its line.
fn btrfs_cut(disk: &DiskImage, log: &str) -> Result<(), String> {
    btrfs_checked(disk, log)?;
    let synced = churn_rounds(log, "synced");
    let Some(&last) = synced.last() else { return Ok(()) };
    let removed = churn_rounds(log, "removed");
    let restored = btrfs_restore(disk)?;
    let result = churn_left(&restored, &synced, &removed, last.checked_sub(CHURN_KEEP));
    let _ = std::fs::remove_dir_all(&restored);
    result.map_err(|err| {
        format!("{}: {err}, after a power cut after file-{last:04} was synced", disk.path.display())
    })
}

/// Checks the `ext4-power-cut` test's disk after a boot as [`btrfs_cut`]
/// checks Btrfs: with its journal replayed, `e2fsck` finds nothing wrong,
/// and the files are as the churn left them.
fn ext4_cut(disk: &DiskImage, log: &str) -> Result<(), String> {
    ext4_replayed_fsck(disk)?;
    let synced = churn_rounds(log, "synced");
    let Some(&last) = synced.last() else { return Ok(()) };
    let removed = churn_rounds(log, "removed");
    let restored = ext4_dump(disk)?;
    let result = churn_left(&restored, &synced, &removed, last.checked_sub(CHURN_KEEP));
    let _ = std::fs::remove_dir_all(&restored);
    result.map_err(|err| {
        format!("{}: {err}, after a power cut after file-{last:04} was synced", disk.path.display())
    })
}

/// [`btrfs_cut`]'s check of the files copied out to `dir`; the file of
/// round `removing` may have gone unsaid.
fn churn_left(
    dir: &Path,
    synced: &[usize],
    removed: &[usize],
    removing: Option<usize>,
) -> Result<(), String> {
    let mut found = BTreeMap::new();
    let entries = std::fs::read_dir(dir).map_err(|err| format!("listing it: {err}"))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("listing it: {err}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let bytes = std::fs::read(entry.path()).map_err(|err| format!("reading {name}: {err}"))?;
        found.insert(name, bytes);
    }
    for (name, bytes) in &found {
        if name == STORAGE_FROM_HOST_NAME {
            if bytes != STORAGE_FROM_HOST {
                return Err(format!("{name} is not as the host's mkfs wrote it"));
            }
            continue;
        }
        let Some((round, temp)) = churn_name(name) else {
            return Err(format!("it holds {name}, which the churn never made"));
        };
        let size = churn_size(round);
        let start = bytes.iter().enumerate().all(|(at, &byte)| byte == churn_byte(round, at));
        if !start || bytes.len() > size || (!temp && bytes.len() != size) {
            return Err(format!(
                "{name} holds {} bytes, not what the churn wrote ({size} bytes)",
                bytes.len()
            ));
        }
        if temp && synced.contains(&round) {
            return Err(format!("{name} is still there, though it was renamed and synced"));
        }
    }
    for round in
        synced.iter().filter(|&&round| !removed.contains(&round) && Some(round) != removing)
    {
        if !found.contains_key(&format!("file-{round:04}")) {
            return Err(format!("file-{round:04} has gone, though it was synced"));
        }
    }
    for round in removed {
        if found.contains_key(&format!("file-{round:04}")) {
            return Err(format!(
                "file-{round:04} is still there, though it was removed and synced"
            ));
        }
    }
    Ok(())
}

/// The `btrfs-power-cut` test's first boot: the program churns files on
/// the disk until the host cuts the power, 0 to 1.5 seconds after the
/// program said it synced round [`CUT_AFTER`]'s; then [`btrfs_cut`]
/// checks the disk.
fn power_cut(vm: &mut Vm, case: &Case) -> Result<(), String> {
    storage_boot(vm, case)?;
    vm.expect(&format!("storage: churn: synced file-{:04}", CUT_AFTER - 1))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    std::thread::sleep(Duration::from_millis(u64::from(nanos % 1500)));
    if vm.log().contains("storage: churn: done") {
        return Err("the churn ended before the power was cut".into());
    }
    Ok(())
}

/// Between the boots of the `btrfs-power-cut` and `root` tests: nothing,
/// as the disk's check reads it.
fn power_cut_host(_: &Case, _: &[DiskImage]) -> Result<(), String> {
    Ok(())
}

/// The `btrfs-power-cut` test's second boot: the program finds the
/// churn's files whole or a start of themselves, removes them, and
/// writes the volume again.
fn power_cut_check(vm: &mut Vm, case: &Case) -> Result<(), String> {
    storage_boot(vm, case)?;
    for name in volume_names(case.disks) {
        vm.expect(&format!("storage: /volumes/{name}: after the power cut, "))?;
        vm.expect(&format!("storage: /volumes/{name}: written again after the power cut"))?;
    }
    vm.expect("storage: /processes lists ")?;
    storage_end(vm, "churned")
}

/// Checks that a Btrfs disk is as [`make_btrfs`] made it.
fn btrfs_unchanged(disk: &DiskImage, _log: &str) -> Result<(), String> {
    let sums = disk.path.with_extension("sha256");
    let made = std::fs::read(&sums).map_err(|err| format!("reading {}: {err}", sums.display()))?;
    let now = host_tool(std::process::Command::new("sha256sum").arg(&disk.path))?;
    if now != made {
        return Err(format!("{} changed, though it was mounted read-only", disk.path.display()));
    }
    Ok(())
}

/// The lines the storage program's `list` prints for a volume of the
/// `btrfs` test ([`btrfs_files`]): as Linux would list them, but for the
/// damaged file, which fails with `io`.
fn btrfs_listing(volume: &str) -> Vec<String> {
    let files = btrfs_files();
    let mut lines = Vec::new();
    // The first path of each file with more than one name.
    let mut first: BTreeMap<&str, String> = BTreeMap::new();
    fn walk<'a>(
        files: &'a [(String, Made)],
        at: &str,
        volume: &str,
        first: &mut BTreeMap<&'a str, String>,
        lines: &mut Vec<String>,
    ) {
        let mut inside: Vec<&(String, Made)> = files
            .iter()
            .filter(|(path, _)| match at {
                "" => !path.contains('/'),
                _ => path
                    .strip_prefix(at)
                    .and_then(|p| p.strip_prefix('/'))
                    .is_some_and(|p| !p.contains('/')),
            })
            .collect();
        inside.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, made) in inside {
            let line = |what: String| format!("storage: list {volume}: {what}");
            match made {
                Made::Dir => {
                    lines.push(line(format!("dir /{path}")));
                    walk(files, path, volume, first, lines);
                }
                Made::Link(target) => lines.push(line(format!("link /{path} -> {target}"))),
                Made::Pipe => lines.push(line(format!("pipe /{path}"))),
                Made::Hard(of) => match first.get(*of) {
                    Some(seen) => {
                        lines.push(line(format!("file /{path}: the same file as {seen}")));
                    }
                    None => {
                        first.insert(of, format!("/{path}"));
                        let bytes = files.iter().find(|(p, _)| p == of).map(|(_, m)| made_bytes(m));
                        lines.push(line(file_line(path, &bytes.unwrap_or_default())));
                    }
                },
                Made::File(_) | Made::Sparse(..) => match first.get(path.as_str()) {
                    Some(seen) => {
                        lines.push(line(format!("file /{path}: the same file as {seen}")));
                    }
                    None => {
                        first.insert(path.as_str(), format!("/{path}"));
                        if path == BTRFS_DAMAGED {
                            lines.push(line(format!("file /{path}: io")));
                        } else {
                            lines.push(line(file_line(path, &made_bytes(made))));
                        }
                    }
                },
            }
        }
    }
    /// A file's line: its size and FNV-1a hash.
    fn file_line(path: &str, bytes: &[u8]) -> String {
        let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
        format!("file /{path}: {} bytes, FNV-1a {hash:016x}", bytes.len())
    }
    walk(&files, "", volume, &mut first, &mut lines);
    lines.push(format!("storage: list {volume}: {} entries", files.len()));
    lines.push(format!("storage: list {volume}: writes fail with read_only"));
    lines
}

/// The `btrfs` test: Spark mounts each disk read-only, the first as Radix
/// does not write it yet and the second as the disk refuses writes, and
/// the storage program lists every volume as [`btrfs_listing`] says.
fn btrfs_list(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    let volumes = volume_names(case.disks);
    for (disk, name) in case.disks.iter().zip(&volumes) {
        let label = disk.label;
        vm.expect(&format!(
            "Files: mounted btrfs from {name} on /volumes/{name}, read-only, labelled {label}"
        ))?;
        vm.expect(&format!(
            "spark: mounted the Btrfs filesystem of {name} at /volumes/{name}, read-only"
        ))?;
    }
    vm.expect("spark: built the hierarchy of 9 directories and mounted 9 filesystems")?;
    vm.expect_ahead("spark: started /system/tests/storage as process ")?;
    // The program lists the volumes in name order.
    let mut sorted = volumes.clone();
    sorted.sort();
    for name in &sorted {
        for line in btrfs_listing(name) {
            vm.expect(&line)?;
        }
    }
    storage_end(vm, "list")
}

/// Linux's lists of the `btrfs-log` test's disk, before and after its
/// replay, as `ob fixture btrfs-log` keeps them.
const LOG_LISTS: &str = include_str!("../../../kernel/fs/btrfs/src/tests/fixtures/btrfs-log.txt");

/// The lines the storage program's `replayed` prints for the volume
/// `volume` of the `btrfs-log` test: Linux's list after its replay, a
/// file with more than one name listed once, then the write it takes.
fn log_listing(volume: &str) -> Vec<String> {
    let mut lines = Vec::new();
    // The hash and first path of each file with more than one name.
    let mut first: Vec<(&str, &str)> = Vec::new();
    let replayed = LOG_LISTS
        .lines()
        .skip_while(|line| *line != "LIST replayed")
        .skip(1)
        .take_while(|line| *line != "LIST end");
    for entry in replayed {
        // LIST <path> <kind> <mode> <uid> <gid> <links> <size> <what> <xattrs>
        let fields: Vec<&str> = entry.split(' ').collect();
        let ["LIST", path, kind, _, _, _, links, size, what, _] = fields[..] else { continue };
        let what = match kind {
            _ if path == "/" => continue,
            "dir" => format!("dir {path}"),
            "link" => format!("link {path} -> {what}"),
            _ => match first.iter().find(|(hash, _)| links != "1" && *hash == what) {
                Some((_, seen)) => format!("file {path}: the same file as {seen}"),
                None => {
                    if links != "1" {
                        first.push((what, path));
                    }
                    format!("file {path}: {size} bytes, FNV-1a {what}")
                }
            },
        };
        lines.push(format!("storage: list {volume}: {what}"));
    }
    lines.push(format!("storage: list {volume}: {} entries", lines.len()));
    lines.push(format!("storage: list {volume}: takes writes"));
    lines
}

/// The `btrfs-log` test: Spark mounts the disk, which replays Linux's
/// log of fsyncs (ADR 0151), and the storage program lists it as
/// [`log_listing`] says, then writes to it.
fn btrfs_log(vm: &mut Vm, case: &Case) -> Result<(), String> {
    storage_boot(vm, case)?;
    for name in volume_names(case.disks) {
        for line in log_listing(&name) {
            vm.expect(&line)?;
        }
    }
    storage_end(vm, "replayed")
}

/// Makes a Btrfs RAID1 of data and metadata across the images at `paths`,
/// labelled `label`. It holds no file of the host's: `mkfs.btrfs` fills
/// a filesystem of one disk only.
fn make_btrfs_raid(ctx: &Ctx, label: &str, paths: &[PathBuf]) -> Result<(), String> {
    let mkfs = btrfs::mkfs(ctx).map_err(|err| err.to_string())?;
    let mut cmd = std::process::Command::new(mkfs);
    cmd.args(["-q", "-f", "-L", label, "-d", "raid1", "-m", "raid1"]);
    host_tool(cmd.args(paths).stdin(Stdio::null())).map(drop)
}

/// Where a Btrfs disk's superblock is.
const BTRFS_SUPERBLOCK: u64 = 0x10000;

/// The ID of the Btrfs whose superblock the image at `path` holds.
fn btrfs_id(path: &Path) -> Option<[u8; 16]> {
    let file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 72];
    std::os::unix::fs::FileExt::read_exact_at(&file, &mut head, BTRFS_SUPERBLOCK).ok()?;
    (&head[64..] == b"_BHRfS_M").then(|| head[32..48].try_into().unwrap_or_default())
}

/// Checks the Btrfs RAID1 of the `btrfs-raid` test with Linux's `btrfs
/// check`, the data's checksums too, from its first disk still there,
/// given each of its disks still there; then from each of them alone, as
/// Linux reads a RAID1 with the others missing, so that every disk must
/// hold a copy of everything. A disk pulled out, or taken out and wiped,
/// is passed over; the case's first disk, which `mkfs.btrfs` made, checks
/// them even then. After a boot that lost a disk while it ran
/// ([`PULL_WARNING`]), the disks are checked alone only: the one lost
/// holds the filesystem as it was then, which the other has moved on
/// from.
fn raid_checked(disk: &DiskImage, log: &str) -> Result<(), String> {
    let image = disk.path.display().to_string();
    let prefix = image.rsplit_once("-disk").map_or(image.as_str(), |(prefix, _)| prefix);
    let paths: Vec<PathBuf> =
        (0..RAID_DISKS.len()).map(|i| PathBuf::from(format!("{prefix}-disk{i}.img"))).collect();
    if paths.first() != Some(&disk.path) {
        return Ok(());
    }
    let Some(id) = paths.iter().find_map(|path| btrfs_id(path)) else { return Ok(()) };
    let disks: Vec<PathBuf> = paths.into_iter().filter(|path| btrfs_id(path) == Some(id)).collect();
    let tool = disk.btrfs.as_ref().ok_or("a Btrfs disk was made without the btrfs tool")?;
    let lost = log.contains(PULL_WARNING);
    let mut sets = if lost { Vec::new() } else { vec![&disks[..]] };
    if disks.len() > 1 {
        sets.extend(disks.iter().map(std::slice::from_ref));
    }
    for set in sets {
        btrfs_check_of(tool, &id, set, &disk.path.with_extension("blkid"))?;
    }
    Ok(())
}

/// Runs Linux's `btrfs check`, the data's checksums too, on the Btrfs
/// `id` from the first of the images `disks`, given them all in a blkid
/// cache written to `tab` (`BLKID_FILE`), where `btrfs check` finds a
/// filesystem's other disks.
fn btrfs_check_of(tool: &Path, id: &[u8; 16], disks: &[PathBuf], tab: &Path) -> Result<(), String> {
    use std::fmt::Write as _;
    let uuid = id.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    });
    let uuid = format!(
        "{}-{}-{}-{}-{}",
        &uuid[..8],
        &uuid[8..12],
        &uuid[12..16],
        &uuid[16..20],
        &uuid[20..]
    );
    let cache = disks.iter().enumerate().fold(String::new(), |mut out, (n, path)| {
        let _ = writeln!(
            out,
            "<device DEVNO=\"0x07{n:02x}\" TIME=\"1.1\" TYPE=\"btrfs\" UUID=\"{uuid}\">{}</device>",
            path.display()
        );
        out
    });
    std::fs::write(tab, cache).map_err(|err| format!("writing {}: {err}", tab.display()))?;
    let first = disks.first().ok_or("no disk to check")?;
    let mut cmd = std::process::Command::new(tool);
    cmd.args(["check", "--readonly", "--check-data-csum"]).arg(first);
    let checked = host_tool(cmd.env("BLKID_FILE", tab).stdin(Stdio::null()));
    // libblkid keeps the cache it rewrote as `<file>.old`.
    let _ = std::fs::remove_file(tab);
    let _ = std::fs::remove_file(format!("{}.old", tab.display()));
    checked.map(drop).map_err(|err| {
        let names: Vec<String> = disks.iter().map(|path| path.display().to_string()).collect();
        format!("the RAID1 of {} fails Linux's btrfs check: {err}", names.join(", "))
    })
}

/// What the kernel says once a disk of a Btrfs fails while it runs.
const PULL_WARNING: &str = "a disk failed and was left out; 1 of its 2 disks missing now";

/// Which disk the `btrfs-raid-pull` test pulls out, and after how many of
/// its writes: picked once a run, from the clock.
fn pull_at() -> (usize, u32) {
    static AT: std::sync::OnceLock<(usize, u32)> = std::sync::OnceLock::new();
    *AT.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        ((nanos & 1) as usize, PULL_FIRST + (nanos >> 1) % PULL_SPAN)
    })
}

/// QEMU's blkdebug rules for disk `i` of the `btrfs-raid-pull` test: the
/// disk [`pull_at`] picks counts its writes, from state 1 up, and from
/// the last fails every read, write and flush with `EIO`, as a disk
/// pulled out fails them; the other, none.
fn pull_rules(i: usize) -> String {
    use std::fmt::Write as _;
    let (disk, writes) = pull_at();
    let mut rules = String::new();
    if i != disk {
        return rules;
    }
    for state in 1..=writes {
        let _ = write!(
            rules,
            "[set-state]\nevent = \"write_aio\"\nstate = \"{state}\"\nnew_state = \"{}\"\n\n",
            state + 1
        );
    }
    for event in ["read_aio", "write_aio", "flush_to_disk"] {
        let _ = write!(
            rules,
            "[inject-error]\nevent = \"{event}\"\nstate = \"{}\"\nerrno = \"5\"\n\n",
            writes + 1
        );
    }
    rules
}

/// Between the boots of the `btrfs-raid-pull` test: the disk pulled out
/// is back, failing nothing.
fn raid_back(_: &Case, disks: &[DiskImage]) -> Result<(), String> {
    for disk in disks {
        let at = disk.path.with_extension("blkdebug");
        std::fs::write(&at, "").map_err(|err| format!("writing {}: {err}", at.display()))?;
    }
    Ok(())
}

/// The `btrfs-raid-pull` test's first boot: the program writes its files
/// to the RAID1 while one of its disks fails, which the kernel leaves out
/// and warns of.
fn raid_pulled(vm: &mut Vm, case: &Case) -> Result<(), String> {
    let (disk, writes) = pull_at();
    let names = disk_names(case.disks);
    let pulled = || format!("with {} pulled out after {writes} writes", names[disk]);
    raid_boot(vm, case, false, false).map_err(|err| format!("{err}, {}", pulled()))?;
    let first = &names[0];
    let written = vm
        .expect(&format!("storage: /volumes/{first}: wrote "))
        .and_then(|()| storage_end(vm, "write"))
        .and_then(|()| vm.expect_anywhere(&format!("Btrfs: {first}: {PULL_WARNING}")));
    written.map_err(|err| format!("{err}, {}", pulled()))
}

/// The `btrfs-raid-pull` test's second boot, with the disk pulled out
/// back: the kernel warns that it missed changes, the program reads every
/// file back, and its scrub catches the disk up.
fn raid_caught_up(vm: &mut Vm, case: &Case) -> Result<(), String> {
    raid_boot(vm, case, false, true)?;
    let first = &disk_names(case.disks)[0];
    vm.expect(&format!("storage: /volumes/{first}: "))?;
    vm.expect(" files read back after the reboot, every byte as written")?;
    vm.expect(&format!("storage: /volumes/{first}: a scrub read "))?;
    storage_end(vm, "check")
}

/// Between the boots of the `btrfs-raid` test: its second disk is pulled
/// out, as Btrfs sees it, its superblock wiped.
fn raid_pull(_: &Case, disks: &[DiskImage]) -> Result<(), String> {
    let disk = disks.get(1).ok_or("the case has one disk")?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&disk.path)
        .and_then(|file| {
            std::os::unix::fs::FileExt::write_all_at(&file, &[0; 4096], BTRFS_SUPERBLOCK)
        })
        .map_err(|err| format!("wiping {}: {err}", disk.path.display()))
}

/// What the kernel and Spark say of the `btrfs-raid` test's disks: Spark
/// mounts the RAID1 from the first, and leaves the second alone, as in
/// use, or once `pulled` as holding nothing, when the kernel warns that
/// the first mounted without it; and a third, raw, alone. A disk
/// `behind`, back after it was lost, has the kernel warn too.
fn raid_boot(vm: &mut Vm, case: &Case, pulled: bool, behind: bool) -> Result<(), String> {
    disks_found(vm, case)?;
    let names = disk_names(case.disks);
    let (first, second) = (&names[0], &names[1]);
    if pulled {
        vm.expect(&format!(
            "Btrfs: {first}: 1 of its 2 disks missing; mounted from the copies on the others"
        ))?;
    }
    if behind {
        vm.expect(&format!(
            "Btrfs: {first}: 1 of its 2 disks missed changes; read from the others until a scrub \
             catches them up"
        ))?;
    }
    let label = case.disks[0].label;
    vm.expect(&format!("Files: mounted btrfs from {first} on /volumes/{first}, labelled {label}"))?;
    vm.expect(&format!("spark: mounted the Btrfs filesystem of {first} at /volumes/{first}"))?;
    if pulled {
        vm.expect(&format!(
            "spark: {second} holds no FAT, exFAT, Btrfs or ext4 filesystem; left alone"
        ))?;
    } else {
        vm.expect(&format!("spark: {second} is in use already; left alone"))?;
    }
    for other in names.iter().skip(2) {
        vm.expect(&format!(
            "spark: {other} holds no FAT, exFAT, Btrfs or ext4 filesystem; left alone"
        ))?;
    }
    vm.expect("spark: built the hierarchy of 9 directories and mounted 9 filesystems")?;
    vm.expect_ahead("spark: started /system/tests/storage as process ")?;
    Ok(())
}

/// The `btrfs-raid` test's first boot: the program writes its files to
/// the RAID1.
fn raid_write(vm: &mut Vm, case: &Case) -> Result<(), String> {
    raid_boot(vm, case, false, false)?;
    let first = &disk_names(case.disks)[0];
    vm.expect(&format!("storage: /volumes/{first}: btrfs, "))?;
    vm.expect(&format!("storage: /volumes/{first}: wrote "))?;
    storage_end(vm, "write")
}

/// The `btrfs-raid` test's second boot, with the second disk pulled out:
/// the program reads every file back from the first, puts the third disk
/// in the place of the second (device 2), which the kernel rebuilds from
/// the first's copies, adds the fourth (device 3), moves every chunk,
/// and takes the first out (device 1), its chunks moved to the others,
/// reading every file back after each.
fn raid_check(vm: &mut Vm, case: &Case) -> Result<(), String> {
    raid_boot(vm, case, true, false)?;
    let names = disk_names(case.disks);
    let (first, third, fourth) = (&names[0], &names[2], &names[3]);
    let read_back = |vm: &mut Vm| {
        vm.expect(&format!("storage: /volumes/{first}: "))?;
        vm.expect(" files read back after the reboot, every byte as written")
    };
    read_back(vm)?;
    vm.expect(&format!("storage: /volumes/{first}: simple quotas on"))?;
    vm.expect(&format!(
        "Btrfs: disk 2 replaced by {third}, and rebuilt from the other disks' copies; 0 errors \
         no copy mends"
    ))?;
    vm.expect(&format!("storage: /volumes/{first}: disk 2 replaced by {third}, rebuilt from "))?;
    read_back(vm)?;
    vm.expect(&format!("Btrfs: {fourth} added as disk 3"))?;
    vm.expect(&format!("storage: /volumes/{first}: {fourth} added as disk 3"))?;
    read_back(vm)?;
    vm.expect(&format!("storage: /volumes/{first}: a balance moved "))?;
    read_back(vm)?;
    vm.expect(&format!("storage: /volumes/{first}: disk 1 removed"))?;
    read_back(vm)?;
    vm.expect(&format!("storage: /volumes/{first}: quota group 0/5 counts "))?;
    storage_end(vm, "check")
}

/// The `swap` test: swaptest (userspace/posix/swaptest.c) turns on the
/// area with swapon(8), fills more memory than the machine has and reads
/// it back, through a fork too, and turns the area off with swapoff(8)
/// while some of it is still out there; no domain may be stopped for
/// want of memory meanwhile.
fn swap(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect("spark: vda holds no FAT, exFAT, Btrfs or ext4 filesystem; left alone")?;
    vm.expect_ahead("spark: started /system/tests/posix/swaptest as process ")?;
    let slots = SWAP_KIB / 4 - 1;
    vm.expect(&format!("Swap: vda: {} MiB in {slots} slots, priority -2", slots * 4 / 1024))?;
    vm.expect(&format!("swaptest: swap is on, {} KiB", slots * 4))?;
    vm.expect("swaptest: filled 384 MiB")?;
    vm.expect("swaptest: read it back, ")?;
    vm.expect("swaptest: a child changed its copy")?;
    vm.expect("Swap: vda: off")?;
    vm.expect("swaptest: swap is off")?;
    vm.expect("swaptest: every check passed")?;
    let code = program_line(vm, "(/system/tests/posix/swaptest) exited with code ")?;
    if code != "0x0" {
        return Err(format!("swaptest exited with code {code}"));
    }
    let log = vm.log();
    if let Some(line) = log
        .lines()
        .find(|line| line.contains("Out of memory") || line.contains("no memory for a page"))
    {
        return Err(format!("memory ran out though swap was on: {}", line.trim()));
    }
    vm.expect("spark: nothing more to do; waiting")
}

/// Checks that the `swap` test's area is as `mkswap` made it: Linux's
/// header, with its last page, UUID and label.
fn swap_kept(disk: &DiskImage, _: &str) -> Result<(), String> {
    let mut page = [0; 4096];
    std::fs::File::open(&disk.path)
        .and_then(|file| std::os::unix::fs::FileExt::read_exact_at(&file, &mut page, 0))
        .map_err(|err| format!("reading {}: {err}", disk.path.display()))?;
    let word = |at: usize| u32::from_le_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]]);
    let label = &page[1052..1052 + 8];
    if &page[4086..] != b"SWAPSPACE2"
        || word(1024) != 1
        || u64::from(word(1028)) != SWAP_KIB / 4 - 1
        || label != b"SWAPTEST"
    {
        return Err(format!("{}: the swap area's header changed", disk.path.display()));
    }
    Ok(())
}

/// Checks that the replay was committed, the superblock naming no log,
/// and that Linux's `btrfs check` passes ([`btrfs_checked`]).
fn btrfs_replayed(disk: &DiskImage, log: &str) -> Result<(), String> {
    let mut root = [0; 8];
    std::fs::File::open(&disk.path)
        .and_then(|file| std::os::unix::fs::FileExt::read_exact_at(&file, &mut root, 0x1_0060))
        .map_err(|err| format!("reading {}: {err}", disk.path.display()))?;
    if u64::from_le_bytes(root) != 0 {
        return Err(format!("{} still names a log to replay", disk.path.display()));
    }
    btrfs_checked(disk, log)
}

/// The `disk-errors` test: the reads and writes of [`ERROR_DISKS`]'s
/// errors, each tried again by the kernel, up to five times: the SATA
/// disk's after recovering the port, which QEMU's AHCI disk stops for an
/// aborted command, and the NVMe disk's at once, which QEMU's controller
/// fails as a media error without Do Not Retry.
fn disk_errors(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    vm.expect_ahead("spark: started /system/tests/storage as process ")?;
    for name in disk_names(case.disks) {
        for line in [
            "a read of sector 6000, which fails once, reads what it holds",
            "a read of sector 10000, which always fails, fails with io",
            "a read up to sector 10000 ends short, after 65536 bytes",
            "a write of sector 12000, which always fails, fails with io",
            "reads and writes elsewhere work after the errors",
        ] {
            vm.expect(&format!("storage: /devices/{name}: {line}"))?;
        }
    }
    storage_end(vm, "errors")?;
    // The first read failed once and then worked; each of the others
    // went five times.
    let log = vm.log();
    let errors = log.matches("Disk: sda: an error stopped its port (status ").count();
    if errors != 1 + 5 * 3 {
        return Err(format!("the kernel recovered sda's port {errors} times, not 16"));
    }
    if !log.contains("; it runs again (16 errors since boot)") || log.contains("the disk is gone") {
        return Err("the kernel did not recover sda's port every time".into());
    }
    // Each disk's failed requests are logged, with what the NVMe
    // controller said.
    for (name, read, write) in [
        ("sda", "an I/O error", "an I/O error"),
        ("nvme0n1", "unrecovered read error", "write fault"),
    ] {
        for line in [
            format!(
                "Disk: {name}: a read of 8 sectors from sector 10000 failed after 5 tries: {read}"
            ),
            format!(
                "Disk: {name}: a read of 128 sectors from sector 10000 failed after 5 tries: {read}"
            ),
            format!(
                "Disk: {name}: a write of 8 sectors from sector 12000 failed after 5 tries: {write}"
            ),
        ] {
            if !log.contains(&line) {
                return Err(format!("the kernel never said \"{line}\""));
            }
        }
        if log.matches(&format!("Disk: {name}: a ")).count() != 3 {
            return Err(format!("the kernel logged other failures of {name}"));
        }
    }
    Ok(())
}

fn rcu(vm: &mut Vm, case: &Case) -> Result<(), String> {
    sched_case(vm, case, "rcu", "objects were retired, none too early")
}

fn oom(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("selftest oom: an app runs the machine out of memory")?;
    vm.expect("Out of memory: stopped domain `oom-app` (app class")?;
    vm.expect("selftest oom: a service runs the machine out of memory while another app runs")?;
    vm.expect("Out of memory: stopped domain `oom-app2` (app class")?;
    vm.expect("Out of memory: stopped domain `oom-service` (service class")?;
    vm.expect("Out of memory: restarting domain `oom-service`")?;
    selftest_passed(vm, "oom")?;
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

/// Spark supervises `hog`, which takes memory until the out-of-memory
/// policy stops its domain: Spark sees it end as stopped by the policy and
/// starts it again in the same domain, after a wait that doubles when it
/// happens again soon.
fn spark_oom(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("spark: 1 services in /system/tests/services, 1 enabled")?;
    vm.expect("Domains: `spark` made domain `hog` below `services` (service class, weight 100)")?;
    vm.expect("spark: started hog (/system/tests/posix/hog) as process ")?;
    // Spark copies the program's own lines into the log when it reads
    // them, which may be after the kernel's line that stops it (as under
    // KVM), so each round looks only for the kernel's lines and Spark's.
    for wait in ["0.1", "0.2"] {
        vm.expect("Out of memory: stopped domain `hog` (service class")?;
        vm.expect("spark: hog (process ")?;
        vm.expect(" was ended by signal 9 (stopped by the out-of-memory policy)")?;
        vm.expect(&format!("spark: starting hog again in {wait} s"))?;
        vm.expect("spark: started hog (/system/tests/posix/hog) as process ")?;
    }
    if !vm.log().contains("hog: taking memory") {
        return Err("the log does not show hog's own line, \"hog: taking memory\"".into());
    }
    Ok(())
}

/// The `logging` test: logd(8) starts with the services and keeps the
/// kernel log; logtest (userspace/posix/logtest.c) checks what log(1),
/// journalctl(1) and dmesg(1) show of it, and that logd started again
/// keeps no record twice (ADR 0210).
fn logging(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_anywhere("logd: keeping the log in /system/state/log")?;
    vm.expect_ahead("spark: started /system/tests/posix/logtest as process ")?;
    let code = program_line(vm, "(/system/tests/posix/logtest) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> =
        log.lines().filter(|line| line.contains("logtest: FAILED")).map(str::trim).collect();
    if !failed.is_empty() {
        return Err(format!("logtest failed: {}", failed.join("; ")));
    }
    if code != "0x0" || !log.contains("logtest: every check passed") {
        return Err(format!("logtest exited with code {code}"));
    }
    // Spark goes on supervising the services: no line ends the boot.
    vm.expect_anywhere("logd: keeping the log in /system/state/log, after record ")
}

/// The `principals` test: Spark starts principals
/// (userspace/posix/principals.c) as root with the names capability,
/// which starts itself as bob and as alice, each with a root of their own
/// name, and checks the second gate between them (ADR 0214).
fn principals(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_ahead("spark: started /system/tests/posix/principals as process ")?;
    let code = program_line(vm, "(/system/tests/posix/principals) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> =
        log.lines().filter(|line| line.contains("principals: FAILED")).map(str::trim).collect();
    if !failed.is_empty() {
        return Err(format!("principals failed: {}", failed.join("; ")));
    }
    if code != "0x0" || !log.contains("principals: every check passed") {
        return Err(format!("principals exited with code {code}"));
    }
    vm.expect("spark: nothing more to do; waiting")
}

/// The `services` test: Spark starts the shipped services from their
/// TOML files and servicetest (userspace/posix/servicetest.c), which
/// checks them, has Spark convert an old file, writes and edits a new one,
/// and breaks one under its running service (ADR 0216).
fn services(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_anywhere("spark: 7 services in /system/settings/spark/services, 7 enabled")?;
    vm.expect_ahead("spark: started /system/tests/posix/servicetest as process ")?;
    let code = program_line(vm, "(/system/tests/posix/servicetest) exited with code ")?;
    let log = vm.log();
    // The inside service's lines are Spark's copies of its output.
    let failed: Vec<&str> = log
        .lines()
        .filter(|line| {
            ["servicetest: FAILED", "inside: FAILED", "is in its view"]
                .iter()
                .any(|f| line.contains(f))
        })
        .map(str::trim)
        .collect();
    if !failed.is_empty() {
        return Err(format!("servicetest failed: {}", failed.join("; ")));
    }
    if code != "0x0" || !log.contains("servicetest: every check passed") {
        return Err(format!("servicetest exited with code {code}"));
    }
    for said in [
        "inside: every check passed",
        "spark: could not start inside: its secret token is not set: set it with `service secret \
         set inside token`",
        "spark: converted /system/settings/spark/services/legacy.service to legacy.toml; the old \
         file is legacy.service.old",
        "spark: /system/settings/spark/services/fresh.toml: line 5: restart must be always, \
         on-failure or never, not `sometimes`; it is not started",
        "spark: /system/settings/spark/services/demo.toml: line 5: restart must be always, \
         on-failure or never, not `sometimes`; it runs its last good copy",
    ] {
        if !log.contains(said) {
            return Err(format!("Spark did not say \"{said}\""));
        }
    }
    Ok(())
}

/// The `accounts` test: Spark starts the services, authd among them,
/// and accounttest (userspace/posix/accounttest.c) as root with the
/// names capability, which makes accounts with user(8), checks what
/// authd wrote and how it answers, and starts itself as alice
/// (ADR 0218).
fn accounts(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_anywhere("authd: 0 accounts; answering at /system/live/authd/socket")?;
    vm.expect_ahead("spark: started /system/tests/posix/accounttest as process ")?;
    let code = program_line(vm, "(/system/tests/posix/accounttest) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> =
        log.lines().filter(|line| line.contains("accounttest: FAILED")).map(str::trim).collect();
    if !failed.is_empty() {
        return Err(format!("accounttest failed: {}", failed.join("; ")));
    }
    if code != "0x0" || !log.contains("accounttest: every check passed") {
        return Err(format!("accounttest exited with code {code}"));
    }
    // What authd did is in the audit log: the accounts it made, the
    // wrong passwords, and the password changed.
    for said in [
        "authd: made the account alice (1000), an administrator's, for process ",
        "authd: made the account bob (1001), a user's, for process ",
        "authd: a wrong password for alice, from process ",
        "authd: a wrong password for nobody, from process ",
    ] {
        if !log.contains(said) {
            return Err(format!("authd did not say \"{said}\""));
        }
    }
    Ok(())
}

/// The `signin` test (ADR 0250): a machine with no account shows the
/// owner's sign-in on tty1, where alice's account is made and she is
/// signed in; signintest (userspace/posix/signintest.c), as root, then
/// makes bob, who signs in on tty2. Each sees only their own home, `who`
/// lists both, signing out brings the sign-in back, and a wrong password
/// is refused after a wait. The power button ends it.
fn signin(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_anywhere("authd: 0 accounts; answering at /system/live/authd/socket")?;
    vm.expect_anywhere("consoled: started /system/commands/login on tty1 as process ")?;
    // No account can sign in yet: the owner's comes first.
    vm.expect_screen(&[
        &format!("{RELEASE} {VERSION} (tty1)"),
        "This computer has no account yet: make yours, the owner's,",
    ])?;
    answer_prompt(vm, "Name:", "alice")?;
    answer_prompt(vm, "Full name (may be empty):", "Alice Liddell")?;
    answer_prompt(vm, "New password:", "queen of hearts")?;
    answer_prompt(vm, "The same again:", "queen of hearts")?;
    vm.expect_anywhere("authd: made the account alice (1000), an administrator's, for process ")?;
    vm.expect_anywhere("authd: alice signed in on pts/")?;
    vm.expect_screen(&["Made alice, number 1000, home /users/alice"])?;
    answer_prompt(vm, "$", "clear; id; pwd; echo \"$USER $HOME $SHELL\"; tty")?;
    vm.expect_screen(&[
        "uid=1000(alice) gid=1000(alice) groups=1000(alice),10(admins)",
        "alice /users/alice /system/commands/nukesh",
        "/devices/pts/",
    ])?;
    // Bob, whom signintest made, signs in on the second console.
    vm.expect_anywhere("signintest: bob is made")?;
    vm.press(&["alt-f2"])?;
    vm.expect_anywhere("consoled: started /system/commands/login on tty2 as process ")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty2)")])?;
    answer_prompt(vm, "Name:", "bob")?;
    answer_prompt(vm, "Password:", "white rabbit")?;
    vm.expect_anywhere("authd: bob signed in on pts/")?;
    // Which pseudo-terminals the consoles got depends on what opened one
    // first; signintest checks that they differ.
    vm.expect_anywhere("signintest: alice on pts/")?;
    vm.expect_anywhere("signintest: every check passed")?;
    // Each is themself, and alice's home is closed to bob.
    answer_prompt(vm, "$", "clear; id; ls /users/alice; echo \"status $?\"; who")?;
    vm.expect_screen(&[
        "uid=1001(bob) gid=1001(bob) groups=1001(bob)",
        "cannot open directory '/users/alice': Permission denied",
        "status 2",
        "alice    pts/",
        "bob      pts/",
    ])?;
    // His session's root holds no control right: he mounts on /tmp and
    // unmounts /processes for no one.
    answer_prompt(vm, "$", "clear; /system/tests/posix/signintest mount")?;
    vm.expect_screen(&["signintest-mount: every check passed"])?;
    // Bob signs out, and the sign-in comes back on a clear screen. A wrong
    // password then waits and fails, and the right one signs him in again.
    answer_prompt(vm, "$", "clear; exit")?;
    vm.expect_anywhere("authd: bob signed out on pts/")?;
    answer_prompt(vm, "Name:", "bob")?;
    answer_prompt(vm, "Password:", "cheshire cat")?;
    vm.expect_anywhere("authd: a wrong password for bob, from process ")?;
    vm.expect_screen(&["The name or the password is wrong."])?;
    answer_prompt(vm, "Name:", "bob")?;
    answer_prompt(vm, "Password:", "white rabbit")?;
    answer_prompt(vm, "$", "clear; whoami")?;
    vm.expect_screen(&["bob"])?;
    // Back on tty1, alice's session ran on through all of it.
    vm.press(&["alt-f1"])?;
    answer_prompt(vm, "$", "clear; ls /users/bob; echo \"status $?\"")?;
    vm.expect_screen(&["cannot open directory '/users/bob': Permission denied", "status 2"])?;
    vm.press_power_button()?;
    vm.expect("spark: every service has stopped")?;
    vm.expect("Power: `spark` asked to power off the machine; syncing the filesystems")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// Waits, for up to a minute, until the last line on the screen with
/// anything on it ends with `prompt`, then types `text` and Enter. Waiting
/// for the prompt itself, not text anywhere on the screen, keeps a line
/// from being typed before the program reads it: a password typed before
/// its prompt turns echo off would show.
fn answer_prompt(vm: &mut Vm, prompt: &str, text: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let screen = vm.screen()?;
        // The cursor's block, after the prompt, reads as no character: `?`.
        let shown = |line: &str| {
            let line = line.trim_end();
            line.strip_suffix('?').unwrap_or(line).trim_end().to_string()
        };
        let last = screen.iter().rev().map(|line| shown(line)).find(|line| !line.is_empty());
        if last.as_ref().is_some_and(|line| line.ends_with(prompt)) {
            return vm.type_text(&format!("{text}\n"));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the screen does not end with the prompt \"{prompt}\"; its last line: {}",
                last.unwrap_or_default()
            ));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The `views` test: Spark starts views (userspace/posix/views.c) as
/// root with the names capability, which builds a view for alice as
/// authd will build a session's and starts itself as alice in it
/// (ADR 0215).
fn views(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_ahead("spark: started /system/tests/posix/views as process ")?;
    let code = program_line(vm, "(/system/tests/posix/views) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> =
        log.lines().filter(|line| line.contains("views: FAILED")).map(str::trim).collect();
    if !failed.is_empty() {
        return Err(format!("views failed: {}", failed.join("; ")));
    }
    // Alice's run and root's each say so.
    if code != "0x0" || log.matches("views: every check passed").count() != 2 {
        return Err(format!("views exited with code {code}"));
    }
    vm.expect("spark: nothing more to do; waiting")
}

/// Spark starts two services that check the network capabilities they
/// got (userspace/spark/tests/services/network), both netgrant
/// (userspace/posix/netgrant.c): `granted` asks to connect out and to
/// listen on ports 8000 to 8009 and 9000, `none` for nothing.
fn spark_network(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("spark: 2 services in /system/tests/services/network, 2 enabled")?;
    // The two run at once, so their lines are read from the whole log.
    for (name, said) in [
        (
            "granted",
            "netgrant: Spark gave it the network to connect out and to listen on ports \
             8000-8009 and 9000",
        ),
        ("none", "netgrant: Spark gave it no network"),
    ] {
        vm.expect_ahead(&format!("{name}: netgrant: done"))?;
        vm.expect_ahead(&format!("spark: {name} (process "))?;
        let log = vm.log();
        let lines: Vec<&str> =
            log.lines().filter(|line| line.contains(&format!("{name}: netgrant: "))).collect();
        if let Some(failed) = lines.iter().find(|line| line.contains("FAILED")) {
            return Err(format!("the {name} service failed: {}", failed.trim()));
        }
        for needle in [said, "netgrant: every check passed"] {
            if !lines.iter().any(|line| line.contains(needle)) {
                return Err(format!("the {name} service never said \"{needle}\""));
            }
        }
    }
    let log = vm.log();
    for name in ["granted", "none"] {
        let ended = log
            .lines()
            .find_map(|line| line.split(&format!("spark: {name} (process ")).nth(1))
            .unwrap_or_default();
        if !ended.contains(") exited with code 0x0") {
            return Err(format!("the {name} service ended: {ended}"));
        }
    }
    Ok(())
}

/// Spark supervises three services: `stubborn` ignores SIGTERM, so 5 s
/// after `service stubborn restart`, and again after `stop`, Spark stops
/// its domain; `leaver` ends and leaves a process in its domain, which
/// Spark stops; `driver` makes the requests and powers the machine off.
fn spark_stop(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("spark: 3 services in /system/tests/services/stop, 3 enabled")?;
    vm.expect("spark: started leaver (/system/commands/nukesh) as process ")?;
    vm.expect("spark: started stubborn (/system/commands/nukesh) as process ")?;
    vm.expect("spark: started driver (/system/commands/nukesh) as process ")?;
    vm.expect("spark: asked to restart stubborn")?;
    vm.expect("spark: stubborn did not end in 5 s; stopping its domain")?;
    vm.expect("spark: stubborn (process ")?;
    vm.expect(" was ended by signal 9")?;
    vm.expect("spark: starting stubborn again in 0.1 s")?;
    vm.expect("spark: started stubborn (/system/commands/nukesh) as process ")?;
    vm.expect("spark: asked to stop stubborn")?;
    vm.expect("spark: stubborn did not end in 5 s; stopping its domain")?;
    vm.expect(" was ended by signal 9")?;
    vm.expect("driver:  [ - ]  stubborn")?;
    vm.expect("driver:    Active: inactive (dead)")?;
    vm.expect("spark: asked to power off the machine")?;
    vm.expect("spark: every service has stopped")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)?;
    // leaver's lines come whenever it ends, among the others.
    let log = vm.log();
    for line in [
        "leaver: left",
        "spark: leaver (process ",
        "spark: leaver left 1 thread behind; stopping its domain",
        "stubborn: up",
    ] {
        if !log.contains(line) {
            return Err(format!("the log does not show \"{line}\""));
        }
    }
    if log.contains("starting leaver again") || log.matches("starting stubborn again").count() != 1
    {
        return Err("a service was started again that should not have been".into());
    }
    Ok(())
}

fn microcode(vm: &mut Vm, case: &Case) -> Result<(), String> {
    // The boot CPU loads microcode before anything else, so these lines
    // come before the kernel's banner.
    let module = test_microcode().len();
    vm.expect(&format!(
        "microcode: module of {module} bytes with 2 updates; CPU signature {MICROCODE_SIGNATURE:#x}"
    ))?;
    // QEMU accepts the update but cannot apply it, so the revision stays.
    vm.expect(&format!("microcode: revision {MICROCODE_REVISION:#x} did not take effect"))?;
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    if !vm.log().contains("(GenuineIntel, family 0x6, model 0x5e, stepping 3, microcode revision") {
        return Err("the CPU line does not show the emulated Intel CPU".into());
    }
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every member of the kernel workspace with host tests is in
    /// KERNEL_LIBRARIES, so that `ob test unit` runs them and `ob lint`
    /// lints them.
    #[test]
    fn every_kernel_library_with_tests_is_tested() {
        #[derive(serde::Deserialize)]
        struct Manifest {
            workspace: Option<Workspace>,
            package: Option<Package>,
        }
        #[derive(serde::Deserialize)]
        struct Workspace {
            members: Vec<String>,
        }
        #[derive(serde::Deserialize)]
        struct Package {
            name: String,
        }
        fn has_tests(dir: &Path) -> bool {
            std::fs::read_dir(dir).unwrap().flatten().any(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    return has_tests(&path);
                }
                path.extension().is_some_and(|e| e == "rs")
                    && std::fs::read_to_string(&path).unwrap().contains("#[cfg(test)]")
            })
        }
        let kernel = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kernel");
        let read = |path: &Path| -> Manifest {
            toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let members = read(&kernel.join("Cargo.toml")).workspace.unwrap().members;
        assert!(members.len() > 40);
        for member in members {
            let dir = kernel.join(&member);
            let name = read(&dir.join("Cargo.toml")).package.unwrap().name;
            if has_tests(&dir.join("src")) {
                assert!(
                    KERNEL_LIBRARIES.contains(&name.as_str()),
                    "{name} is not in KERNEL_LIBRARIES"
                );
            }
        }
    }

    #[test]
    fn a_churn_cut_short_may_have_removed_one_file_unsaid() {
        // Rounds 0 to 9 synced, round 8 removed file-0000, and the power
        // went as round 9's removal of file-0001 was said.
        let mut log = String::new();
        for round in 0..10 {
            log += &format!("storage: churn: synced file-{round:04}\n");
            if round == 8 {
                log += "storage: churn: removed file-0000\n";
            }
        }
        log += "storage: churn: removed fi";
        let (synced, removed) = (churn_rounds(&log, "synced"), churn_rounds(&log, "removed"));
        assert_eq!(synced, (0..10).collect::<Vec<_>>());
        assert_eq!(removed, [0]);
        let dir = std::env::temp_dir().join(format!("churn-left-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for round in 1..10 {
            let bytes: Vec<u8> = (0..churn_size(round)).map(|at| churn_byte(round, at)).collect();
            std::fs::write(dir.join(format!("file-{round:04}")), bytes).unwrap();
        }
        let removing = synced.last().and_then(|last| last.checked_sub(CHURN_KEEP));
        assert_eq!(removing, Some(1));
        // file-0001 may stay, whole, or have gone.
        assert_eq!(churn_left(&dir, &synced, &removed, removing), Ok(()));
        std::fs::remove_file(dir.join("file-0001")).unwrap();
        assert_eq!(churn_left(&dir, &synced, &removed, removing), Ok(()));
        // Any other synced file may not.
        std::fs::remove_file(dir.join("file-0002")).unwrap();
        assert!(churn_left(&dir, &synced, &removed, removing).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn types_on_a_us_keyboard() {
        assert_eq!(key_combo('a').unwrap(), "a");
        assert_eq!(key_combo('Q').unwrap(), "shift-q");
        assert_eq!(key_combo('$').unwrap(), "shift-4");
        assert_eq!(key_combo('-').unwrap(), "minus");
        assert_eq!(key_combo('\n').unwrap(), "ret");
        assert!(key_combo('é').is_err());
    }

    #[test]
    fn disk_sizes_read_as_the_kernel_logs_them() {
        assert_eq!(disk_size(1440), "1.4 MiB");
        assert_eq!(disk_size(32 * 1024), "32 MiB");
        assert_eq!(disk_size(48 * 1024 - 1024), "47 MiB");
        assert_eq!(disk_size(3 << 19), "1.5 GiB");
    }

    #[test]
    fn gpt_test_tables_are_whole() {
        // The CRC's check value (the CRC catalogue's CRC-32/ISO-HDLC).
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        let sectors = 48 * 1024 * 2;
        assert_eq!(gpt_last(sectors, 512), 96_255);
        // In blocks of 4 KiB, the backup's 5 blocks end at the same MiB.
        assert_eq!(gpt_last(sectors, 4096), 96_255);
        for block in [512, 4096] {
            let b = block as usize;
            let table = 128 * 128 / b;
            let blocks = sectors / (block / 512);
            let (primary, backup) = gpt_tables(sectors, block);
            assert_eq!((primary.len(), backup.len()), ((2 + table) * b, (1 + table) * b));
            // Each header's CRC covers its 92 bytes with the CRC field as 0,
            // and the entries' CRC the 128 entries.
            for (header, at) in [(&primary[b..2 * b], 1), (&backup[table * b..], blocks - 1)] {
                let mut copy = header[..92].to_vec();
                copy[16..20].fill(0);
                assert_eq!(crc32(&copy).to_le_bytes(), header[16..20]);
                assert_eq!(header[24..32], u64::to_le_bytes(at));
                assert_eq!(crc32(&backup[..table * b]).to_le_bytes(), header[88..92]);
            }
            assert_eq!(primary[2 * b..], backup[..table * b]);
            // The partition, in blocks: from 1 MiB to the last whole MiB.
            let entry = &primary[2 * b..2 * b + 128];
            let per_mib = (1 << 20) / block;
            assert_eq!(entry[32..40], u64::to_le_bytes(per_mib));
            assert_eq!(entry[40..48], u64::to_le_bytes((96_256 / 2048) * per_mib - 1));
        }
    }

    #[test]
    fn strips_escapes() {
        assert_eq!(clean_log("\u{1b}[1;1HRadix\r\n\u{1b}[=3hok"), "Radix\nok");
    }

    #[test]
    fn a_failure_shows_the_whole_panic_report() {
        let numbered =
            |count: usize| (0..count).map(|i| format!("line {i}\n")).collect::<Vec<_>>().concat();
        // No panic: the last 15 lines.
        let log = numbered(40);
        let lines = failure_lines(&log);
        assert_eq!((lines.len(), lines[0]), (15, "line 25"));
        // A panic: from 10 lines before its report to the end.
        let report =
            format!("{PANIC_BANNER} ====================\nRadix panic on CPU 0\n{}", numbered(30));
        let log = format!("{}{report}", numbered(40));
        let lines = failure_lines(&log);
        assert_eq!((lines.len(), lines[0]), (42, "line 30"));
        assert!(lines[10].starts_with(PANIC_BANNER));
        // The first report, not the replay of one kept from the boot
        // before, and at most 100 lines.
        let log = format!("{}[    0.000000]   | {report}{report}{}", numbered(3), numbered(200));
        let lines = failure_lines(&log);
        assert_eq!((lines.len(), lines[0]), (100, "line 20"));
        assert!(lines[10].starts_with(PANIC_BANNER));
    }

    #[test]
    fn fills_the_command_line_slot() {
        let slot = cmdline_slot();
        let image = format!("....multiboot2 /boot/radix {slot}\n....").into_bytes();
        let filled = with_cmdline(&image, "qemu-exit kdb").unwrap();
        let text = String::from_utf8(filled).unwrap();
        assert!(text.contains("/boot/radix qemu-exit kdb   "));
        assert_eq!(text.len(), image.len());
        assert!(with_cmdline(b"no slot", "x").is_err());
        let twice = format!("{slot}{slot}").into_bytes();
        assert!(with_cmdline(&twice, "x").is_err());
        // Padded by bytes, so a line with wider characters fills it too.
        let wide = String::from_utf8(with_cmdline(&image, "hostname=Dukovó").unwrap()).unwrap();
        assert_eq!(wide.len(), image.len());
        assert!(with_cmdline(&image, &"é".repeat(CMDLINE_SLOT)).is_err());
    }

    #[test]
    fn shards_split_the_cases_between_them() {
        let cases: Vec<&Case> = all_cases().collect();
        let mut seen: Vec<&str> = Vec::new();
        for part in 1..=3 {
            seen.extend(in_shard(cases.clone(), part, 3).iter().map(|case| case.name));
        }
        seen.sort_unstable();
        let mut all: Vec<&str> = cases.iter().map(|case| case.name).collect();
        all.sort_unstable();
        assert_eq!(seen, all);
        assert_eq!(parse_shard("2/3").unwrap(), (2, 3));
        for bad in ["0/3", "4/3", "3", "a/b", "1/0"] {
            assert!(parse_shard(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn case_names_are_unique_and_command_lines_fit() {
        let cases: Vec<&Case> = all_cases().collect();
        for (i, case) in cases.iter().enumerate() {
            assert!(cases[..i].iter().all(|other| other.name != case.name), "{}", case.name);
            assert!(case.cmdline.len() <= CMDLINE_SLOT);
            crate::config::validate_cmdline(case.cmdline).unwrap();
        }
    }

    #[test]
    fn a_failed_jitter_test_is_accepted_only_under_kvm() {
        let all = "ChaCha20 seeded before KASLR from RDSEED, RDRAND and timing jitter \
                   (4096 samples, 37 stuck)";
        // From a boot test on an AMD EPYC 9V45 CI runner.
        let steady = "ChaCha20 seeded before KASLR from RDSEED and RDRAND; failed its \
                      health test: timing jitter (4096 samples, 2079 stuck)";
        for kvm in [false, true] {
            assert!(check_random(all, true, kvm).is_ok());
        }
        assert!(check_random(steady, true, true).is_ok());
        assert!(check_random(steady, true, false).is_err());

        // RDSEED and RDRAND must pass wherever the CPU has them.
        let bad_rdrand = "ChaCha20 seeded before KASLR from RDSEED and timing jitter \
                          (4096 samples, 37 stuck); failed its health test: RDRAND";
        let missing = "ChaCha20 seeded before KASLR from timing jitter (4096 samples, \
                       134 stuck); the CPU has no RDSEED or RDRAND";
        for kvm in [false, true] {
            assert!(check_random(bad_rdrand, true, kvm).is_err());
            assert!(check_random(missing, true, kvm).is_err());
            assert!(check_random("", true, kvm).is_err());
        }

        // An emulated CPU without them seeds from the jitter alone.
        assert!(check_random(missing, false, false).is_ok());
        let none = "ChaCha20 seeded before KASLR, but no source passed its health test; \
                    failed its health test: timing jitter (4096 samples, 2079 stuck); the \
                    CPU has no RDSEED or RDRAND";
        assert!(check_random(none, false, false).is_err());
        assert!(check_random(none, false, true).is_ok());
    }

    #[test]
    fn test_microcode_parses() {
        let file = test_microcode();
        assert_eq!(radix_microcode::updates(&file).flatten().count(), 2);
        let cpu = radix_microcode::CpuId { signature: MICROCODE_SIGNATURE, platform: 1 };
        assert_eq!(radix_microcode::find(&file, cpu).unwrap().revision(), MICROCODE_REVISION);
    }
}
