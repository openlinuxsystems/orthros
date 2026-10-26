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

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::{Firmware, Media};
use crate::image::{self, ImageSpec};
use crate::process::{self, step};
use crate::qemu::{self, Machine, Serial};
use crate::{Args, Ctx, Error, Result, VERSION, corpus, doctor, kernel, screen, tla, userspace};

/// QEMU's exit status after the kernel writes 0x10 to isa-debug-exit.
pub const QEMU_EXIT_SUCCESS: i32 = 33;
/// QEMU's exit status after the kernel writes 0x11 (a panic).
const QEMU_EXIT_PANIC: i32 = 35;
/// QEMU's exit status when the guest restarts under `-no-reboot`.
const QEMU_EXIT_RESET: i32 = 0;
/// QEMU's exit status when the guest powers the machine off.
const QEMU_EXIT_POWER_OFF: i32 = 0;
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
    "radix-bootfs",
    "radix-block",
    "radix-bootinfo",
    "radix-cap",
    "radix-elf",
    "radix-fat",
    "radix-fbcon",
    "radix-hal",
    "radix-hw",
    "radix-input",
    "radix-ipc",
    "radix-kdb",
    "radix-klog",
    "radix-lockdep",
    "radix-ksyms",
    "radix-microcode",
    "radix-mm",
    "radix-nvme",
    "radix-posix",
    "radix-random",
    "radix-relocs",
    "radix-sched",
    "radix-sync",
    "radix-tty",
    "radix-vfs",
    "radix-virtio",
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

/// A disk of a boot test: a FAT filesystem `mkfs.fat` makes, or raw
/// bytes.
pub struct Disk {
    /// Its size in KiB.
    kib: u64,
    /// 12, 16 or 32; or 0 for no filesystem, the disk holding
    /// [`storage_big_byte`]'s bytes.
    fat: u8,
    /// The filesystem's label.
    label: &'static str,
    /// Whether the filesystem is in an MBR partition from 1 MiB on, rather
    /// than the whole disk.
    partitioned: bool,
    bus: Bus,
    /// More of QEMU's properties of the disk's device, each after a comma.
    device: &'static str,
    /// Errors for QEMU's blkdebug driver to inject, as its configuration
    /// file.
    errors: Option<&'static str>,
    /// What the host checks after each boot: [`fsck`] for a FAT disk.
    check: fn(&DiskImage) -> Result<(), String>,
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
}

