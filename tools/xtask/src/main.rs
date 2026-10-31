// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob`, the Dukovo build tool.
//!
//! The `./ob` shell wrapper at the top of the repository builds this program
//! with the pinned toolchain and runs it. Every command is documented in
//! ob(1); `docs/man/ob.1.scd` is the source of that page.

mod account;
mod bench;
mod btrfs;
mod config;
mod corpus;
mod docs;
mod doctor;
mod elf;
mod fixture;
mod frames;
mod image;
mod kernel;
mod krelocs;
mod ksyms;
mod libc_check;
mod licenses;
mod lint;
mod manuals;
mod patched;
mod ports;
mod process;
mod qemu;
mod screen;
mod signoff;
mod test;
mod tla;
mod userspace;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use config::{Config, Profile};

/// The Dukovo version, shared by the kernel and every image.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Every command with a one-line summary, in the order `ob help` lists them.
/// `ob docs --check` verifies that ob(1) documents each one.
pub const COMMANDS: &[(&str, &str)] = &[
    ("setup", "Install the pinned Rust toolchain, then run doctor and crates"),
    ("doctor", "Check host tools and print the commands that install what is missing"),
    ("build", "Build the kernel, with its symbol table, and the user programs into out/sysroot"),
    ("ports", "Build the ports (the C library and programs from elsewhere) into out/sysroot"),
    ("crates", "Fetch the crates.io crates the kernel patches, and patch them into kernel/patched"),
    ("image", "Build the bootable USB image (or an ISO with --iso)"),
    ("run", "Boot the image in QEMU with the serial console on this terminal"),
    ("test", "Run unit tests, then boot the image in QEMU under UEFI and BIOS"),
    ("bench", "Measure system calls and IPC round trips on Radix and on this machine's Linux"),
    ("fixture", "Make test data that only Linux makes, such as a Btrfs log of fsyncs"),
    ("docs", "Build the man pages and the handbook into out/docs"),
    ("lint", "Check formatting and run Clippy with warnings denied"),
    ("fmt", "Format all Rust code"),
    ("signoff", "Check new commits for a Developer Certificate of Origin sign-off"),
    ("clean", "Remove build output"),
    ("help", "Show help for ob or for one command"),
];

const USAGE: &str = "\
Usage: ./ob [--verbose] <command> [options]
       ./ob --container <command> [options]

The Dukovo build tool. Settings come from dukovo.toml; options override them.

Commands:
";

const FOOTER: &str = "
Global options:
  -v, --verbose    Print every external command before running it
  --container      Run the command inside the pinned build container
                   (Podman or Docker); handled by the ./ob wrapper
  -h, --help       Show this help, or a command's help after its name
  -V, --version    Print the Dukovo version

Start with `./ob setup`, then `./ob run`. Full manual: ob(1), or
`man -l out/docs/man/man1/ob.1` after `./ob docs`.";

