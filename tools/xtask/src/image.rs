// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Building the hybrid boot image with grub-mkrescue.
//!
//! The image carries GRUB for both legacy BIOS (El Torito plus an MBR boot
//! record) and UEFI (an EFI system partition), so the same file boots from
//! a USB stick, a hard disk or optical media. An image for a disk has a
//! Dukovo root partition after grub-mkrescue's, with the system on Btrfs
//! (ADR 0137). Phase 14 replaces it with the GPT disk image assembled by
//! Dukovo's own rdisk and mkfs tools.

use std::fmt::Write;
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::account::{self, Account};
use crate::config::{self, Profile};
use crate::process::{self, step};
use crate::{Context, Ctx, Error, Result, VERSION, btrfs, kernel, licenses, userspace};

/// What goes into an image and where it is written.
pub struct ImageSpec<'a> {
    /// Kernel to boot.
    pub kernel: &'a Path,
    /// GRUB menu timeout in seconds.
    pub timeout: u32,
    /// Kernel command line.
    pub cmdline: &'a str,
    /// Intel microcode updates to load as a boot module, if any.
    pub microcode: Option<Vec<u8>>,
    /// The user programs: the test image and the boot archive, each
    /// loaded as a boot module.
    pub programs: Option<&'a userspace::Programs>,
    /// The serial number of the disk for a disk check entry in the boot
    /// menu, with Linux's side of the check beside it ([`disk_check`]).
    pub disk_check: Option<&'a str>,
    /// A file holding the Dukovo root's filesystem ([`make_root`]), to put
    /// after the image's partitions ([`append_root`]).
    pub root: Option<&'a Path>,
    /// Staging directory, recreated on every build.
    pub staging: PathBuf,
    /// Output file.
    pub output: PathBuf,
}

/// File name of a release image.
pub fn file_name(iso: bool) -> String {
    let extension = if iso { "iso" } else { "img" };
    format!("dukovo-{VERSION}-x86_64.{extension}")
}

/// Path of the microcode module inside the image.
pub const MICROCODE_PATH: &str = "/boot/firmware/intel-ucode.bin";

/// What `ob image` and `ob run` put into the image beyond dukovo.toml.
#[derive(Default)]
pub struct Overrides {
    /// Kernel flags instead of `boot.cmdline`.
    pub cmdline: Option<String>,
    /// Microcode file or directory instead of `image.microcode`.
    pub microcode: Option<String>,
    /// The serial number of a disk to add the disk check of (ADR 0122).
    pub disk_check: Option<String>,
    /// An account to make in the root (ADR 0218).
    pub account: Option<Account>,
}

/// Builds the kernel and the release image described by dukovo.toml.
pub fn build_release(
    ctx: &Ctx,
    profile: Profile,
    iso: bool,
    overrides: &Overrides,
) -> Result<PathBuf> {
    let microcode_source = overrides.microcode.as_deref().unwrap_or(&ctx.config.image.microcode);
    let microcode = if microcode_source.is_empty() {
        None
    } else {
        Some(read_microcode(ctx, Path::new(microcode_source))?)
    };
    let kernel = kernel::build(ctx, profile)?;
    let programs = userspace::build(ctx, profile)?;
    // Optical media are read-only, so an ISO has no root to write.
    let root = ctx.out.join("root.btrfs");
    if !iso {
        let staging = ctx.out.join("root-staging");
        make_root(ctx, &programs.bootfs, &staging, &root, overrides.account.as_ref())?;
    }
    let built = build(
        ctx,
        &ImageSpec {
            kernel: &kernel,
            timeout: ctx.config.boot.timeout,
            cmdline: overrides.cmdline.as_deref().unwrap_or(&ctx.config.boot.cmdline),
            microcode,
            programs: Some(&programs),
            disk_check: overrides.disk_check.as_deref(),
            root: (!iso).then_some(root.as_path()),
            staging: ctx.out.join("image-root"),
            output: ctx.out.join(file_name(iso)),
        },
    );
    let _ = std::fs::remove_file(&root);
    built?;
    Ok(ctx.out.join(file_name(iso)))
}

