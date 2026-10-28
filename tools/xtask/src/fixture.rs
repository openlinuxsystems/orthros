// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob fixture`: test data that only another system makes.
//!
//! `btrfs-log` boots Linux twice, with `linux-bench --btrfs-log` as its
//! init process, on a Btrfs disk `mkfs.btrfs` makes: first to write files,
//! commit them and change them with an fsync after each change, so that
//! the changes are in Btrfs's log tree alone when the machine goes off;
//! then, on a copy, to list the disk as its last commit has it and as
//! Linux replays the log. The disk, kept as [`sparse`] blocks, and the
//! lists go to the Btrfs host tests' fixtures, which check that Radix
//! replays the log as Linux does (ADR 0151).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{Firmware, Media};
use crate::process::{self, step};
use crate::qemu::{self, DirectBoot, Machine, Serial};
use crate::test::Vm;
use crate::{Args, Context, Ctx, Error, Result, bench};

/// Where the Btrfs host tests keep their fixtures.
pub const FIXTURES: &str = "kernel/fs/btrfs/src/tests/fixtures";

/// The start of a disk the fixtures keep ([`sparse`]).
const SPARSE_MAGIC: &[u8] = b"radix-sparse 1\n";

/// The disk's size: what `mkfs.btrfs` takes with its defaults, and room.
const DISK_BYTES: u64 = 128 << 20;

/// How long each boot may take, under emulation too.
const TIMEOUT: Duration = Duration::from_mins(15);

/// `ob fixture <name>`.
pub fn command(ctx: &Ctx, args: &mut Args) -> Result<()> {
    let name = args.positional();
    let kernel = args.value("--linux-kernel")?;
    args.finish()?;
    match name.as_deref() {
        Some("btrfs-log") => btrfs_log(ctx, kernel),
        Some(other) => crate::bail!("there is no fixture {other}: the one there is is btrfs-log"),
        None => crate::bail!("name the fixture to make: btrfs-log"),
    }
}

/// `ob fixture btrfs-log`.
fn btrfs_log(ctx: &Ctx, kernel: Option<String>) -> Result<()> {
    qemu::require_qemu()?;
    let Some(kernel) = bench::linux_kernel(kernel)? else {
        crate::bail!(
            "no Linux kernel to boot: give one with --linux-kernel or OB_LINUX_KERNEL, or let \
             this user read /boot/vmlinuz-$(uname -r)"
        );
    };
    let dir = ctx.out.join("fixture");
    process::create_dir(&dir)?;
    let initrd = bench::linux_init(ctx, &dir, None, &kernel)?;
    let disk = dir.join("btrfs-log.img");
    let file = std::fs::File::create(&disk).context(|| format!("making {}", disk.display()))?;
    file.set_len(DISK_BYTES).context(|| format!("sizing {}", disk.display()))?;
    let mut cmd = std::process::Command::new(crate::btrfs::mkfs(ctx)?);
    cmd.args(["-q", "-f", "-L", "LOG"]).arg(&disk);
    process::run(ctx, &mut cmd)?;
    step("Booting Linux to leave changes in the Btrfs log");
    boot(ctx, &dir, (&kernel, &initrd), &disk, "make", "linux-bench: logged")?;
    let image = std::fs::read(&disk).context(|| format!("reading {}", disk.display()))?;
    let log_root = image
        .get(0x1_0060..0x1_0068)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or_default()));
    if log_root == 0 {
        crate::bail!("Linux committed before the machine went off: there is no log to replay");
    }
    let copy = dir.join("btrfs-log.replayed.img");
    std::fs::copy(&disk, &copy).context(|| format!("writing {}", copy.display()))?;
    step("Booting Linux to list the disk before and after it replays the log");
    let log = boot(ctx, &dir, (&kernel, &initrd), &copy, "list", "LIST end")?;
    let release = bench::release_of(&kernel)?;
    let mut lists = format!(
        "# Made by `ob fixture btrfs-log` with Linux {release}: the disk as its last commit \
         has it, and as Linux replays its log.\n"
    );
    for line in log.lines().filter(|line| line.starts_with("LIST ")) {
        lists.push_str(line.trim_end());
        lists.push('\n');
    }
    let at = ctx.root.join(FIXTURES);
    process::create_dir(&at)?;
    write(&at.join("btrfs-log.sparse"), &sparse(&image))?;
    write(&at.join("btrfs-log.txt"), lists.as_bytes())?;
    println!("\nFixtures ready in {}", ctx.display(&at));
    Ok(())
}