impl Disk {
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
/// size, and what checks it.
pub struct DiskImage {
    path: PathBuf,
    offset: u64,
    bytes: u64,
    check: fn(&DiskImage) -> Result<(), String>,
}

/// The names the kernel gives a case's disks: `vda`, `vdb` and on for
/// virtio ones, `sda`, `sdb` and on for SATA ones, each in their order,
/// and `nvme<n>n1` and on for each NVMe controller's namespaces.
fn disk_names(disks: &[Disk]) -> Vec<String> {
    let (mut virtio, mut sata) = (b'a', b'a');
    let mut namespaces = [0u32; 256];
    disks
        .iter()
        .map(|disk| {
            let (prefix, letter) = match disk.bus {
                Bus::Virtio => ("vd", &mut virtio),
                Bus::Sata => ("sd", &mut sata),
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

/// The volumes Spark mounts from a case's FAT disks: each disk, or its
/// partition.
fn volume_names(disks: &[Disk]) -> Vec<String> {
    disk_names(disks)
        .into_iter()
        .zip(disks)
        .filter(|(_, disk)| disk.fat != 0)
        .map(|(name, disk)| if disk.partitioned { partition_name(&name, 1) } else { name })
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
const fn fat_disk(kib: u64, fat: u8, label: &'static str, partitioned: bool, bus: Bus) -> Disk {
    Disk { kib, fat, label, partitioned, bus, device: "", errors: None, check: fsck }
}

/// The storage test's disks: FAT12, FAT16 and FAT32 on whole disks, and
/// FAT32 in an MBR partition, as virtio disks. Spark mounts them at
/// `/volumes/vda`, `vdb`, `vdc` and `vdd1`.
const STORAGE_DISKS: &[Disk] = &[
    fat_disk(1440, 12, "RADIX12", false, Bus::Virtio),
    fat_disk(32 * 1024, 16, "RADIX16", false, Bus::Virtio),
    fat_disk(64 * 1024, 32, "RADIX32", false, Bus::Virtio),
    fat_disk(48 * 1024, 32, "RADIXPART", true, Bus::Virtio),
];

/// The same disks on SATA, the last one solid state (QEMU's rotation rate
/// of 1).
const SATA_DISKS: &[Disk] = &[
    fat_disk(1440, 12, "RADIX12", false, Bus::Sata),
    fat_disk(32 * 1024, 16, "RADIX16", false, Bus::Sata),
    fat_disk(64 * 1024, 32, "RADIX32", false, Bus::Sata),
    Disk { device: ",rotation_rate=1", ..fat_disk(48 * 1024, 32, "RADIXPART", true, Bus::Sata) },
];

/// The same disks as NVMe namespaces, two on each of two controllers:
/// `nvme0n1`, `nvme0n2`, `nvme1n1` and `nvme1n2p1`.
const NVME_DISKS: &[Disk] = &[
    fat_disk(1440, 12, "RADIX12", false, Bus::Nvme(0)),
    fat_disk(32 * 1024, 16, "RADIX16", false, Bus::Nvme(0)),
    fat_disk(64 * 1024, 32, "RADIX32", false, Bus::Nvme(1)),
    fat_disk(48 * 1024, 32, "RADIXPART", true, Bus::Nvme(1)),
];

/// The size of the raw disks of the `disk-raw` and `disk-errors` tests.
const RAW_KIB: u64 = 8 * 1024;

/// A raw test disk on `bus`, which the host checks with `check`.
const fn raw_disk(bus: Bus, check: fn(&DiskImage) -> Result<(), String>) -> Disk {
    Disk {
        kib: RAW_KIB,
        fat: 0,
        label: "",
        partitioned: false,
        bus,
        device: "",
        errors: None,
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

/// What the `disk-raw` test's program writes to each disk: at these
/// offsets, these many bytes, each the complement of the one there
/// before; and one write of 20 bytes 10 before the end, of which 10 fit.
/// Then it discards [`RAW_DISCARD`] (offset and length), which QEMU
/// zeroes. The program (userspace/storage) has the same lists.
const RAW_WRITES: &[(u64, u64)] =
    &[(513, 3), (3 * 512 - 1, 2), (20 * 4096 + 100, 70_000), (1 << 20, 8192)];
const RAW_DISCARD: (u64, u64) = (4 << 20, 1 << 20);

/// The `disk-errors` test's disks, a SATA disk and an NVMe namespace,
/// and the errors QEMU gives their reads and writes ([`ERROR_RULES`]):
/// a read of sector 6000 fails once, one of sector 10000 every time, and
/// so does a write of sector 12000. The program (userspace/storage) reads
/// and writes those sectors.
const ERROR_DISKS: &[Disk] = &[
    Disk { errors: Some(ERROR_RULES), ..raw_disk(Bus::Sata, raw_unchanged) },
    Disk { errors: Some(ERROR_RULES), ..raw_disk(Bus::Nvme(0), raw_unchanged) },
];

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

/// The CPU the microcode test emulates: an Intel Skylake client part,
/// signature 0x506e3.
const MICROCODE_CPU: &str = "qemu64,vendor=GenuineIntel,family=6,model=94,stepping=3";
const MICROCODE_SIGNATURE: u32 = 0x506e3;
const MICROCODE_REVISION: u32 = 0x7f;

/// The boot tests, in the order they run.
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
        about: "a normal boot shows a shell on consoled's virtual consoles; typing, Alt+F2, poweroff",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        // No program: Spark starts the services, as on a normal boot.
        cmdline: "",
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
        about: "Spark mounts four FAT disks; what is written reads back after a reboot and on the host",
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
        about: "The storage test on four SATA disks of an AHCI controller, one of them solid state",
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
        name: "storage-nvme",
        about: "The storage test on four NVMe namespaces of two controllers, each with a queue and an interrupt for each of four CPUs",
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
        name: "posix",
        about: "C programs on mlibc, a Rust std one and the ported utilities: fork, exec, signals, pipes, terminals, files, threads",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "lockdep -- --posix /system/tests/posix/run",
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
    let profile = args.profile(&ctx.config)?;
    let which = args.positional();
    args.finish()?;
    let (unit, boot) = match which.as_deref() {
        None if selected.is_empty() => (true, true),
        None | Some("boot") => (false, true),
        Some("unit") if selected.is_empty() => (true, false),
        Some("kani") if selected.is_empty() => return kani(ctx),
        Some("tla") if selected.is_empty() => return tla::check(ctx),
        Some("corpus") => return corpus::test(ctx, &selected),
        Some(suite @ ("unit" | "kani" | "tla")) => {
            crate::bail!("--case selects boot tests; it cannot be used with `{suite}`")
        }
        Some(other) => crate::bail!(
            "unknown test suite `{other}`; use `unit`, `boot`, `kani`, `tla` or `corpus`"
        ),
    };
    let cases: Vec<&Case> = if selected.is_empty() {
        CASES.iter().collect()
    } else {
        let mut cases = Vec::new();
        for name in &selected {
            match CASES.iter().find(|case| case.name == name) {
                Some(case) => cases.push(case),
                None => {
                    let names: Vec<_> = CASES.iter().map(|case| case.name).collect();
                    crate::bail!("no boot test `{name}`; the tests are {}", names.join(", "));
                }
            }
        }
        cases
    };

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
    let mut cmd = kernel::cargo(ctx, "test");
    for library in KERNEL_LIBRARIES {
        cmd.args(["-p", library]);
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
    "radix-ahci",
    "radix-nvme",
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
        let mut cmd = kernel::cargo(ctx, "kani");
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
        let log = dir.join(format!("boot-{}.log", case.name));
        let outcome = run_case(ctx, case, base, &dir, kvm, timeout);
        let secs = started.elapsed().as_secs_f32();
        match outcome {
            Ok(()) => eprintln!("    PASS  {:<15} {} ({secs:.1} s)", case.name, case.about),
            Err(reason) => {
                failures += 1;
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
    if failures > 0 {
        crate::bail!("{failures} of {} boot tests failed", cases.len());
    }
    step(&format!("All {} boot tests passed", cases.len()));
    Ok(())
}

fn run_case(
    ctx: &Ctx,
    case: &Case,
    base: &[u8],
    dir: &Path,
    kvm: bool,
    timeout: Duration,
) -> Result<(), String> {
    let image = dir.join(format!("{}.img", case.name));
    let qmp = dir.join(format!("{}.qmp", case.name));
    let disks = make_disks(dir, case)?;
    let mut extra: Vec<String> = case.qemu.iter().map(ToString::to_string).collect();
    if case.disks.iter().any(|disk| disk.bus == Bus::Sata) {
        extra.extend(["-device".into(), "ich9-ahci,id=sata".into()]);
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
        extra.push(format!("file={file},if=none,id=disk{i},format=raw,discard=unmap"));
        extra.push("-device".into());
        extra.push(match disk.bus {
            Bus::Virtio => format!("virtio-blk-pci,drive=disk{i}{}", disk.device),
            Bus::Sata => {
                port += 1;
                format!("ide-hd,drive=disk{i},bus=sata.{}{}", port - 1, disk.device)
            }
            Bus::Nvme(n) => {
                namespaces[usize::from(n)] += 1;
                let nsid = namespaces[usize::from(n)];
                format!("nvme-ns,drive=disk{i},bus=nvme{n},nsid={nsid}{}", disk.device)
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
        disks.iter().try_for_each(|disk| (disk.check)(disk))
    };
    let mut result = boot(case.name, case.cmdline, case.script);
    if let (Ok(()), Some(second)) = (&result, &case.second) {
        let name = format!("{}-reboot", case.name);
        result = (second.before)(case, &disks)
            .and_then(|()| boot(&name, second.cmdline, second.script))
            .map_err(|err| {
                let log = dir.join(format!("boot-{name}.log"));
                format!("after the reboot: {err} (serial log: {})", ctx.display(&log))
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

/// Makes the case's disks afresh in `dir`: with `mkfs.fat`, or filled
/// with [`storage_big_byte`]'s bytes; and the configuration files of
/// their errors.
fn make_disks(dir: &Path, case: &Case) -> Result<Vec<DiskImage>, String> {
    let mut made = Vec::new();
    for (i, disk) in case.disks.iter().enumerate() {
        let path = dir.join(format!("{}-disk{i}.img", case.name));
        let _ = std::fs::remove_file(&path);
        let image = DiskImage { path, offset: 0, bytes: disk.kib * 1024, check: disk.check };
        if let Some(rules) = disk.errors {
            let at = image.path.with_extension("blkdebug");
            std::fs::write(&at, rules).map_err(|err| format!("writing {}: {err}", at.display()))?;
        }
        if disk.fat == 0 {
            std::fs::write(&image.path, raw_bytes(image.bytes))
                .map_err(|err| format!("writing {}: {err}", image.path.display()))?;
            made.push(image);
            continue;
        }
        let path = &image.path;
        let mut mkfs = std::process::Command::new(fat_tool("mkfs.fat"));
        // A fixed volume ID, so that runs match.
        mkfs.args(["-F", &disk.fat.to_string(), "-n", disk.label, "-i", "4f525448"]);
        let offset = if disk.partitioned {
            let file = std::fs::File::create(path)
                .map_err(|err| format!("making {}: {err}", path.display()))?;
            file.set_len(disk.kib * 1024)
                .map_err(|err| format!("sizing {}: {err}", path.display()))?;
            write_mbr(path, disk.kib * 2)?;
            mkfs.args(["--offset", &PARTITION_START.to_string()]);
            mkfs.arg(path).arg((disk.kib - PARTITION_START / 2).to_string());
            PARTITION_START * 512
        } else {
            mkfs.arg("-C").arg(path).arg(disk.kib.to_string());
            0
        };
        host_tool(&mut mkfs)?;
        made.push(DiskImage { offset, ..image });
    }
    Ok(made)
}

/// What a raw test disk holds before the test: [`storage_big_byte`]'s
/// bytes.
fn raw_bytes(bytes: u64) -> Vec<u8> {
    (0..bytes as usize).map(storage_big_byte).collect()
}

/// Checks that a raw disk holds what it held before the boot.
fn raw_unchanged(disk: &DiskImage) -> Result<(), String> {
    raw_same(disk, &raw_bytes(disk.bytes))
}

/// Checks a disk of the `disk-raw` test: what [`RAW_WRITES`] wrote, the
/// write at the end, zeros where [`RAW_DISCARD`] discarded, and the bytes
/// it held before everywhere else.
fn raw_check(disk: &DiskImage) -> Result<(), String> {
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

/// Checks a disk's filesystem with `fsck.fat`, changing nothing: a
/// partition is copied out first, as `fsck.fat` reads whole files only.
fn fsck(disk: &DiskImage) -> Result<(), String> {
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
    /// catches up or 15 seconds pass.
    pub fn expect_screen(&mut self, needles: &[&str]) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let screen = self.screen()?;
            match screen_shows(&screen, needles) {
                Ok(()) => return Ok(()),
                Err(err) if Instant::now() >= deadline => return Err(err),
                Err(_) => std::thread::sleep(Duration::from_millis(500)),
            }
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
        writeln!(self.stream, "{command}").map_err(|err| format!("writing to QMP: {err}"))?;
        loop {
            let line = self.read_line()?;
            if line.contains(r#""return""#) {
                return Ok(());
            }
            if line.contains(r#""error""#) {
                return Err(format!("QMP: {}", line.trim()));
            }
        }
    }
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
    vm.expect("Capabilities: ready; 18 object kinds, 12 rights")?;
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
    vm.expect(&format!("Timer: all {cpus} CPUs take timer interrupts at 100 Hz"))?;
    if cpus > 1 {
        vm.expect(&format!("IPI: all {} other CPUs answered a ping", cpus - 1))?;
    }
    Ok(())
}

/// Fails if the kernel logged a problem no case looks for: a lock-checker
/// report (but in the case that seeds them) or a thread that slept where
/// it must not.
fn kernel_complaints(vm: &Vm, case: &Case) -> Result<(), String> {
    let log = vm.log();
    let complaint = log.lines().find(|line| {
        let checker = line.contains("Lock checker: ")
            && !line.contains("Lock checker: on; ")
            && !line.contains("Lock checker: off");
        (checker && case.name != "lockdep") || line.contains("with preemption held off")
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
    vm.expect("spark: 1 services in /system/settings/spark/services, 1 enabled")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect("consoled: console of ")?;
    vm.expect("consoled: started /system/commands/nukesh on tty1 as process ")?;
    vm.expect("consoled: 6 consoles; tty1 is shown")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    vm.type_text("echo $((6 * 7)) consoles\n")?;
    vm.expect_screen(&["# echo $((6 * 7)) consoles", "42 consoles"])?;
    vm.press(&["alt-f2"])?;
    vm.expect("consoled: started /system/commands/nukesh on tty2 as process ")?;
    vm.expect_screen(&["(tty2)"])?;
    vm.type_text("service --status-all; service consoled status\n")?;
    vm.expect_screen(&[
        " [ + ]  consoled",
        "consoled - Virtual consoles",
        "Active: active (running)",
    ])?;
    // Its shells end with it, and Spark starts it again.
    vm.type_text("service consoled restart\n")?;
    vm.expect("spark: asked to restart consoled")?;
    vm.expect("spark: consoled (process ")?;
    vm.expect(" was ended by signal 15")?;
    vm.expect("spark: starting consoled again in ")?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect("consoled: 6 consoles; tty1 is shown")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    vm.type_text("poweroff\n")?;
    vm.expect("spark: asked to power off the machine")?;
    vm.expect("spark: every service has stopped")?;
    vm.expect("Power: `spark` asked to power off the machine; syncing the filesystems")?;
    // QEMU ends when the guest enters ACPI's S5.
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

fn caps(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("selftest caps: ")?;
    vm.expect(" forgeries and ")?;
    vm.expect(" derivations by 3 other CPUs during a ")?;
    selftest_passed(vm, "caps")?;
    vm.expect_exit(QEMU_EXIT_SUCCESS)
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
    vm.expect("spark: built the hierarchy of 9 directories and mounted 8 filesystems")?;
    let number = program_line(vm, "spark: started /system/tests/hello as process ")?;
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
    vm.expect("spark: started /system/tests/posix/run as process ")?;
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
        // QEMU's controller takes shadow doorbells.
        let queues = if lanes == 1 {
            ", 1 I/O queue of 63 commands, one MSI-X vector, shadow doorbells".to_string()
        } else {
            format!(
                ", {lanes} I/O queues of 63 commands, an MSI-X vector for each of {lanes} CPUs, \
                 shadow doorbells"
            )
        };
        if !line.contains(": QEMU NVMe Ctrl, firmware ") || !line.ends_with(&queues) {
            return Err(format!("the kernel described nvme{n} as \"{line}\""));
        }
    }
    let mut port = 0;
    for (disk, name) in case.disks.iter().zip(disk_names(case.disks)) {
        let size = disk_size(disk.kib);
        let (line, want) = match disk.bus {
            Bus::Virtio => (
                line_of(&format!("Disk: {name} is a virtio block device of {size} at "))?,
                ", 512 bytes a block, write cache, discards, MSI-X vector ",
            ),
            Bus::Sata => {
                port += 1;
                (
                    line_of(&format!(
                        "Disk: {name} is a SATA disk of {size} on AHCI port {}: QEMU HARDDISK, ",
                        port - 1
                    ))?,
                    "512 bytes a block, NCQ depth 32, write cache, TRIM",
                )
            }
            Bus::Nvme(_) => (
                line_of(&format!("Disk: {name} is an NVMe namespace of {size}: QEMU NVMe Ctrl, "))?,
                "512 bytes a block, write cache, deallocates",
            ),
        };
        // Only a SATA disk's line says whether it is solid state.
        let says_solid = disk.bus == Bus::Sata && disk.solid();
        if !line.contains(want) || line.ends_with(", solid state") != says_solid {
            return Err(format!("the kernel described {name} as \"{line}\""));
        }
        if disk.partitioned {
            line_of(&format!("Disk: {name} has an MBR with 1 partition"))?;
            line_of(&format!(
                "Disk: {}: {} from sector {PARTITION_START}, type 0x0c",
                partition_name(&name, 1),
                disk_size(disk.kib - PARTITION_START / 2)
            ))?;
        } else {
            line_of(&format!("Disk: {name} has no partition table; it is used whole"))?;
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
    disks_found(vm, case)?;
    vm.expect("spark: built the hierarchy of 9 directories and mounted 8 filesystems")?;
    for (disk, name) in case.disks.iter().zip(volume_names(case.disks)) {
        let label = disk.label;
        vm.expect(&format!("Files: mounted fat from {name} on /volumes/{name}, labelled {label}"))?;
        vm.expect(&format!("spark: mounted the FAT filesystem of {name} at /volumes/{name}"))?;
    }
    vm.expect("spark: started /system/tests/storage as process ")
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

/// The first boot: the program writes its files to every volume.
fn storage_write(vm: &mut Vm, case: &Case) -> Result<(), String> {
    storage_boot(vm, case)?;
    let volumes = volume_names(case.disks);
    for name in &volumes {
        vm.expect(&format!("storage: /volumes/{name}: fat, "))?;
        vm.expect(&format!("storage: /volumes/{name}: wrote "))?;
    }
    vm.expect("storage: /tmp: a 10000-byte file written, read back and removed")?;
    vm.expect("storage: /devices: ")?;
    vm.expect(" of them disks; zero, random and null work")?;
    for name in &volumes {
        vm.expect(&format!("storage: /kernel/block/{name}/size: "))?;
    }
    vm.expect(&format!("storage: /kernel/version: Radix {VERSION} ({RELEASE} {VERSION})"))?;
    vm.expect("storage: /kernel/bus/pci/devices: ")?;
    vm.expect("storage: /processes lists ")?;
    vm.expect("1 spark (running)")?;
    vm.expect("storage (running)")?;
    vm.expect("storage: /processes/mounts lists 13 mounts, 4 of them FAT")?;
    vm.expect("storage: /processes/meminfo: ")?;
    storage_end(vm, "write")
}

/// Between the boots: the host reads what the program wrote with mtools,
/// and writes a file of its own to each disk, from beside the disks in
/// out/test (not the shared temporary directory, where another user could
/// put a link in its place).
fn storage_host(case: &Case, disks: &[DiskImage]) -> Result<(), String> {
    let Some(first) = disks.first() else { return Err("the case has no disks".into()) };
    let source = first.path.with_file_name("storage-from-host.txt");
    std::fs::write(&source, STORAGE_FROM_HOST)
        .map_err(|err| format!("writing {}: {err}", source.display()))?;
    let result = disks.iter().zip(volume_names(case.disks)).try_for_each(|(disk, name)| {
        let image = mtools_image(disk);
        let mtype =
            |path: &str| host_tool(std::process::Command::new("mtype").args(["-i", &image, path]));
        let same = |path: &str, want: &[u8]| -> Result<(), String> {
            let got = mtype(path)?;
            if got != want {
                return Err(format!(
                    "{image}: mtype {path} gives {} bytes, not the {} written",
                    got.len(),
                    want.len()
                ));
            }
            Ok(())
        };
        same("::/hello.txt", STORAGE_HELLO)?;
        same(&format!("::/notes/{STORAGE_LONG_NAME}"), format!("Saved on {name}.\n").as_bytes())?;
        same("::/notes/final.txt", b"final\n")?;
        same("::/log.txt", b"one\ntwo\n")?;
        same("::/cut.txt", &[b'x'; 1000])?;
        let big = mtype("::/big.bin")?;
        let want: Vec<u8> = (0..STORAGE_BIG).map(storage_big_byte).collect();
        if big != want {
            let at = big
                .iter()
                .zip(&want)
                .position(|(a, b)| a != b)
                .unwrap_or(big.len().min(want.len()));
            return Err(format!("{image}: big.bin as mtools reads it differs at byte {at}"));
        }
        let listing =
            host_tool(std::process::Command::new("mdir").args(["-i", &image, "-b", "::/many"]))?;
        // FAT names match without regard to case.
        let txt = |line: &&str| {
            Path::new(line).extension().is_some_and(|e| e.eq_ignore_ascii_case("txt"))
        };
        let files = String::from_utf8_lossy(&listing).lines().filter(txt).count();
        if files != STORAGE_MANY {
            return Err(format!("{image}: mdir lists {files} files in many/, not {STORAGE_MANY}"));
        }
        let to = "::/Written on the host.txt";
        host_tool(std::process::Command::new("mcopy").args(["-i", &image]).arg(&source).arg(to))?;
        Ok(())
    });
    let _ = std::fs::remove_file(&source);
    result
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

/// The `disk-raw` test: the program reads, writes and discards each disk
/// raw, and asks it Linux's block requests; the host then checks its
/// bytes ([`raw_check`]).
fn disk_raw(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    vm.expect("spark: started /system/tests/storage as process ")?;
    for (disk, name) in case.disks.iter().zip(disk_names(case.disks)) {
        let rotational = u8::from(!disk.solid());
        vm.expect(&format!(
            "storage: /devices/{name}: {} bytes, {} sectors of 512 bytes, physical blocks of 512, \
             minimum and optimal I/O 512 and 0, alignment 0, read-only 0, rotational \
             {rotational}, discarded sectors read as zeros 0",
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

/// The `disk-errors` test: the reads and writes of [`ERROR_DISKS`]'s
/// errors, each tried again by the kernel, up to five times: the SATA
/// disk's after recovering the port, which QEMU's AHCI disk stops for an
/// aborted command, and the NVMe disk's at once, which QEMU's controller
/// fails as a media error without Do Not Retry.
fn disk_errors(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    vm.expect("spark: started /system/tests/storage as process ")?;
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
    fn case_names_are_unique_and_command_lines_fit() {
        for (i, case) in CASES.iter().enumerate() {
            assert!(CASES[..i].iter().all(|other| other.name != case.name), "{}", case.name);
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