/// Reads Intel microcode from a file of updates, or from every file in a
/// directory such as `/lib/firmware/intel-ucode`, and checks that it holds
/// at least one valid update.
pub fn read_microcode(ctx: &Ctx, path: &Path) -> Result<Vec<u8>> {
    let mut files = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(path).context(|| format!("reading {}", path.display()))? {
            let entry = entry.context(|| format!("reading {}", path.display()))?.path();
            if entry.is_file() {
                files.push(entry);
            }
        }
        files.sort();
    } else {
        files.push(path.to_path_buf());
    }
    let mut bytes = Vec::new();
    let (mut valid, mut invalid) = (0, 0);
    for file in &files {
        let data =
            std::fs::read(file).context(|| format!("reading microcode {}", file.display()))?;
        for update in radix_microcode::updates(&data) {
            match update {
                Ok(update) => {
                    bytes.extend_from_slice(update.bytes());
                    valid += 1;
                }
                Err(_) => invalid += 1,
            }
        }
    }
    if valid == 0 {
        return Err(Error::new(format!("{} holds no Intel microcode updates", ctx.display(path)))
            .hint(
                "give the raw update files, such as /lib/firmware/intel-ucode from the \
             intel-microcode package, not an initramfs image",
            ));
    }
    step(&format!(
        "Adding {valid} microcode updates from {}{}",
        ctx.display(path),
        if invalid > 0 { format!(" (skipped {invalid} damaged ones)") } else { String::new() }
    ));
    Ok(bytes)
}

/// Fills `@VERSION@`, `@TIMEOUT@`, `@CMDLINE@`, `@MODULES@` and
/// `@ENTRIES@` in the grub.cfg template. `modules` are (path, command
/// line) pairs, and `entries` more menu entries.
pub fn render_grub_cfg(
    template: &str,
    timeout: u32,
    cmdline: &str,
    modules: &[(&str, &str)],
    entries: &str,
) -> Result<String> {
    config::validate_cmdline(cmdline)?;
    let lines = module_lines(modules);
    let rendered = template
        .replace("@VERSION@", VERSION)
        .replace("@TIMEOUT@", &timeout.to_string())
        .replace("@CMDLINE@", cmdline.trim())
        .replace("@MODULES@", &lines)
        .replace("@ENTRIES@", entries);
    if let Some(name) = find_placeholder(&rendered) {
        crate::bail!("image/grub/grub.cfg.in uses unknown placeholder {name}");
    }
    Ok(rendered)
}

/// A menu entry's `module2` lines for `modules`.
fn module_lines(modules: &[(&str, &str)]) -> String {
    let mut lines = String::new();
    for (path, args) in modules {
        let _ = writeln!(lines, "    module2 {path} {args}");
    }
    lines
}

/// Where the disk check's program is in the boot archive.
const DISK_CHECK_PROGRAM: &str = "/system/tests/posix/disktest";

/// Where the POSIX benchmarks are in the boot archive.
const BENCH_PROGRAM: &str = "/system/tests/posix/bench";

