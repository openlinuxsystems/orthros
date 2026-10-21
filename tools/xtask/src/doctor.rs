// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob setup` and `ob doctor`: preparing and checking the build host.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::process::{self, step};
use crate::{Ctx, Error, Result, kernel, qemu, tla};

/// mdBook release the handbook is built and tested with.
pub const MDBOOK_VERSION: &str = "0.4.52";

/// Kani release the proof harnesses are checked with (ADR 0019).
pub const KANI_VERSION: &str = "0.68.0";

/// The installed Kani's version, if any.
pub fn kani_version() -> Option<String> {
    process::find_program("cargo-kani")?;
    let out = Command::new("cargo-kani").arg("--version").output().ok()?;
    // "Kani Rust Verifier 0.68.0 (cargo plugin)"
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_string)
}

/// How to install the pinned Kani.
pub fn kani_install() -> String {
    format!(
        "install it with `cargo install --locked kani-verifier --version {KANI_VERSION}`, then \
         run `cargo kani setup`"
    )
}

/// Package names on the distributions `ob doctor` knows.
#[derive(Clone, Copy)]
struct Packages {
    apt: Option<&'static str>,
    dnf: Option<&'static str>,
    pacman: Option<&'static str>,
}

const fn all(name: &'static str) -> Packages {
    Packages { apt: Some(name), dnf: Some(name), pacman: Some(name) }
}

/// One thing the build needs from the host.
struct Requirement {
    name: &'static str,
    purpose: &'static str,
    found: fn(&Ctx) -> Option<String>,
    packages: Packages,
    /// Install command for when no distribution package exists.
    fallback: Option<String>,
}

/// Package managers, in the order the report prints them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Distro {
    Apt,
    Dnf,
    Pacman,
}

impl Distro {
    fn label(self) -> &'static str {
        match self {
            Distro::Apt => "Debian or Ubuntu",
            Distro::Dnf => "Fedora",
            Distro::Pacman => "Arch Linux",
        }
    }

    fn install_command(self, packages: &[&str]) -> String {
        let list = packages.join(" ");
        match self {
            Distro::Apt => format!("sudo apt install {list}"),
            Distro::Dnf => format!("sudo dnf install {list}"),
            Distro::Pacman => format!("sudo pacman -S --needed {list}"),
        }
    }

    fn package(self, packages: Packages) -> Option<&'static str> {
        match self {
            Distro::Apt => packages.apt,
            Distro::Dnf => packages.dnf,
            Distro::Pacman => packages.pacman,
        }
    }
}

/// Reads /etc/os-release to pick the package manager.
fn detect_distro() -> Option<Distro> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    distro_from_os_release(&text)
}

fn distro_from_os_release(text: &str) -> Option<Distro> {
    let mut ids = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("ID=").or_else(|| line.strip_prefix("ID_LIKE=")) {
            ids.extend(value.trim_matches('"').split_whitespace().map(str::to_string));
        }
    }
    ids.iter().find_map(|id| match id.as_str() {
        "debian" | "ubuntu" => Some(Distro::Apt),
        "fedora" | "rhel" | "centos" => Some(Distro::Dnf),
        "arch" | "archlinux" => Some(Distro::Pacman),
        _ => None,
    })
}

fn program_version(program: &str, flag: &str) -> Option<String> {
    let path = process::find_program(program)?;
    let version = Command::new(&path)
        .arg(flag)
        .output()
        .ok()
        .map(|out| {
            let text = if out.stdout.is_empty() { out.stderr } else { out.stdout };
            String::from_utf8_lossy(&text).lines().next().unwrap_or_default().trim().to_string()
        })
        .filter(|line| !line.is_empty());
    Some(version.unwrap_or_else(|| path.display().to_string()))
}

