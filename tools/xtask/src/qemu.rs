//! Starting QEMU, for `ob run` and the boot tests.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Firmware, Media};
use crate::process::{self, step};
use crate::{Args, Ctx, Error, Result, image};

/// Locations distributions install OVMF to, as (code, variables) pairs.
const OVMF_PATHS: &[(&str, &str)] = &[
    // Debian and Ubuntu (4 MiB build, then the older 2 MiB build)
    ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
    ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
    // Fedora
    ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd"),
    // Arch Linux
    ("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd"),
    ("/usr/share/edk2-ovmf/x64/OVMF_CODE.fd", "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"),
];

/// Finds OVMF's code and variable-store images.
pub fn find_ovmf() -> Option<(PathBuf, PathBuf)> {
    if let (Some(code), Some(vars)) =
        (std::env::var_os("OB_OVMF_CODE"), std::env::var_os("OB_OVMF_VARS"))
    {
        return Some((code.into(), vars.into()));
    }
    OVMF_PATHS
        .iter()
        .map(|&(code, vars)| (PathBuf::from(code), PathBuf::from(vars)))
        .find(|(code, vars)| code.is_file() && vars.is_file())
}

/// Whether this user can open /dev/kvm for hardware acceleration.
pub fn kvm_available() -> bool {
    std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok()
}

/// Where the guest's first serial port goes.
pub enum Serial {
    /// This terminal, multiplexed with the QEMU monitor (Ctrl-A C).
    Terminal,
    /// A log file.
    File(PathBuf),
}

/// One virtual machine.
pub struct Machine {
    /// Firmware to boot through.
    pub firmware: Firmware,
    /// How the image is attached.
    pub media: Media,
    /// Boot image.
    pub image: PathBuf,
    /// Writable copy of the OVMF variable store, for UEFI.
    pub ovmf: Option<(PathBuf, PathBuf)>,
    /// Guest memory.
    pub memory: String,
    /// Virtual CPUs.
    pub cpus: u32,
    /// Use KVM.
    pub kvm: bool,
    /// Serial port destination.
    pub serial: Serial,
    /// Open a window for the screen.
    pub display: bool,
    /// Extra arguments passed straight to QEMU.
    pub extra: Vec<String>,
}

impl Machine {
    /// QEMU's arguments for this machine.
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = vec!["-M".into(), "q35".into()];
        args.extend(["-m".into(), self.memory.clone(), "-smp".into(), self.cpus.to_string()]);
        if self.kvm {
            args.extend(["-accel".into(), "kvm".into(), "-cpu".into(), "host".into()]);
        } else {
            args.extend(["-accel".into(), "tcg".into(), "-cpu".into(), "max".into()]);
        }
        if let (Firmware::Uefi, Some((code, vars))) = (self.firmware, &self.ovmf) {
            args.push("-drive".into());
            args.push(format!("if=pflash,format=raw,readonly=on,file={}", code.display()));
            args.push("-drive".into());
            args.push(format!("if=pflash,format=raw,file={}", vars.display()));
        }
        // `snapshot=on` keeps the image file unchanged whatever the guest writes.
        let drive = format!("if=none,id=boot,format=raw,snapshot=on,file={}", self.image.display());
        args.extend(["-drive".into(), drive]);
        match self.media {
            Media::Usb => args.extend([
                "-device".into(),
                "qemu-xhci,id=xhci".into(),
                "-device".into(),
                "usb-storage,bus=xhci.0,drive=boot,removable=on,bootindex=0".into(),
            ]),
            Media::Cdrom => args.extend(["-device".into(), "ide-cd,drive=boot,bootindex=0".into()]),
            Media::Disk => args.extend(["-device".into(), "ide-hd,drive=boot,bootindex=0".into()]),
        }
        // Radix leaves QEMU through this device when booted with `qemu-exit`.
        args.extend(["-device".into(), "isa-debug-exit,iobase=0xf4,iosize=0x04".into()]);
        args.extend(["-nic".into(), "none".into(), "-no-reboot".into()]);
        match &self.serial {
            Serial::Terminal => args.extend(["-serial".into(), "mon:stdio".into()]),
            Serial::File(path) => {
                args.extend(["-serial".into(), format!("file:{}", path.display())]);
                args.extend(["-monitor".into(), "none".into()]);
            }
        }
        if !self.display {
            args.extend(["-display".into(), "none".into()]);
        }
        args.extend(self.extra.iter().cloned());
        args
    }

    /// A ready-to-spawn QEMU command.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new("qemu-system-x86_64");
        cmd.args(self.args());
        cmd
    }
}