/// The boot menu entry of the disk check of the disk with serial number
/// `serial` (ADR 0122), which runs the kernel's benchmarks and then its
/// program under Spark, with no disk mounted, which also runs the POSIX
/// benchmarks (ADR 0124);
/// and Linux's side of the check in the image's `crosscheck` directory:
/// the program built for Linux, linked against the live system's C
/// library so that the image carries none of it, the benchmarks built
/// for Linux as static programs, and `run.sh`.
fn disk_check(
    ctx: &Ctx,
    spec: &ImageSpec<'_>,
    serial: &str,
    modules: &[(&str, &str)],
) -> Result<String> {
    let valid = !serial.is_empty()
        && serial.len() <= 40
        && serial.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !valid {
        crate::bail!(
            "--disk-check takes a disk's serial number, of up to 40 letters, digits, dots, \
             dashes and underscores, not `{serial}`"
        );
    }
    if spec.cmdline.split_whitespace().any(|word| word == "--") {
        crate::bail!(
            "--disk-check runs a program of its own under Spark, so the kernel command line \
             cannot have a `--` part"
        );
    }
    if spec.cmdline.split_whitespace().any(|word| word.starts_with("selftest=")) {
        crate::bail!(
            "--disk-check runs the kernel's benchmarks, so the kernel command line cannot \
             name another self-test"
        );
    }
    step(&format!("Adding the disk check of the disk with serial number {serial}"));
    let dir = spec.staging.join("crosscheck");
    process::create_dir(&dir)?;
    let compiler = crate::ports::find_clang()
        .map(|(clang, _)| clang)
        .or_else(|| process::find_program("cc"))
        .ok_or_else(|| {
            Error::new("no C compiler is installed, for Linux's side of the disk check")
                .hint("run `./ob doctor`")
        })?;
    let mut cmd = Command::new(compiler);
    cmd.args(["-std=c11", "-D_GNU_SOURCE", "-O2", "-Wall", "-o"])
        .arg(dir.join("disktest"))
        .arg(ctx.root.join("userspace/posix/disktest.c"));
    process::run(ctx, &mut cmd)?;
    crate::bench::posix_for_linux(ctx, &dir)?;
    process::copy(&crate::bench::linux_bench_static(ctx)?, &dir.join("linux-bench"))?;
    let template = ctx.root.join("image/crosscheck/run.sh.in");
    let script =
        std::fs::read_to_string(&template).context(|| format!("reading {}", template.display()))?;
    let run = dir.join("run.sh");
    std::fs::write(&run, script.replace("@SERIAL@", serial))
        .context(|| format!("writing {}", run.display()))?;
    let cmdline = spec.cmdline.trim();
    let space = if cmdline.is_empty() { "" } else { " " };
    Ok(format!(
        "\nmenuentry \"Dukovo disk check: erases the disk with serial {serial}\" {{\n    \
         multiboot2 /boot/radix {cmdline}{space}selftest=bench init-after-selftest -- \
         --no-volumes --posix {DISK_CHECK_PROGRAM} --yes-erase {serial} --bench \
         {BENCH_PROGRAM}\n{}    boot\n}}\n",
        module_lines(modules)
    ))
}

/// Returns the first `@NAME@` left in `text`.
fn find_placeholder(text: &str) -> Option<&str> {
    let mut rest = text;
    while let Some(start) = rest.find('@') {
        let after = &rest[start + 1..];
        if let Some(len) = after.find('@') {
            let name = &after[..len];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                return Some(&rest[start..start + len + 2]);
            }
        }
        rest = after;
    }
    None
}

