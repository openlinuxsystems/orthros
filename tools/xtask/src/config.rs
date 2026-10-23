// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `dukovo.toml`, the build configuration. Documented in dukovo.toml(5).

use std::fmt;
use std::path::Path;

use serde::Deserialize;

use crate::{Error, Result};

/// Every key `dukovo.toml` accepts. `ob docs --check` verifies that
/// dukovo.toml(5) documents each one.
pub const KEYS: &[&str] = &[
    "build.arch",
    "build.profile",
    "boot.timeout",
    "boot.cmdline",
    "image.microcode",
    "run.firmware",
    "run.media",
    "run.memory",
    "run.cpus",
];

/// The whole file.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `[build]`
    pub build: Build,
    /// `[boot]`
    pub boot: Boot,
    /// `[image]`
    pub image: Image,
    /// `[run]`
    pub run: Run,
}

/// `[build]`: what to compile.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Build {
    /// Target architecture.
    pub arch: Arch,
    /// Cargo profile.
    pub profile: Profile,
}

/// `[boot]`: how GRUB starts the kernel.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Boot {
    /// Seconds the GRUB menu waits before booting the default entry.
    pub timeout: u32,
    /// Extra kernel command-line flags.
    pub cmdline: String,
}

impl Default for Boot {
    fn default() -> Self {
        Self { timeout: 3, cmdline: String::new() }
    }
}

/// `[image]`: what goes into the boot image besides the kernel.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Image {
    /// Intel microcode to load at boot: a file of updates, or a directory
    /// such as `/lib/firmware/intel-ucode` whose files are all included.
    /// Empty for none.
    pub microcode: String,
}

/// `[run]`: the virtual machine `ob run` starts.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Run {
    /// Firmware QEMU boots through.
    pub firmware: Firmware,
    /// How the image is attached.
    pub media: Media,
    /// Guest memory, in QEMU's notation (`512M`, `2G`).
    pub memory: String,
    /// Virtual CPUs.
    pub cpus: u32,
}

impl Default for Run {
    fn default() -> Self {
        Self { firmware: Firmware::Uefi, media: Media::Usb, memory: "1G".into(), cpus: 4 }
    }
}

/// Target architecture.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub enum Arch {
    /// 64-bit x86, the only architecture before Phase 34.
    #[default]
    #[serde(rename = "x86_64")]
    X86_64,
}

/// Cargo profile.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Unoptimised, fast to compile.
    Debug,
    /// Optimised.
    #[default]
    Release,
}

impl Profile {
    /// Cargo's output directory name for this profile.
    pub fn dir(self) -> &'static str {
        match self {
            Profile::Debug => "debug",
            Profile::Release => "release",
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.dir())
    }
}

/// Firmware a virtual machine boots through.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Firmware {
    /// OVMF.
    Uefi,
    /// SeaBIOS.
    Bios,
}

impl fmt::Display for Firmware {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Firmware::Uefi => "UEFI",
            Firmware::Bios => "BIOS",
        })
    }
}

/// How a virtual machine sees the boot image.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Media {
    /// A USB stick on an xHCI controller, like the real install medium.
    Usb,
    /// A CD-ROM on the SATA controller.
    Cdrom,
    /// A SATA hard disk.
    Disk,
}

impl Media {
    /// Parses the `--media` option.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "usb" => Ok(Media::Usb),
            "cdrom" => Ok(Media::Cdrom),
            "disk" => Ok(Media::Disk),
            _ => Err(Error::new(format!("unknown media `{value}`; use usb, cdrom or disk"))),
        }
    }
}

impl fmt::Display for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Media::Usb => "usb",
            Media::Cdrom => "cdrom",
            Media::Disk => "disk",
        })
    }
}

impl Config {
    /// Reads and validates `path`. A missing file means all defaults.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(Error::new(format!("reading {}: {err}", path.display()))),
        };
        Self::parse(&text).map_err(|err| {
            Error::new(format!("{}: {err}", path.display()))
                .hint("the accepted keys are listed in dukovo.toml(5)")
        })
    }

    /// Parses and validates the text of `dukovo.toml`.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text).map_err(|err| Error::new(err.message()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        validate_cmdline(&self.boot.cmdline)?;
        validate_memory(&self.run.memory)?;
        if !(1..=64).contains(&self.run.cpus) {
            crate::bail!("run.cpus must be between 1 and 64, not {}", self.run.cpus);
        }
        if self.boot.timeout > 600 {
            crate::bail!("boot.timeout must be at most 600 seconds, not {}", self.boot.timeout);
        }
        Ok(())
    }
}

/// Kernel flags end up in grub.cfg, so only characters GRUB's parser treats
/// literally are allowed.
pub fn validate_cmdline(cmdline: &str) -> Result<()> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || " ._=,:/+-".contains(c);
    match cmdline.chars().find(|&c| !allowed(c)) {
        Some(c) => crate::bail!(
            "boot.cmdline may only contain letters, digits, spaces and . _ = , : / + -; found {c:?}"
        ),
        None => Ok(()),
    }
}

fn validate_memory(memory: &str) -> Result<()> {
    let (digits, unit) = memory.split_at(memory.len().saturating_sub(1));
    let valid = matches!(unit, "M" | "G")
        && !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && digits.parse::<u32>().is_ok_and(|n| n > 0);
    if !valid {
        crate::bail!("run.memory must look like 512M or 2G, not `{memory}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_config_parses() {
        let config = Config::parse(include_str!("../../../dukovo.toml")).unwrap();
        assert_eq!(config.build.arch, Arch::X86_64);
    }

    #[test]
    fn empty_file_gives_defaults() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.build.profile, Profile::Release);
        assert_eq!(config.boot.timeout, 3);
        assert_eq!(config.run.firmware, Firmware::Uefi);
        assert_eq!(config.run.media, Media::Usb);
        assert_eq!(config.run.memory, "1G");
        assert_eq!(config.run.cpus, 4);
        assert!(config.image.microcode.is_empty());
    }

    #[test]
    fn rejects_unknown_keys_and_values() {
        assert!(Config::parse("[build]\nprofle = \"debug\"").is_err());
        assert!(Config::parse("[build]\nprofile = \"fast\"").is_err());
        assert!(Config::parse("[build]\narch = \"riscv64\"").is_err());
        assert!(Config::parse("[run]\nmemory = \"lots\"").is_err());
        assert!(Config::parse("[run]\ncpus = 0").is_err());
    }

    #[test]
    fn cmdline_rejects_grub_syntax() {
        validate_cmdline("qemu-exit log=debug").unwrap();
        assert!(validate_cmdline("x; reboot").is_err());
        assert!(validate_cmdline("$root").is_err());
        assert!(validate_cmdline("\"quoted\"").is_err());
    }

    #[test]
    fn keys_cover_the_repository_config() {
        let table: toml::Table = include_str!("../../../dukovo.toml").parse().unwrap();
        for (section, values) in &table {
            for key in values.as_table().unwrap().keys() {
                let dotted = format!("{section}.{key}");
                assert!(KEYS.contains(&dotted.as_str()), "{dotted} is missing from KEYS");
            }
        }
    }
}
