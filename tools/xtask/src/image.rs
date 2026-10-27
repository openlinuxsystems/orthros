// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Building the hybrid boot image with grub-mkrescue.
//!
//! The image carries GRUB for both legacy BIOS (El Torito plus an MBR boot
//! record) and UEFI (an EFI system partition), so the same file boots from
//! a USB stick, a hard disk or optical media. Phase 14 replaces it with the
//! GPT disk image assembled by Dukovo's own rdisk and mkfs tools.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, Profile};
use crate::process::{self, step};
use crate::{Context, Ctx, Error, Result, VERSION, kernel, licenses, userspace};

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
    build(
        ctx,
        &ImageSpec {
            kernel: &kernel,
            timeout: ctx.config.boot.timeout,
            cmdline: overrides.cmdline.as_deref().unwrap_or(&ctx.config.boot.cmdline),
            microcode,
            programs: Some(&programs),
            disk_check: overrides.disk_check.as_deref(),
            staging: ctx.out.join("image-root"),
            output: ctx.out.join(file_name(iso)),
        },
    )?;
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

/// The boot menu entry of the disk check of the disk with serial number
/// `serial` (ADR 0122), which runs its program under Spark, and Linux's
/// side of the check in the image's `crosscheck` directory: the program
/// built for Linux, linked against the live system's C library so that
/// the image carries none of it, and `run.sh`.
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
         multiboot2 /boot/radix {cmdline}{space}-- --posix {DISK_CHECK_PROGRAM} --yes-erase \
         {serial}\n{}    boot\n}}\n",
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
    process::copy(spec.kernel, &spec.staging.join("boot/radix"))?;
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
    // A copy of the manifest next to the image, for release pages and CI.
    let sidecar = spec.output.with_extension("spdx");
    std::fs::write(&sidecar, manifest).context(|| format!("writing {}", sidecar.display()))?;
    Ok(())
}

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