/// Builds an image from `spec`.
pub fn build(ctx: &Ctx, spec: &ImageSpec<'_>) -> Result<()> {
    let mkrescue = process::find_any(&["grub-mkrescue", "grub2-mkrescue"]).ok_or_else(|| {
        Error::new("grub-mkrescue is not installed")
            .hint("run `./ob doctor` to see what is missing")
    })?;
    for (tool, purpose) in [("xorriso", "write the image"), ("mformat", "build the EFI partition")]
    {
        if process::find_program(tool).is_none() {
            return Err(Error::new(format!("{tool} is needed to {purpose} but is not installed"))
                .hint("run `./ob doctor` to see what is missing"));
        }
    }

    step(&format!("Assembling the boot image {}", ctx.display(&spec.output)));
    if spec.staging.exists() {
        std::fs::remove_dir_all(&spec.staging)
            .context(|| format!("removing {}", spec.staging.display()))?;
    }
    kernel::copy_for_image(ctx, spec.kernel, &spec.staging.join("boot/radix"))?;
    let manifest = licenses::install(
        ctx,
        &spec.staging,
        &licenses::Extras {
            mkrescue: &mkrescue,
            microcode: spec.microcode.is_some(),
            programs: spec.programs.map(|programs| &programs.ports),
        },
    )?;
    let mut modules = Vec::new();
    if let Some(programs) = spec.programs {
        // The kernel finds each module by the word on its command line.
        for (from, path, word) in [
            (&programs.usertests, userspace::USERTESTS_PATH, userspace::USERTESTS_WORD),
            (&programs.bootfs, userspace::BOOTFS_PATH, userspace::BOOTFS_WORD),
        ] {
            process::copy(from, &spec.staging.join(path.trim_start_matches('/')))?;
            modules.push((path, word));
        }
    }
    if let Some(microcode) = &spec.microcode {
        let path = spec.staging.join(MICROCODE_PATH.trim_start_matches('/'));
        process::create_dir(path.parent().expect("the microcode path has a parent"))?;
        std::fs::write(&path, microcode).context(|| format!("writing {}", path.display()))?;
        // The kernel recognises the module by this word on its command line.
        modules.push((MICROCODE_PATH, "microcode"));
    }
    let template_path = ctx.root.join("image/grub/grub.cfg.in");
    let template = std::fs::read_to_string(&template_path)
        .context(|| format!("reading {}", template_path.display()))?;
    let entries = match spec.disk_check {
        Some(serial) => disk_check(ctx, spec, serial, &modules)?,
        None => String::new(),
    };
    let grub_cfg = render_grub_cfg(&template, spec.timeout, spec.cmdline, &modules, &entries)?;
    let cfg_path = spec.staging.join("boot/grub/grub.cfg");
    process::create_dir(cfg_path.parent().expect("grub.cfg has a parent"))?;
    std::fs::write(&cfg_path, grub_cfg).context(|| format!("writing {}", cfg_path.display()))?;
    if let Some(parent) = spec.output.parent() {
        process::create_dir(parent)?;
    }

    let mut cmd = Command::new(&mkrescue);
    // The menu uses GRUB's text terminals, so no font, translation or theme
    // is needed; leaving them out keeps the image's GRUB files to GRUB's own
    // code, which the license manifest lists.
    cmd.args(["--fonts=", "--locales=", "--themes="]);
    cmd.arg("-o").arg(&spec.output).arg(&spec.staging);
    // Arguments after `--` go to xorriso. The volume label lets installers
    // and udev rules recognise the medium.
    cmd.args(["--", "-volid", &volume_id()]);
    if let Some(epoch) = source_date_epoch(ctx) {
        cmd.env("SOURCE_DATE_EPOCH", epoch);
    }
    let output = process::output(ctx, &mut cmd)?;
    if ctx.verbose {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    if let Some(root) = spec.root {
        step("Adding the Dukovo root partition");
        append_root(&spec.output, root)?;
    }
    // A copy of the manifest next to the image, for release pages and CI.
    let sidecar = spec.output.with_extension("spdx");
    std::fs::write(&sidecar, manifest).context(|| format!("writing {}", sidecar.display()))?;
    Ok(())
}

/// The size of the Dukovo root partition of an image (ADR 0137).
pub const ROOT_BYTES: u64 = 512 << 20;

/// The Dukovo root partition's GPT type, 90C9A303-3954-4174-8F8B-EE75A92A9EB0
/// (minted by ADR 0137), in its mixed-endian byte order.
pub const ROOT_TYPE: [u8; 16] = [
    0x03, 0xa3, 0xc9, 0x90, 0x54, 0x39, 0x74, 0x41, 0x8f, 0x8b, 0xee, 0x75, 0xa9, 0x2a, 0x9e, 0xb0,
];

/// The Dukovo root partition's GPT name.
const ROOT_NAME: &str = "Dukovo root";

/// The Dukovo root's Btrfs label.
pub const ROOT_LABEL: &str = "dukovo-root";

/// The subvolumes of the Dukovo root, the first read-only: those Spark
/// mounts (spark(8)), and `@snapshots`.
const SUBVOLUMES: [&str; 7] =
    ["@system.a", "@settings", "@state", "@applications", "@users", "@shared", "@snapshots"];

/// Makes the Dukovo root's Btrfs in the file `output`, of [`ROOT_BYTES`],
/// with the pinned `mkfs.btrfs` ([`btrfs::mkfs`]): `@system.a` holds what
/// the boot archive at `bootfs` has in its `system` directory, unpacked
/// into `staging` first, and the other [`SUBVOLUMES`] are empty but for
/// the files of `account`, if there is one. Data is compressed with zstd.
pub fn make_root(
    ctx: &Ctx,
    bootfs: &Path,
    staging: &Path,
    output: &Path,
    account: Option<&Account>,
) -> Result<()> {
    step(&format!("Making the Dukovo root's Btrfs {}", ctx.display(output)));
    if staging.exists() {
        std::fs::remove_dir_all(staging).context(|| format!("removing {}", staging.display()))?;
    }
    // Each directory gets its mode whatever the umask ob runs with: a
    // user's is often 002.
    let mode = |path: &Path, mode: u32| {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
            .context(|| format!("setting the mode of {}", path.display()))
    };
    for name in SUBVOLUMES {
        process::create_dir(&staging.join(name))?;
        mode(&staging.join(name), 0o755)?;
    }
    let bytes = std::fs::read(bootfs).context(|| format!("reading {}", bootfs.display()))?;
    let archive = radix_bootfs::open(&bytes)
        .map_err(|err| Error::new(format!("reading {}: {err:?}", bootfs.display())))?;
    let system = staging.join(SUBVOLUMES[0]);
    for entry in archive.entries() {
        let mut names = entry.path().components();
        if names.next() != Some("system") {
            continue;
        }
        let path = names.fold(system.clone(), |path, name| path.join(name));
        if entry.kind == radix_bootfs::Kind::Directory {
            process::create_dir(&path)?;
            mode(&path, entry.mode)?;
            continue;
        }
        process::create_dir(path.parent().expect("an entry is inside the system"))?;
        if entry.kind == radix_bootfs::Kind::Symlink {
            let target = std::str::from_utf8(entry.data)
                .map_err(|_| Error::new(format!("the link {} is not UTF-8", path.display())))?;
            std::os::unix::fs::symlink(target, &path)
                .context(|| format!("linking {}", path.display()))?;
            continue;
        }
        let mut file =
            std::fs::File::create(&path).context(|| format!("writing {}", path.display()))?;
        file.write_all(entry.data)
            .and_then(|()| {
                use std::os::unix::fs::PermissionsExt as _;
                file.set_permissions(std::fs::Permissions::from_mode(entry.mode & 0o7777))
            })
            .and_then(|()| {
                file.set_modified(
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(entry.mtime),
                )
            })
            .context(|| format!("writing {}", path.display()))?;
    }
    if let Some(account) = account {
        let subvolume = |name: &str| staging.join(name);
        account::add(&system, &subvolume(SUBVOLUMES[1]), &subvolume(SUBVOLUMES[2]), account)?;
    }
    let _ = std::fs::remove_file(output);
    std::fs::File::create(output)
        .and_then(|file| file.set_len(ROOT_BYTES))
        .context(|| format!("making {}", output.display()))?;
    let mut cmd = Command::new(btrfs::mkfs(ctx)?);
    cmd.args(["-q", "-f", "-L", ROOT_LABEL, "--compress", "zstd", "--rootdir"]).arg(staging);
    cmd.args(["--subvol", &format!("ro:{}", SUBVOLUMES[0])]);
    for name in &SUBVOLUMES[1..] {
        cmd.args(["--subvol", &format!("rw:{name}")]);
    }
    let made = process::run(ctx, cmd.arg(output).stdin(std::process::Stdio::null()));
    let _ = std::fs::remove_dir_all(staging);
    made?;
    // mkfs.btrfs gave each file the owner of its staging copy, whoever
    // runs ob; a system's files are root's (ADR 0213).
    btrfs::give_to_root(output).map(drop)
}

/// Puts the Dukovo root partition, holding the filesystem in the file
/// `root`, after the partitions of the GPT disk image at `image`, from
/// the next whole MiB, in the table's first empty entry (ADR 0137). The
/// backup table moves to the new end, both headers are written again,
/// and the protective MBR covers the disk. The filesystem's zeros are
/// left as holes. Returns the partition's first sector.
pub fn append_root(image: &Path, root: &Path) -> Result<u64> {
    let failed = |what: &str| {
        let what = what.to_string();
        move |err: std::io::Error| Error::new(format!("{what} {}: {err}", image.display()))
    };
    let mut disk = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .map_err(failed("opening"))?;
    let mut head = vec![0u8; 2 * SECTOR];
    disk.read_exact(&mut head).map_err(failed("reading"))?;
    let word = |at: usize| u32::from_le_bytes(head[at..at + 4].try_into().expect("4 bytes"));
    let long = |at: usize| u64::from_le_bytes(head[at..at + 8].try_into().expect("8 bytes"));
    if &head[SECTOR..SECTOR + 8] != b"EFI PART"
        || word(SECTOR + 12) != 92
        || word(SECTOR + 84) != 128
    {
        crate::bail!("{} has no GPT of 128-byte entries", image.display());
    }
    let (old_backup, entries_at) = (long(SECTOR + 32), long(SECTOR + 72));
    let count = word(SECTOR + 80) as usize;
    let entry_sectors = (count * 128).div_ceil(SECTOR) as u64;
    let mut entries = vec![0u8; entry_sectors as usize * SECTOR];
    disk.seek(SeekFrom::Start(entries_at * SECTOR as u64))
        .and_then(|_| disk.read_exact(&mut entries))
        .map_err(failed("reading the GPT entries of"))?;
    let entry = |i: usize| &entries[i * 128..(i + 1) * 128];
    let free = (0..count)
        .find(|&i| entry(i)[..16] == [0; 16])
        .ok_or_else(|| Error::new(format!("the GPT of {} has no empty entry", image.display())))?;
    let used = (0..count)
        .filter(|&i| entry(i)[..16] != [0; 16])
        .map(|i| u64::from_le_bytes(entry(i)[40..48].try_into().expect("8 bytes")))
        .max()
        .unwrap_or(0)
        .max(old_backup);
    let size = std::fs::metadata(root).map_err(failed("reading the root for"))?.len();
    let first = (used + 1).div_ceil(2048) * 2048;
    let last = first + size / SECTOR as u64 - 1;
    let (backup_entries, backup) = (last + 1, last + 1 + entry_sectors);

    let mut unique = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut unique))
        .map_err(failed("making a GUID for"))?;
    // A random GUID: version 4, variant 1.
    unique[7] = unique[7] & 0x0f | 0x40;
    unique[8] = unique[8] & 0x3f | 0x80;
    let new = &mut entries[free * 128..(free + 1) * 128];
    new.fill(0);
    new[..16].copy_from_slice(&ROOT_TYPE);
    new[16..32].copy_from_slice(&unique);
    new[32..40].copy_from_slice(&first.to_le_bytes());
    new[40..48].copy_from_slice(&last.to_le_bytes());
    for (i, c) in ROOT_NAME.encode_utf16().enumerate() {
        new[56 + 2 * i..58 + 2 * i].copy_from_slice(&c.to_le_bytes());
    }
    let table = crc32(&entries[..count * 128]);
    let header = |at: u64, alternate: u64, entries_at: u64| {
        let mut h = head[SECTOR..2 * SECTOR].to_vec();
        h[16..20].fill(0);
        h[24..32].copy_from_slice(&at.to_le_bytes());
        h[32..40].copy_from_slice(&alternate.to_le_bytes());
        h[48..56].copy_from_slice(&last.to_le_bytes());
        h[72..80].copy_from_slice(&entries_at.to_le_bytes());
        h[88..92].copy_from_slice(&table.to_le_bytes());
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };
    // The protective MBR's one partition, from sector 1 to the end, with
    // the CHS of an end past what CHS can say.
    let mut mbr = head[..SECTOR].to_vec();
    if mbr[446 + 4] != 0xee {
        crate::bail!("{} has no protective MBR", image.display());
    }
    mbr[446 + 5..446 + 8].fill(0xff);
    let sectors = u32::try_from(backup).unwrap_or(u32::MAX);
    mbr[446 + 12..446 + 16].copy_from_slice(&sectors.to_le_bytes());

    let mut from = std::fs::File::open(root).map_err(failed("opening the root for"))?;
    copy_sparse(&mut from, 0, &mut disk, first * SECTOR as u64, size)
        .map_err(failed("writing the root to"))?;
    let at = |sector: u64| SeekFrom::Start(sector * SECTOR as u64);
    let zeros = vec![0u8; (entry_sectors as usize + 1) * SECTOR];
    (|| {
        // The old backup table, in the gap before the partition.
        disk.seek(at(old_backup - entry_sectors))?;
        disk.write_all(&zeros)?;
        disk.seek(at(backup_entries))?;
        disk.write_all(&entries)?;
        disk.write_all(&header(backup, 1, backup_entries))?;
        disk.seek(at(0))?;
        disk.write_all(&mbr)?;
        disk.write_all(&header(1, backup, entries_at))?;
        disk.seek(at(entries_at))?;
        disk.write_all(&entries)?;
        disk.set_len((backup + 1) * SECTOR as u64)?;
        disk.sync_all()
    })()
    .map_err(failed("writing the partition table of"))?;
    Ok(first)
}