/// Per-command usage, shown by `ob help <command>` and `ob <command> --help`.
fn command_usage(name: &str) -> Option<&'static str> {
    Some(match name {
        "setup" => {
            "Usage: ./ob setup

Installs the Rust toolchain pinned in rust-toolchain.toml with rustup,
including its rust-src component (the kernel builds `core` itself) and the
aarch64-unknown-none target (CI checks the AArch64 HAL stubs), then runs
`./ob doctor` and `./ob crates`, so that editors find the crates the kernel
patches."
        }
        "crates" => {
            "Usage: ./ob crates

Fetches each crate from crates.io that kernel/patches/crates.toml lists,
checks the SHA-256 of its .crate file, applies its patch from
kernel/patches and puts it in kernel/patched, where kernel/Cargo.toml takes
it from in place of crates.io's (ADR 0182). A crate already there from the
same file and patch is left as it is. ob's own commands do this before any
Cargo command on the kernel; run it before Cargo commands of your own, such
as cargo-deny's."
        }
        "doctor" => {
            "Usage: ./ob doctor

Checks every host tool the build needs and prints the exact command that
installs anything missing on Debian, Ubuntu, Fedora or Arch Linux.
Exits with status 1 when a required tool is missing."
        }
        "build" => {
            "Usage: ./ob build [--debug | --release]

Builds the Radix kernel for its own target (kernel/targets/), writes the
kernel symbol table into it, and places it in out/sysroot/boot/radix. Then
writes the C headers of the system call interface (abi/*.ridl) into
out/sysroot/system/include/radix, builds the ports that changed (see
`./ob help ports`) and the C test programs, builds the user programs
(userspace/, for userspace/targets/), places their test image in
out/sysroot/boot/usertests, and writes the boot archive.

  --debug      Build without optimisation
  --release    Build with optimisation (the default from dukovo.toml)"
        }
        "ports" => {
            "Usage: ./ob ports [--rebuild <name>]...

Builds the ports in ports/, each after those it needs: the C library
(mlibc), the C++ headers it is built with, and programs from elsewhere.
Each source is fetched with git into target/ports once, by the commit its
recipe pins, and checked to be that commit; a port is built again only
when its directory, the toolchain or a port it needs changed. The
toolchain is the host's Clang (20 or later) and LLD, wrapped by
out/toolchain/bin/x86_64-radix-cc and the Meson cross files in
out/toolchain/meson. `./ob build` runs this first.

  --rebuild <name>   Build this port even if nothing changed; repeat for more"
        }
        "image" => {
            "Usage: ./ob image [--iso] [--cmdline <flags>] [--microcode <path>]
                  [--disk-check <serial>] [--user <name> [--full-name <name>]
                  [--password-file <path>] [--test-cost]] [--debug | --release]

Builds the kernel, then a hybrid boot image that starts under UEFI and
legacy BIOS from a USB stick, a hard disk or optical media:
  out/dukovo-<version>-x86_64.img   (default)
  out/dukovo-<version>-x86_64.iso   (with --iso)
The .img ends with a Dukovo root partition: the system and the
subvolumes for what changes, on Btrfs.

  --iso                Name the output .iso for virtual machines and optical media,
                       without the Dukovo root partition
  --cmdline <flags>    Kernel flags instead of boot.cmdline (dukovo-boot(7))
  --microcode <path>   Intel microcode file or directory instead of image.microcode
  --disk-check <serial>
                       Add a boot entry that checks, and erases, the disk with
                       this serial number, and Linux's side of the check in
                       crosscheck/ (the handbook's Supported hardware chapter)
  --user <name>        Make an administrator's account of this name in the root,
                       asking for its password twice; authd makes its home at the
                       first boot (ADR 0218)
  --full-name <name>   The account's full name
  --password-file <path>
                       Take the password from the first line of this file
  --test-cost          Hash the password at the boot tests' Argon2id cost, which
                       is quick to check under emulation, and to guess"
        }
        "run" => {
            "Usage: ./ob run [--bios | --uefi] [--media usb|cdrom|disk] [--headless] [--gdb]
                [--cmdline <flags>] [--microcode <path>] [--user <name> ...]
                [--debug | --release] [-- <extra QEMU arguments>]

Builds the image and boots it in QEMU. The serial console is this terminal;
press Ctrl-A then X to quit QEMU. KVM is used when /dev/kvm is accessible.

  --bios               Boot through SeaBIOS (legacy BIOS)
  --uefi               Boot through OVMF (UEFI)
  --media <kind>       Attach the image as a USB stick, CD-ROM or SATA disk
  --headless           Do not open a QEMU window (automatic without a display)
  --gdb                Wait for GDB on localhost:1234; writes out/qemu/radix.gdb,
                       which stops at the kernel's first Rust function (adds
                       nokaslr, so the kernel runs where its symbols say)
  --cmdline <flags>    Kernel flags instead of boot.cmdline, such as \"kdb\"
  --microcode <path>   Intel microcode file or directory instead of image.microcode
  --user <name>        Make an administrator's account in the image, with
                       --full-name, --password-file and --test-cost as ./ob image
                       takes them"
        }
        "test" => {
            "Usage: ./ob test [unit | boot | kani | tla | corpus] [--case <name>]...
                 [--shard <part>/<parts>] [--timeout <seconds>] [--debug | --release]
       ./ob test --list

Runs host unit tests, then the boot tests: QEMU boots a test image under
UEFI and BIOS, from USB and CD-ROM, with 4 CPUs, and each test follows the
serial console, types kdb commands and reads the framebuffer from
screenshots. Logs go to out/test/. Give `unit` or `boot` to run only that
part; `kani` checks the Kani proof harnesses instead, `tla` the TLA+
models with TLC, and `corpus` compares Nukesh with dash and bash on POSIX's
shell tests and real projects' scripts, in a sandbox (minutes each, so plain
`ob test` leaves them out; TLC needs Java, and ob downloads the pinned TLA+
tools into .cache/tla). With `corpus`, --case names a source in
userspace/programs/nukesh/tests/corpus.toml. `boot --shard 2/3` runs every
third boot test from the second, as CI's parallel jobs do. --list names every
boot test; those that Phases 11 to 13 add from now on are listed only there
(ob(1) has them all). The boot tests are:
  uefi-usb, bios-usb, uefi-cdrom, bios-cdrom   normal boots (bios-usb with the
                   lock checker on, uefi-cdrom in xAPIC mode, bios-cdrom
                   without KASLR and PCIDs)
  screen           the boot log on the UEFI framebuffer
  consoles         a shell on consoled's virtual consoles: typing, Alt+F2,
                   a German keyboard layout, poweroff
  power-button     pressing the ACPI power button powers the machine off,
                   after Spark stops the services, both through uACPI
  aml-off          the same with aml=off, without uACPI, under UEFI
  acpi-power       a battery, AC adapter, lid and power button of an ACPI
                   table of the test's, in /sys/class/power_supply and
                   /proc/acpi/button/lid
  acpi-thermal     a thermal zone of an ACPI table of the test's, in
                   /sys/class/thermal, warms past its trip points to the
                   critical one, which powers the machine off
  usb-devices      the xHCI controller names the boot stick, a keyboard, two
                   hubs and the devices behind them, follows devices
                   plugged in and pulled out, and has no Debug Capability
  usb-input        a USB keyboard types into the console beside the PS/2
                   one, and repeats a held key; /devices/mouse reads a USB
                   mouse and a tablet
  watchdog, watchdog-tco   a Spark that stops responding leaves the hardware
                   watchdog unpetted, and it restarts the machine (QEMU's
                   i6300esb under BIOS, q35's TCO timer under UEFI)
  machine-check    QEMU puts errors in the CPUs' machine-check banks: a
                   corrected one is logged, memory errors are counted by
                   channel, an uncorrected one panics and the next boot
                   finds it
  machine-check-program   bad memory a program reads ends only it, with
                   SIGBUS, and its page leaves use for good
  spark-oom        a service that runs the machine out of memory is stopped
                   and started again
  swap             a program fills more memory than the machine has, with a
                   swap area Linux's mkswap made on, and gets it all back
  spark-stop       Spark stops a service that ignores SIGTERM, and what a
                   service leaves behind
  logging          logd keeps the kernel log and the services' lines; log,
                   journalctl and dmesg read them; a restart keeps none twice;
                   the audit log, and logtrim
  principals       alice and bob, each with a root of their own name, and the
                   second gate between them: owners, modes, signals, tracing
  views            alice in a view made of attached capabilities, as a
                   session's: real paths, .., links, a /tmp of its own
  services         service files in TOML: service check, new and edit, old
                   files converted, and the last good copy of a broken one
  accounts         authd, user and passwd: accounts made and their files,
                   passwords checked with a wait after a wrong one, and alice
                   changing her own
  signin           sign-in on the consoles: the owner's account at the first
                   boot, two people side by side, neither in the other's home
  elevate          elevate and sudo: alice runs commands as root after her
                   password, once a pane for 15 minutes; bob may not
  startup-trouble  the boot menu's startup-trouble entry resets an
                   administrator's password, and she is told at sign-in
  spark-network    Spark gives each service the network its file asks for
  page-fault       a page fault's panic report, on serial and on screen
  crash-log-uefi   the panic report survives a restart (UEFI)
  crash-log-bios   the same under BIOS
  kdb              a breakpoint and every kdb command
  mm-stress        tens of millions of heap objects on 4 CPUs and 2 NUMA nodes
  tlb              TLB shootdowns and address-space switches
  framework-ec     a Framework EC's ACPI device with no EC behind it, and ectool
  smbus            q35's SMBus controller, its memory modules' EEPROMs and DDR4 SPD data
  smbios           QEMU's SMBIOS tables under UEFI, /sys/class/dmi/id and hwinfo
  gpio             an AMD and an Intel GPIO controller in a test device's memory,
                   and the firmware's events on their pins
  msr              a read of an MSR the CPU lacks faults and goes on
  caps             no capability forged or widened; revokes against 3 CPUs
  stack-overflow   a kernel stack overflow stops at its guard page
  smep, smap       the kernel cannot run or read a user page
  microcode        a microcode module on an emulated Intel CPU
  fpu              vector registers survive thousands of thread switches
  sched-fair       fair shares by nice level and resource-domain CPU weight
  sched-rt         real-time preemption and timer latency, FIFO and round robin
  sched-pi         priority inheritance, plain and through a chain of mutexes
  sched-balance    threads spread from one CPU over 4 CPUs and 2 NUMA nodes
  lockdep          the lock checker catches seeded deadlocks
  rcu              RCU readers never see an object retired too early
  user             a user program checks every system call and method;
                   8 kinds of fault each end only their program
  ipc              user programs ping-pong over endpoints, capabilities in
                   messages, notifications, channels and rings, on 1 and 2 CPUs
  fuzz             groups of programs make random system calls; nothing leaks
  spark            the kernel starts Spark, which starts a program and reaps it
  storage          four FAT disks: what is written reads back after a reboot
                   and on the host
  storage-sata     the same on four SATA disks of an AHCI controller, the
                   partition in a GPT
  storage-usb      the same on four USB sticks, two behind a full-speed hub;
                   then one is pulled out and plugged in again
  storage-uas      the same on four disks of three USB devices that speak
                   UAS, at SuperSpeed and high speed; then one is pulled out
  volumes          volumed mounts a stick plugged in again by its label; a
                   program writes to it and ejects it, the host reads it back
  storage-nvme     the same on four NVMe namespaces of two controllers,
                   with a queue for each of 4 CPUs, the partition in a GPT
  storage-4k       the same on two virtio disks and two NVMe namespaces of
                   4 KiB sectors, the partition in a GPT of 4 KiB blocks
  exfat-write      the storage test on two exFAT disks Linux made; the host
                   checks them with fsck.exfat and The Sleuth Kit
  disk-raw         raw reads, writes and discards of a virtio, a SATA and an
                   NVMe disk, and Linux's block requests; the host checks
                   every byte
  disk-raw-4k      the same on a virtio disk and an NVMe namespace of 4 KiB
                   sectors
  iommu            disk-raw behind QEMU's VT-d unit, each device reaching
                   only what is mapped for it and raising only its own
                   interrupts; QEMU's edu device is stopped writing where
                   it was not given memory and raising another's interrupt
  iommu-amd        the same behind QEMU's AMD IOMMU
  disk-check       the disk check of a real machine, on an NVMe disk found
                   by its serial number; Linux's side checks it on the host
  disk-errors      QEMU fails SATA and NVMe reads and writes: retried, then
                   EIO
  disk-health      Spark's diskhealth service logs that a SATA disk and an
                   NVMe drive pass; diskhealth shows them at the console
  storage-tools    lsblk, blkid and findmnt name a FAT partition in a GPT
                   and a Btrfs disk, their labels and their mounts; fstrim
                   trims the Btrfs, iostat counts its discards
  btrfs            two Btrfs disks Linux made, compressed, mounted
                   read-only: every file reads as the host wrote it, a
                   damaged one fails
  btrfs-write      the storage test on two Btrfs disks Linux made, past
                   their first data chunk; Linux's btrfs check passes
  btrfs-stress     torture changes files on two Btrfs disks for two
                   minutes, one compressing, checking every byte; after a
                   reboot they hold what was synced, btrfs check passes
  btrfs-power-cut  the power is cut while Btrfs files are synced: btrfs
                   check passes, every synced file is whole
  btrfs-log        a Btrfs disk Linux left with a log of fsyncs: the mount
                   replays it, and every file lists as after Linux's replay
  btrfs-raid       a Btrfs RAID1 of two disks: written, then read back with one
                   pulled out, which a third replaces; quotas go on, a
                   fourth is added, every chunk moved, and the first taken
                   out; btrfs check passes after each boot, from each disk
                   alone too, and counts every quota group again
  btrfs-raid-pull  a disk of a Btrfs RAID1 fails at a random write while it
                   is written; back after a reboot, a scrub catches it up
  ext4-write       the storage test on two ext4 disks Linux made; the host
                   checks them with e2fsck and debugfs, the journal replayed
  ext4-power-cut   the power is cut while ext4 files are synced: e2fsck
                   passes after the journal's replay, every synced file is
                   whole
  root             Dukovo's root on a copy of the image: Spark mounts its
                   subvolumes, and what is written lasts through a reboot
  posix            C programs on mlibc, a Rust std one and the ported
                   utilities: fork, exec, signals, pipes, terminals, files,
                   threads
  oom              out of memory stops an app, then a restarted service
  net              a virtio network card behind QEMU's VT-d unit takes the
                   ip= and gateway= address; QEMU's router answers 4 pings
  sockets          a program connects and listens over TCP and UDP on lo and
                   a virtio card, only as far as its network capabilities
                   allow
  interfaces       a virtio card with no address: a bound socket sends a
                   DHCPDISCOVER and gets QEMU's offer, and ip sets it up
  dhcp             netd sets a virtio card up by DHCP, keeps the lease and
                   goes on with it, and takes the settings file's address

  --case <name>         Run only this boot test; repeat for more
  --timeout <seconds>   Limit for each boot test (default 300)"
        }
        "bench" => {
            "Usage: ./ob bench [--no-linux] [--gate] [--linux-kernel <file>] [--runs <n>]
                [--instructions] [--from <file>] [--summary <file>]
                [--debug | --release]

Boots Radix in QEMU on 2 CPUs with selftest=bench, where user programs time
the cheapest system calls and IPC round trips over endpoints,
notifications, shared-memory channels and rings, on one CPU and across
two. Then measures the closest Linux operations (tools/linux-bench) in a
virtual machine set up the same way, the baseline, and on this machine
for reference. Writes out/bench/report.md and out/bench/results.json and
prints the report. Without KVM the Radix numbers measure QEMU's emulation
and take minutes.

  --no-linux          Skip the Linux baseline
  --gate              Fail if Radix takes longer than Linux in the same
                      virtual machine at any operation, measuring both
                      again (up to 6 more runs) while one is over
  --linux-kernel <file>
                      The Linux kernel to boot for the baseline
  --runs <n>          Measure Radix and Linux in turn this many times and
                      keep each operation's fastest run (default 3; one
                      without KVM)
  --instructions      Count the instructions each operation takes instead,
                      under QEMU's instruction counting; with --gate, fail
                      if an operation on one CPU goes over its budget
  --from <file>       Measure nothing: report a real machine's numbers, from
                      the dukovo-bench.txt of the disk check's run.sh
  --summary <file>    Also append the Markdown report to <file> (CI passes
                      $GITHUB_STEP_SUMMARY)"
        }
        "fixture" => {
            "Usage: ./ob fixture btrfs-log [--linux-kernel <file>]

Makes test data that only Linux makes. btrfs-log boots Linux in QEMU on a
new Btrfs disk with tools/linux-bench as its only process, which commits
files, changes them with an fsync after each change and powers off, so the
changes are in Btrfs's log tree alone; then boots it on a copy to list the
disk before and after Linux replays the log. Writes the disk and the lists
to kernel/fs/btrfs/src/tests/fixtures, which the Btrfs host tests check
Radix's replay against.

  --linux-kernel <file>
                      The Linux kernel to boot (else OB_LINUX_KERNEL or this
                      machine's own); it needs Btrfs, built in or as a
                      module in OB_LINUX_MODULES"
        }
        "docs" => {
            "Usage: ./ob docs [--check]

Builds the man pages from docs/man/*.scd into out/docs/man (checked with
mandoc -T lint) and the handbook from docs/handbook into out/docs/handbook.

  --check      Also fail when an ob command or dukovo.toml key is not
               documented in its man page"
        }
        "lint" => {
            "Usage: ./ob lint

Checks formatting with rustfmt and runs Clippy on the host tools, the kernel
(for x86-64, and for AArch64 against the HAL stubs), the kernel libraries
and the user programs, treating every warning as an error."
        }
        "fmt" => {
            "Usage: ./ob fmt

Formats every Rust source file in the repository with rustfmt."
        }
        "signoff" => {
            "Usage: ./ob signoff [<base>]

Checks that every commit after <base> (by default, where HEAD left
origin/main) has a Signed-off-by line certifying the Developer Certificate
of Origin. A person signs off their own commits; a commit written by a tool,
such as an AI assistant, needs the sign-off of the person submitting it,
and a tool's own sign-off does not count. Merge commits are skipped.
CI runs this on the commits of every pull request."
        }
        "clean" => {
            "Usage: ./ob clean [--all]

Removes out/ and the kernel's and user programs' build directories.

  --all        Also remove target/, .cache/ and kernel/patched/ (the build
               tool, caches and the crates the kernel patches)"
        }
        "help" => {
            "Usage: ./ob help [<command>]

Shows the list of commands, or the help for one command."
        }
        _ => return None,
    })
}

fn print_usage() {
    print!("{USAGE}");
    for (name, summary) in COMMANDS {
        println!("  {name:<8} {summary}");
    }
    println!("{FOOTER}");
}

/// An error with an optional hint telling the user what to do about it.
#[derive(Debug)]
pub struct Error {
    message: String,
    hint: Option<String>,
}

impl Error {
    /// Creates an error from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), hint: None }
    }

    /// Adds a hint, printed on its own line after the message.
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Result type used throughout the build tool.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Returns early with a formatted [`Error`].
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::Error::new(format!($($arg)*)))
    };
}

/// Adds a description of what was being done to I/O errors.
pub trait Context<T> {
    /// Wraps the error as "`what`: <error>".
    fn context(self, what: impl FnOnce() -> String) -> Result<T>;
}

impl<T> Context<T> for io::Result<T> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|err| Error::new(format!("{}: {err}", what())))
    }
}

/// Paths and settings shared by every command.
pub struct Ctx {
    /// Repository root.
    pub root: PathBuf,
    /// Build output (`out/`).
    pub out: PathBuf,
    /// Cargo output (`target/`, or `$OB_TARGET_DIR`).
    pub target: PathBuf,
    /// Contents of `dukovo.toml`.
    pub config: Config,
    /// Print external commands before running them.
    pub verbose: bool,
}

impl Ctx {
    fn load(verbose: bool) -> Result<Self> {
        let root = std::env::var_os("OB_ROOT")
            .map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."), PathBuf::from);
        let root = root
            .canonicalize()
            .context(|| format!("cannot find the repository root at {}", root.display()))?;
        let target =
            std::env::var_os("OB_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
        let config = Config::load(&root.join("dukovo.toml"))?;
        Ok(Self { out: root.join("out"), target, config, verbose, root })
    }

    /// Path relative to the repository root, for messages.
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root).unwrap_or(path).display().to_string()
    }
}

/// Command-line arguments of one command, consumed as they are recognised.
pub struct Args {
    command: &'static str,
    args: Vec<String>,
    passthrough: Vec<String>,
}

impl Args {
    fn new(command: &'static str, mut args: Vec<String>) -> Self {
        let passthrough = match args.iter().position(|arg| arg == "--") {
            Some(index) => {
                let rest = args.split_off(index);
                rest[1..].to_vec()
            }
            None => Vec::new(),
        };
        Self { command, args, passthrough }
    }

    /// Removes `name` if present and reports whether it was.
    pub fn flag(&mut self, name: &str) -> bool {
        match self.args.iter().position(|arg| arg == name) {
            Some(index) => {
                self.args.remove(index);
                true
            }
            None => false,
        }
    }

    /// Removes `name <value>` or `name=<value>` and returns the value.
    pub fn value(&mut self, name: &str) -> Result<Option<String>> {
        let prefix = format!("{name}=");
        if let Some(index) = self.args.iter().position(|arg| arg.starts_with(&prefix)) {
            return Ok(Some(self.args.remove(index)[prefix.len()..].to_string()));
        }
        match self.args.iter().position(|arg| arg == name) {
            Some(index) if index + 1 < self.args.len() => {
                self.args.remove(index);
                Ok(Some(self.args.remove(index)))
            }
            Some(_) => bail!("{name} needs a value; see `./ob help {}`", self.command),
            None => Ok(None),
        }
    }

    /// Removes and returns the first argument that is not an option.
    pub fn positional(&mut self) -> Option<String> {
        let index = self.args.iter().position(|arg| !arg.starts_with('-'))?;
        Some(self.args.remove(index))
    }

    /// Reads `--debug` / `--release`, falling back to dukovo.toml.
    pub fn profile(&mut self, config: &Config) -> Result<Profile> {
        match (self.flag("--debug"), self.flag("--release")) {
            (true, true) => bail!("--debug and --release cannot be combined"),
            (true, false) => Ok(Profile::Debug),
            (false, true) => Ok(Profile::Release),
            (false, false) => Ok(config.build.profile),
        }
    }

    /// Arguments after `--`, for commands that pass them on.
    pub fn take_passthrough(&mut self) -> Vec<String> {
        std::mem::take(&mut self.passthrough)
    }

    /// Fails if any argument was not recognised.
    pub fn finish(&self) -> Result<()> {
        if let Some(arg) = self.args.first() {
            bail!(
                "unknown argument `{arg}` for `./ob {}`; see `./ob help {}`",
                self.command,
                self.command
            );
        }
        if !self.passthrough.is_empty() {
            bail!("`./ob {}` takes no arguments after `--`", self.command);
        }
        Ok(())
    }
}

fn dispatch(argv: Vec<String>) -> Result<()> {
    let mut verbose = false;
    let mut rest = argv.into_iter().peekable();
    while let Some(arg) = rest.peek() {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            "-V" | "--version" => {
                println!("ob {VERSION}");
                return Ok(());
            }
            "--container" => {
                return Err(Error::new("--container is handled by the ./ob wrapper script")
                    .hint("run the command through ./ob from the repository root"));
            }
            _ => break,
        }
        rest.next();
    }
    let Some(command) = rest.next() else {
        print_usage();
        return Ok(());
    };
    let Some(&(name, _)) = COMMANDS.iter().find(|(name, _)| *name == command) else {
        return Err(Error::new(format!("unknown command `{command}`"))
            .hint("run `./ob help` for the list of commands"));
    };
    let mut args = Args::new(name, rest.collect());
    if name != "help" && (args.flag("--help") || args.flag("-h")) {
        println!("{}", command_usage(name).unwrap_or_default());
        return Ok(());
    }
    if name == "help" {
        match args.positional() {
            Some(topic) => match command_usage(&topic) {
                Some(text) => println!("{text}"),
                None => bail!("unknown command `{topic}`; run `./ob help` for the list"),
            },
            None => print_usage(),
        }
        return args.finish();
    }

    let ctx = Ctx::load(verbose)?;
    match name {
        "setup" => {
            args.finish()?;
            doctor::setup(&ctx)
        }
        "doctor" => {
            args.finish()?;
            doctor::doctor(&ctx)
        }
        "crates" => {
            args.finish()?;
            patched::ensure(&ctx)
        }
        "build" => {
            let profile = args.profile(&ctx.config)?;
            args.finish()?;
            kernel::build(&ctx, profile)?;
            userspace::build(&ctx, profile).map(|_| ())
        }
        "ports" => {
            let mut rebuild = Vec::new();
            while let Some(name) = args.value("--rebuild")? {
                rebuild.push(name);
            }
            args.finish()?;
            userspace::install_headers(&ctx)?;
            ports::build_all(&ctx, &rebuild).map(|_| ())
        }
        "image" => {
            let iso = args.flag("--iso");
            let account = account::Options::from_args(&mut args)?;
            if iso && account.any() {
                bail!("--user makes an account in the root partition, which an .iso has none of");
            }
            let mut overrides = image::Overrides {
                cmdline: args.value("--cmdline")?,
                microcode: args.value("--microcode")?,
                disk_check: args.value("--disk-check")?,
                account: None,
            };
            let profile = args.profile(&ctx.config)?;
            args.finish()?;
            overrides.account = account.account(&ctx)?;
            let path = image::build_release(&ctx, profile, iso, &overrides)?;
            println!("\nImage ready: {}", ctx.display(&path));
            println!(
                "Write it to a USB stick as shown in ob(1), EXAMPLES, or boot it with ./ob run."
            );
            Ok(())
        }
        "run" => qemu::run_command(&ctx, &mut args),
        "test" => test::command(&ctx, &mut args),
        "bench" => bench::command(&ctx, &mut args),
        "fixture" => fixture::command(&ctx, &mut args),
        "docs" => {
            let check = args.flag("--check");
            args.finish()?;
            docs::build(&ctx, check)
        }
        "lint" => {
            args.finish()?;
            lint::lint(&ctx)
        }
        "fmt" => {
            args.finish()?;
            lint::fmt(&ctx)
        }
        "signoff" => {
            let base = args.positional();
            args.finish()?;
            signoff::check(&ctx, base.as_deref())
        }
        "clean" => {
            let all = args.flag("--all");
            args.finish()?;
            clean(&ctx, all)
        }
        _ => unreachable!("every command in COMMANDS is dispatched"),
    }
}

fn clean(ctx: &Ctx, all: bool) -> Result<()> {
    let mut paths = vec![
        ctx.out.clone(),
        ctx.target.join("kernel"),
        ctx.target.join("userspace"),
        ctx.root.join("kernel/target"),
    ];
    if all {
        paths.push(ctx.target.clone());
        paths.push(ctx.root.join(".cache"));
        paths.push(ctx.root.join(patched::DIR));
    }
    for path in paths {
        if path.exists() {
            process::step(&format!("Removing {}", ctx.display(&path)));
            std::fs::remove_dir_all(&path).context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = corpus::internal(&argv) {
        return code;
    }
    match dispatch(argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ob: error: {err}");
            if let Some(hint) = &err.hint {
                eprintln!("ob: hint: {hint}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Args {
        Args::new("run", list.iter().map(ToString::to_string).collect())
    }

    #[test]
    fn parses_flags_values_and_passthrough() {
        let mut a = args(&["--bios", "--media=cdrom", "--", "-s", "-S"]);
        assert!(a.flag("--bios"));
        assert!(!a.flag("--uefi"));
        assert_eq!(a.value("--media").unwrap().as_deref(), Some("cdrom"));
        assert_eq!(a.take_passthrough(), ["-s", "-S"]);
        a.finish().unwrap();
    }

    #[test]
    fn value_may_be_separate() {
        let mut a = args(&["--timeout", "60"]);
        assert_eq!(a.value("--timeout").unwrap().as_deref(), Some("60"));
        assert!(args(&["--timeout"]).value("--timeout").is_err());
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(args(&["--frobnicate"]).finish().is_err());
    }

    #[test]
    fn every_command_has_usage() {
        for (name, _) in COMMANDS {
            assert!(command_usage(name).is_some(), "{name} has no usage text");
        }
    }

    #[test]
    fn test_usage_lists_every_boot_test() {
        let usage = command_usage("test").unwrap();
        let words: Vec<&str> = usage
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| !w.is_empty())
            .collect();
        for case in test::CASES {
            assert!(words.contains(&case.name), "`ob help test` does not list {}", case.name);
        }
    }

    #[test]
    fn workspace_versions_match() {
        for (path, manifest) in [
            ("kernel/Cargo.toml", include_str!("../../../kernel/Cargo.toml")),
            ("userspace/Cargo.toml", include_str!("../../../userspace/Cargo.toml")),
            (
                "userspace/programs/Cargo.toml",
                include_str!("../../../userspace/programs/Cargo.toml"),
            ),
        ] {
            let table: toml::Table = manifest.parse().unwrap();
            let version = table["workspace"]["package"]["version"].as_str().unwrap();
            assert_eq!(version, VERSION, "{path} and Cargo.toml disagree on the version");
        }
    }
}
