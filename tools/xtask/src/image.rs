//! Building the hybrid boot image with grub-mkrescue.
//!
//! The image carries GRUB for both legacy BIOS (El Torito plus an MBR boot
//! record) and UEFI (an EFI system partition), so the same file boots from
//! a USB stick, a hard disk or optical media. Phase 14 replaces it with the
//! GPT disk image assembled by OrthrOS's own rdisk and mkfs tools.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, Profile};
use crate::process::{self, step};
use crate::{Context, Ctx, Error, Result, VERSION, kernel};

/// What goes into an image and where it is written.
pub struct ImageSpec<'a> {
    /// Kernel to boot.
    pub kernel: &'a Path,
    /// GRUB menu timeout in seconds.
    pub timeout: u32,
    /// Kernel command line.
    pub cmdline: &'a str,
    /// Staging directory, recreated on every build.
    pub staging: PathBuf,
    /// Output file.
    pub output: PathBuf,
}

/// File name of a release image.
pub fn file_name(iso: bool) -> String {
    let extension = if iso { "iso" } else { "img" };
    format!("orthros-{VERSION}-x86_64.{extension}")
}

/// Builds the kernel and the release image described by orthros.toml.
pub fn build_release(ctx: &Ctx, profile: Profile, iso: bool) -> Result<PathBuf> {
    let kernel = kernel::build(ctx, profile)?;
    build(
        ctx,
        &ImageSpec {
            kernel: &kernel,
            timeout: ctx.config.boot.timeout,
            cmdline: &ctx.config.boot.cmdline,
            staging: ctx.out.join("image-root"),
            output: ctx.out.join(file_name(iso)),
        },
    )?;
    Ok(ctx.out.join(file_name(iso)))
}

/// Fills `@VERSION@`, `@TIMEOUT@` and `@CMDLINE@` in the grub.cfg template.
pub fn render_grub_cfg(template: &str, timeout: u32, cmdline: &str) -> Result<String> {
    config::validate_cmdline(cmdline)?;
    let rendered = template
        .replace("@VERSION@", VERSION)
        .replace("@TIMEOUT@", &timeout.to_string())
        .replace("@CMDLINE@", cmdline.trim());
    if let Some(name) = find_placeholder(&rendered) {
        crate::bail!("image/grub/grub.cfg.in uses unknown placeholder {name}");
    }
    Ok(rendered)
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
    let template_path = ctx.root.join("image/grub/grub.cfg.in");
    let template = std::fs::read_to_string(&template_path)
        .context(|| format!("reading {}", template_path.display()))?;
    let grub_cfg = render_grub_cfg(&template, spec.timeout, spec.cmdline)?;
    let cfg_path = spec.staging.join("boot/grub/grub.cfg");
    process::create_dir(cfg_path.parent().expect("grub.cfg has a parent"))?;
    std::fs::write(&cfg_path, grub_cfg).context(|| format!("writing {}", cfg_path.display()))?;
    if let Some(parent) = spec.output.parent() {
        process::create_dir(parent)?;
    }

    let mut cmd = Command::new(mkrescue);
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
    Ok(())
}

/// ISO 9660 volume label, such as `ORTHROS_0_0_1`.
pub fn volume_id() -> String {
    format!("ORTHROS_{}", VERSION.replace(['.', '-'], "_")).to_ascii_uppercase()
}

/// Commit time of HEAD, which xorriso uses for the ISO timestamps instead of
/// the clock. An existing `SOURCE_DATE_EPOCH` wins. Images are not yet
/// bit-for-bit reproducible: grub-mkrescue adds random UUIDs.
fn source_date_epoch(ctx: &Ctx) -> Option<String> {
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
        let cfg = render_grub_cfg(TEMPLATE, 0, "qemu-exit").unwrap();
        assert!(cfg.contains("set timeout=0"));
        assert!(cfg.contains("multiboot2 /boot/radix qemu-exit\n"));
        assert!(cfg.contains(&format!("OrthrOS {VERSION} (Radix)")));
        assert!(!cfg.contains("@VERSION@"));
    }

    #[test]
    fn rejects_unknown_placeholders_and_bad_cmdlines() {
        assert!(render_grub_cfg("set x=@NOPE@", 0, "").is_err());
        assert!(render_grub_cfg(TEMPLATE, 0, "a; reboot").is_err());
    }

    #[test]
    fn names_follow_the_version() {
        assert_eq!(file_name(false), format!("orthros-{VERSION}-x86_64.img"));
        assert_eq!(file_name(true), format!("orthros-{VERSION}-x86_64.iso"));
        let label = volume_id();
        assert!(label.len() <= 32);
        assert!(label.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'));
    }
}