/// Copies `len` bytes from `from` at `from_at` to `to` at `to_at`,
/// skipping the pieces that are all zeros, which a file just made or
/// extended reads as anyway.
pub fn copy_sparse(
    from: &mut std::fs::File,
    from_at: u64,
    to: &mut std::fs::File,
    to_at: u64,
    len: u64,
) -> std::io::Result<()> {
    let mut piece = vec![0u8; 1 << 20];
    let mut done = 0;
    while done < len {
        let n = piece.len().min(usize::try_from(len - done).unwrap_or(usize::MAX));
        from.seek(SeekFrom::Start(from_at + done))?;
        from.read_exact(&mut piece[..n])?;
        if piece[..n].iter().any(|&b| b != 0) {
            to.seek(SeekFrom::Start(to_at + done))?;
            to.write_all(&piece[..n])?;
        }
        done += n as u64;
    }
    Ok(())
}

/// The CRC-32 of `bytes` that GPT uses (IEEE 802.3, reflected).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// The size of a sector, which the GPT of grub-mkrescue's image counts in.
const SECTOR: usize = 512;

/// ISO 9660 volume label, such as `DUKOVO_0_0_1`.
pub fn volume_id() -> String {
    format!("DUKOVO_{}", VERSION.replace(['.', '-'], "_")).to_ascii_uppercase()
}