/// Copies OVMF for a machine named `name`; each run starts with fresh NVRAM.
pub fn prepare_ovmf(ctx: &Ctx, name: &str) -> Result<(PathBuf, PathBuf)> {
    let (code, vars) = find_ovmf().ok_or_else(|| {
        Error::new("OVMF (UEFI firmware for QEMU) is not installed")
            .hint("run `./ob doctor`, or use --bios to boot through SeaBIOS")
    })?;
    let copy = ctx.out.join("qemu").join(format!("{name}-vars.fd"));
    process::copy(&vars, &copy)?;
    Ok((code, copy))
}

/// Fails early, with a hint, when QEMU is missing.
pub fn require_qemu() -> Result<()> {
    if process::find_program("qemu-system-x86_64").is_none() {
        return Err(Error::new("qemu-system-x86_64 is not installed")
            .hint("run `./ob doctor` to see what is missing"));
    }
    Ok(())
}

fn has_display() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// `ob run`.
pub fn run_command(ctx: &Ctx, args: &mut Args) -> Result<()> {
    let firmware = match (args.flag("--bios"), args.flag("--uefi")) {
        (true, true) => crate::bail!("--bios and --uefi cannot be combined"),
        (true, false) => Firmware::Bios,
        (false, true) => Firmware::Uefi,
        (false, false) => ctx.config.run.firmware,
    };
    let media = match args.value("--media")? {
        Some(value) => Media::parse(&value)?,
        None => ctx.config.run.media,
    };
    let headless = args.flag("--headless") || !has_display();
    let profile = args.profile(&ctx.config)?;
    let extra = args.take_passthrough();
    args.finish()?;

    require_qemu()?;
    let image_path = image::build_release(ctx, profile, false)?;
    let machine = Machine {
        firmware,
        media,
        ovmf: match firmware {
            Firmware::Uefi => Some(prepare_ovmf(ctx, "run")?),
            Firmware::Bios => None,
        },
        image: image_path,
        memory: ctx.config.run.memory.clone(),
        cpus: ctx.config.run.cpus,
        kvm: kvm_available(),
        serial: Serial::Terminal,
        display: !headless,
        extra,
    };
    let accel = if machine.kvm { "KVM" } else { "no KVM, so it runs slower" };
    step(&format!(
        "Booting {} from {media} under {firmware} in QEMU ({accel})",
        ctx.display(&machine.image)
    ));
    eprintln!("    Serial console follows. Press Ctrl-A then X to quit QEMU.");
    if headless {
        eprintln!("    No QEMU window: no display found or --headless given.");
    }
    eprintln!();
    let status = process::status(ctx, &mut machine.command())?;
    match status.code() {
        // 33 is a clean exit through isa-debug-exit (the `qemu-exit` flag).
        Some(0 | 33) => Ok(()),
        Some(35) => crate::bail!("the kernel panicked; the serial output above shows why"),
        _ => crate::bail!("QEMU exited with {status}"),
    }
}

/// Builds the boot-test machine for one firmware and media combination.
pub fn test_machine(
    ctx: &Ctx,
    image: &Path,
    firmware: Firmware,
    media: Media,
    log: PathBuf,
) -> Result<Machine> {
    let name = format!("test-{}-{media}", firmware.to_string().to_lowercase());
    Ok(Machine {
        firmware,
        media,
        ovmf: match firmware {
            Firmware::Uefi => Some(prepare_ovmf(ctx, &name)?),
            Firmware::Bios => None,
        },
        image: image.to_path_buf(),
        memory: "512M".into(),
        cpus: 1,
        kvm: kvm_available(),
        serial: Serial::File(log),
        display: false,
        extra: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(firmware: Firmware, media: Media) -> Machine {
        Machine {
            firmware,
            media,
            image: "out/test.img".into(),
            ovmf: Some(("code.fd".into(), "vars.fd".into())),
            memory: "1G".into(),
            cpus: 2,
            kvm: false,
            serial: Serial::File("serial.log".into()),
            display: false,
            extra: vec!["-s".into()],
        }
    }

    #[test]
    fn uefi_usb_machine() {
        let args = machine(Firmware::Uefi, Media::Usb).args().join(" ");
        assert!(args.contains("if=pflash,format=raw,readonly=on,file=code.fd"));
        assert!(args.contains("usb-storage,bus=xhci.0,drive=boot"));
        assert!(args.contains("snapshot=on"));
        assert!(args.contains("-serial file:serial.log -monitor none"));
        assert!(args.contains("-accel tcg -cpu max"));
        assert!(args.ends_with("-display none -s"));
    }

    #[test]
    fn bios_never_loads_ovmf() {
        let args = machine(Firmware::Bios, Media::Cdrom).args().join(" ");
        assert!(!args.contains("pflash"));
        assert!(args.contains("ide-cd,drive=boot,bootindex=0"));
    }
}