fn toolchain_channel(ctx: &Ctx) -> Result<String> {
    let path = ctx.root.join("rust-toolchain.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| Error::new(format!("reading {}: {err}", path.display())))?;
    let table: toml::Table =
        text.parse().map_err(|err| Error::new(format!("{}: {err}", path.display())))?;
    table
        .get("toolchain")
        .and_then(|t| t.get("channel"))
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .ok_or_else(|| Error::new(format!("{} has no toolchain.channel", path.display())))
}

fn pinned_toolchain(ctx: &Ctx) -> Option<String> {
    let channel = toolchain_channel(ctx).ok()?;
    let out = Command::new("rustup").args(["toolchain", "list"]).output().ok()?;
    let listed = String::from_utf8_lossy(&out.stdout).lines().any(|line| {
        line.starts_with(&format!("{channel}-")) || line.split_whitespace().next() == Some(&channel)
    });
    listed.then_some(channel)
}

/// The pinned toolchain's sysroot.
pub fn sysroot(ctx: &Ctx) -> Option<PathBuf> {
    // Ask the pinned toolchain directly; `rustup run` never installs anything.
    let channel = pinned_toolchain(ctx)?;
    let out = Command::new("rustup")
        .args(["run", &channel, "rustc", "--print", "sysroot"])
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    Some(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

fn rust_sources(ctx: &Ctx) -> Option<String> {
    let core = sysroot(ctx)?.join("lib/rustlib/src/rust/library/core");
    core.is_dir().then(|| "rust-src".to_string())
}

fn aarch64_target(ctx: &Ctx) -> Option<String> {
    let dir = sysroot(ctx)?.join("lib/rustlib").join(kernel::AARCH64_TARGET);
    dir.is_dir().then(|| kernel::AARCH64_TARGET.to_string())
}

fn grub_platform(dir: &'static str) -> Option<String> {
    ["/usr/lib/grub", "/usr/share/grub", "/usr/lib64/grub"]
        .iter()
        .map(|base| Path::new(base).join(dir))
        .find(|path| path.join("modinfo.sh").is_file() || path.join("kernel.img").is_file())
        .map(|path| path.display().to_string())
}

fn requirements() -> Vec<Requirement> {
    vec![
        Requirement {
            name: "rustup",
            purpose: "installs the pinned Rust toolchain",
            found: |_| program_version("rustup", "--version"),
            packages: all("rustup"),
            fallback: Some("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh".into()),
        },
        Requirement {
            name: "Rust toolchain",
            purpose: "the nightly pinned in rust-toolchain.toml",
            found: pinned_toolchain,
            packages: Packages { apt: None, dnf: None, pacman: None },
            fallback: Some("./ob setup".into()),
        },
        Requirement {
            name: "Rust sources",
            purpose: "rust-src, from which the kernel builds `core` for its own target",
            found: rust_sources,
            packages: Packages { apt: None, dnf: None, pacman: None },
            fallback: Some("./ob setup".into()),
        },
        Requirement {
            name: "AArch64 target",
            purpose: "Rust's aarch64-unknown-none library, for checking the HAL stubs",
            found: aarch64_target,
            packages: Packages { apt: None, dnf: None, pacman: None },
            fallback: Some("./ob setup".into()),
        },
        Requirement {
            name: "QEMU",
            purpose: "runs and tests the image",
            found: |_| program_version("qemu-system-x86_64", "--version"),
            packages: all("qemu-system-x86"),
            fallback: None,
        },
        Requirement {
            name: "OVMF",
            purpose: "UEFI firmware for QEMU",
            found: |_| qemu::find_ovmf().map(|(code, _)| code.display().to_string()),
            packages: Packages {
                apt: Some("ovmf"),
                dnf: Some("edk2-ovmf"),
                pacman: Some("edk2-ovmf"),
            },
            fallback: None,
        },
        Requirement {
            name: "grub-mkrescue",
            purpose: "builds the hybrid boot image",
            found: |_| {
                program_version("grub-mkrescue", "--version")
                    .or_else(|| program_version("grub2-mkrescue", "--version"))
            },
            packages: Packages {
                apt: Some("grub-common"),
                dnf: Some("grub2-tools-extra"),
                pacman: Some("grub"),
            },
            fallback: None,
        },
        Requirement {
            name: "GRUB for BIOS",
            purpose: "the i386-pc boot code in the image",
            found: |_| grub_platform("i386-pc"),
            packages: Packages {
                apt: Some("grub-pc-bin"),
                dnf: Some("grub2-pc-modules"),
                pacman: Some("grub"),
            },
            fallback: None,
        },
        Requirement {
            name: "GRUB for UEFI",
            purpose: "the x86_64-efi boot code in the image",
            found: |_| grub_platform("x86_64-efi"),
            packages: Packages {
                apt: Some("grub-efi-amd64-bin"),
                dnf: Some("grub2-efi-x64-modules"),
                pacman: Some("grub"),
            },
            fallback: None,
        },
        Requirement {
            name: "xorriso",
            purpose: "writes the ISO 9660 part of the image",
            found: |_| program_version("xorriso", "-version"),
            packages: Packages {
                apt: Some("xorriso"),
                dnf: Some("xorriso"),
                pacman: Some("libisoburn"),
            },
            fallback: None,
        },
        Requirement {
            name: "mtools",
            purpose: "builds the EFI system partition",
            found: |_| program_version("mformat", "--version"),
            packages: all("mtools"),
            fallback: None,
        },
        Requirement {
            name: "scdoc",
            purpose: "compiles the man pages",
            found: |_| process::find_program("scdoc").map(|p| p.display().to_string()),
            packages: all("scdoc"),
            fallback: None,
        },
        Requirement {
            name: "mandoc",
            purpose: "checks the man pages",
            found: |_| process::find_program("mandoc").map(|p| p.display().to_string()),
            packages: all("mandoc"),
            fallback: None,
        },
        Requirement {
            name: "mdBook",
            purpose: "builds the handbook",
            found: |_| program_version("mdbook", "--version"),
            packages: Packages { apt: None, dnf: Some("mdbook"), pacman: Some("mdbook") },
            fallback: Some(format!("cargo install mdbook --version {MDBOOK_VERSION} --locked")),
        },
    ]
}

/// `ob doctor`.
pub fn doctor(ctx: &Ctx) -> Result<()> {
    if std::env::consts::OS != "linux" || std::env::consts::ARCH != "x86_64" {
        return Err(Error::new("ob builds OrthrOS on x86-64 Linux hosts")
            .hint("on macOS or Windows, use `./ob --container <command>` with Podman or Docker"));
    }
    println!("Checking the build host for OrthrOS {}\n", crate::VERSION);
    let mut missing = Vec::new();
    for req in requirements() {
        match (req.found)(ctx) {
            Some(detail) => println!("  ok       {:<15} {detail}", req.name),
            None => {
                println!("  MISSING  {:<15} {}", req.name, req.purpose);
                missing.push(req);
            }
        }
    }
    match program_version("gdb", "--version") {
        Some(version) => println!("  ok       {:<15} {version}", "GDB"),
        None => println!(
            "  note     {:<15} not installed; only `./ob run --gdb` needs it (package gdb)",
            "GDB"
        ),
    }
    match kani_version() {
        Some(version) if version == KANI_VERSION => {
            println!("  ok       {:<15} {version}", "Kani");
        }
        Some(version) => println!(
            "  note     {:<15} {version} installed; `./ob test kani` needs {KANI_VERSION}",
            "Kani"
        ),
        None => println!(
            "  note     {:<15} not installed; only `./ob test kani` needs it (cargo install \
             --locked kani-verifier --version {KANI_VERSION})",
            "Kani"
        ),
    }
    match program_version("java", "--version") {
        Some(version) => println!("  ok       {:<15} {version}", "Java"),
        None => println!(
            "  note     {:<15} not installed; only `./ob test tla` needs it (package \
             default-jre-headless; ob downloads the TLA+ tools {} itself)",
            "Java",
            tla::TLA_VERSION
        ),
    }
    if qemu::kvm_available() {
        println!("  ok       {:<15} /dev/kvm is usable", "KVM");
    } else {
        println!("  note     {:<15} /dev/kvm is not usable; QEMU will run, but slowly", "KVM");
    }
    println!();

    if missing.is_empty() {
        println!("Everything is installed. Next: ./ob run");
        return Ok(());
    }
    print_fixes(&missing, detect_distro());
    Err(Error::new(format!("{} requirement(s) missing", missing.len()))
        .hint("install them with the commands above, then run ./ob doctor again"))
}

fn print_fixes(missing: &[Requirement], distro: Option<Distro>) {
    let distros = match distro {
        Some(distro) => vec![distro],
        None => vec![Distro::Apt, Distro::Dnf, Distro::Pacman],
    };
    for distro in distros {
        let mut packages: Vec<&str> = Vec::new();
        for req in missing {
            if let Some(name) = distro.package(req.packages)
                && !packages.contains(&name)
            {
                packages.push(name);
            }
        }
        if !packages.is_empty() {
            println!("On {}, install the missing packages with:", distro.label());
            println!("  {}\n", distro.install_command(&packages));
        }
    }
    let mut extra: Vec<&str> = Vec::new();
    for req in missing {
        let packaged = distro.is_some_and(|d| d.package(req.packages).is_some());
        if let (false, Some(fallback)) = (packaged, req.fallback.as_deref())
            && !extra.contains(&fallback)
        {
            extra.push(fallback);
        }
    }
    if !extra.is_empty() {
        println!("Then run:");
        for command in extra {
            println!("  {command}");
        }
        println!();
    }
}

/// `ob setup`.
pub fn setup(ctx: &Ctx) -> Result<()> {
    if process::find_program("rustup").is_none() {
        return Err(Error::new("rustup is not installed").hint(
            "install rustup from your distribution or https://rustup.rs, then run ./ob setup again",
        ));
    }
    let channel = toolchain_channel(ctx)?;
    step(&format!("Installing the pinned Rust toolchain {channel}"));
    let mut cmd = Command::new("rustup");
    cmd.args(["toolchain", "install", &channel, "--profile", "minimal"])
        .args(["--component", "rustfmt,clippy,rust-src,llvm-tools"])
        .args(["--target", kernel::AARCH64_TARGET])
        .current_dir(&ctx.root);
    process::run(ctx, &mut cmd)?;
    step("Checking host tools");
    doctor(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_distributions() {
        let ubuntu = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n";
        assert_eq!(distro_from_os_release(ubuntu), Some(Distro::Apt));
        assert_eq!(distro_from_os_release("ID=fedora\n"), Some(Distro::Dnf));
        assert_eq!(distro_from_os_release("ID=endeavouros\nID_LIKE=arch\n"), Some(Distro::Pacman));
        assert_eq!(distro_from_os_release("ID=\"opensuse-tumbleweed\"\n"), None);
    }

    #[test]
    fn install_commands() {
        assert_eq!(
            Distro::Apt.install_command(&["ovmf", "mtools"]),
            "sudo apt install ovmf mtools"
        );
        assert_eq!(Distro::Pacman.install_command(&["grub"]), "sudo pacman -S --needed grub");
    }

    #[test]
    fn container_and_ci_pin_the_same_mdbook() {
        let pin = format!("MDBOOK_VERSION={MDBOOK_VERSION}");
        let containerfile = include_str!("../../container/Containerfile");
        assert!(containerfile.contains(&pin), "tools/container/Containerfile pins another mdBook");
        let ci = include_str!("../../../.github/workflows/ci.yml").replace(": ", "=");
        assert!(ci.contains(&pin), ".github/workflows/ci.yml pins another mdBook");
    }

    #[test]
    fn ci_pins_the_same_kani() {
        let ci = include_str!("../../../.github/workflows/ci.yml").replace(": ", "=");
        let pin = format!("KANI_VERSION={KANI_VERSION}");
        assert!(ci.contains(&pin), ".github/workflows/ci.yml pins another Kani");
    }
}