/// Writes `bytes` to `path`.
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).context(|| format!("writing {}", path.display()))
}

/// Boots `kernel` with `initrd` on a virtual machine whose one disk is
/// `disk`, with `linux-bench --btrfs-log <mode>` as its init process,
/// until it prints `done` and goes off; returns its serial output.
fn boot(
    ctx: &Ctx,
    dir: &Path,
    (kernel, initrd): (&Path, &Path),
    disk: &Path,
    mode: &str,
    done: &str,
) -> Result<String> {
    let machine = Machine {
        firmware: Firmware::Bios,
        media: Media::Usb,
        ovmf: None,
        image: PathBuf::new(),
        memory: "1G".into(),
        cpus: 2,
        kvm: qemu::kvm_available(),
        cpu_model: None,
        serial: Serial::Pipe,
        display: false,
        reboot: false,
        gdb: false,
        qmp: None,
        extra: vec![
            "-drive".into(),
            format!("file={},if=virtio,format=raw", qemu::option_value(disk)),
        ],
        direct: Some(DirectBoot {
            kernel: kernel.to_path_buf(),
            initrd: initrd.to_path_buf(),
            cmdline: format!("{} -- --btrfs-log {mode}", bench::LINUX_CMDLINE),
        }),
    };
    let name = format!("btrfs-log-{mode}");
    let mut vm = Vm::start(ctx, &machine, dir, &name, TIMEOUT).map_err(Error::new)?;
    let outcome = vm.expect(done).and_then(|()| vm.expect_exit(0));
    let log = vm.log();
    drop(vm);
    if let Err(reason) = outcome {
        crate::bail!(
            "Linux's {mode} boot failed: {reason}; serial log: {}",
            ctx.display(&dir.join(format!("boot-{name}.log")))
        );
    }
    Ok(log)
}

/// `image` as the fixtures keep a disk: `radix-sparse 1\n`, the disk's
/// size, then each run of 4 KiB blocks not all zeros, as where it starts,
/// how long it is (8 bytes each, little-endian) and its bytes.
pub fn sparse(image: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 4096;
    let mut out = SPARSE_MAGIC.to_vec();
    out.extend_from_slice(&(image.len() as u64).to_le_bytes());
    let mut at = 0;
    while at < image.len() {
        let zero =
            |from: usize| image[from..(from + BLOCK).min(image.len())].iter().all(|&b| b == 0);
        if zero(at) {
            at += BLOCK;
            continue;
        }
        let start = at;
        while at < image.len() && !zero(at) {
            at += BLOCK;
        }
        let end = at.min(image.len());
        out.extend_from_slice(&(start as u64).to_le_bytes());
        out.extend_from_slice(&((end - start) as u64).to_le_bytes());
        out.extend_from_slice(&image[start..end]);
    }
    out
}

/// Writes the disk `kept` holds, as [`sparse`] keeps one, to `path`, its
/// blocks of zeros left as holes.
pub fn write_sparse(kept: &[u8], path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not a disk ob fixture kept");
    let rest = kept.strip_prefix(SPARSE_MAGIC).ok_or_else(bad)?;
    let word = |at: usize| -> std::io::Result<u64> {
        let bytes = rest.get(at..at + 8).ok_or_else(bad)?;
        Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| bad())?))
    };
    let file = std::fs::File::create(path)?;
    file.set_len(word(0)?)?;
    let mut at = 8;
    while at < rest.len() {
        let start = word(at)?;
        let len = usize::try_from(word(at + 8)?).map_err(|_| bad())?;
        let bytes = rest.get(at + 16..(at + 16).saturating_add(len)).ok_or_else(bad)?;
        file.write_all_at(bytes, start)?;
        at += 16 + len;
    }
    Ok(())
}