/// Commit time of HEAD, which xorriso uses for the ISO timestamps instead of
/// the clock. An existing `SOURCE_DATE_EPOCH` wins. Images are not yet
/// bit-for-bit reproducible: grub-mkrescue adds random UUIDs.
pub fn source_date_epoch(ctx: &Ctx) -> Option<String> {
    if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH") {
        return Some(epoch);
    }
    let output = Command::new("git")
        .args(["log", "-1", "--format=%ct"])
        .current_dir(&ctx.root)
        .output()
        .ok()?;
    let epoch = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && !epoch.is_empty()).then_some(epoch)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = include_str!("../../../image/grub/grub.cfg.in");

    #[test]
    fn renders_the_repository_template() {
        let cfg = render_grub_cfg(TEMPLATE, 0, "qemu-exit", &[], "").unwrap();
        assert!(cfg.contains("set timeout=0"));
        assert!(cfg.contains("multiboot2 /boot/radix qemu-exit\n    boot\n"));
        assert!(cfg.contains(&format!("Dukovo Server {VERSION}: startup trouble")));
        assert!(cfg.contains("multiboot2 /boot/radix startup-trouble qemu-exit\n    boot\n"));
        assert!(cfg.contains(&format!("Dukovo Server {VERSION} (Radix)")));
        assert!(cfg.contains("set gfxpayload=keep"));
        assert!(!cfg.contains("@VERSION@"));
        assert!(!cfg.contains("module2"));
    }

    #[test]
    fn renders_modules() {
        let cfg = render_grub_cfg(TEMPLATE, 0, "", &[(MICROCODE_PATH, "microcode")], "").unwrap();
        assert!(cfg.contains(&format!("    module2 {MICROCODE_PATH} microcode\n    boot\n")));
    }

    #[test]
    fn rejects_unknown_placeholders_and_bad_cmdlines() {
        assert!(render_grub_cfg("set x=@NOPE@", 0, "", &[], "").is_err());
        assert!(render_grub_cfg(TEMPLATE, 0, "a; reboot", &[], "").is_err());
    }

    #[test]
    fn names_follow_the_version() {
        assert_eq!(file_name(false), format!("dukovo-{VERSION}-x86_64.img"));
        assert_eq!(file_name(true), format!("dukovo-{VERSION}-x86_64.iso"));
        let label = volume_id();
        assert!(label.len() <= 32);
        assert!(label.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'));
    }
}
