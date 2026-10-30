// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Starting QEMU, for `ob run` and the boot tests.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Firmware, Media};
use crate::image::{self, Overrides};
use crate::process::{self, step};
use crate::{Args, Context, Ctx, Error, Result, kernel};

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

/// TCP port of QEMU's GDB stub for `ob run --gdb`.
pub const GDB_PORT: u16 = 1234;

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

/// `path` as the value of a QEMU option such as `-drive file=`, where a
/// comma ends the value unless it is doubled.
pub fn option_value(path: &Path) -> String {
    path.display().to_string().replace(',', ",,")
}

/// Whether this user can open /dev/kvm for hardware acceleration.
pub fn kvm_available() -> bool {
    std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok()
}

/// Where the guest's first serial port goes.
pub enum Serial {
    /// This terminal, multiplexed with the QEMU monitor (Ctrl-A C).
    Terminal,
    /// QEMU's standard input and output, for a test that reads and types.
    Pipe,
}

/// A kernel QEMU loads itself, with an initramfs and a command line,
/// instead of an image booted through firmware: how `ob bench` runs Linux.
pub struct DirectBoot {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub cmdline: String,
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
    /// CPU model instead of the default (`host` with KVM, `max` without).
    pub cpu_model: Option<String>,
    /// Serial port destination.
    pub serial: Serial,
    /// Open a window for the screen.
    pub display: bool,
    /// Let a guest restart instead of making QEMU exit.
    pub reboot: bool,
    /// Wait for GDB on [`GDB_PORT`] before running the firmware.
    pub gdb: bool,
    /// QMP control socket, for screenshots in the boot tests.
    pub qmp: Option<PathBuf>,
    /// Extra arguments passed straight to QEMU.
    pub extra: Vec<String>,
    /// Boot this kernel directly; `firmware`, `media` and `image` are then
    /// not used.
    pub direct: Option<DirectBoot>,
}

impl Machine {
    /// QEMU's arguments for this machine.
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = vec!["-M".into(), "q35".into()];
        // An IOMMU translates only the devices made after it.
        let iommu = self.extra.windows(2).position(|pair| {
            pair[0] == "-device"
                && (pair[1].starts_with("intel-iommu") || pair[1].starts_with("amd-iommu"))
        });
        if let Some(at) = iommu {
            args.extend(self.extra[at..at + 2].iter().cloned());
            // KVM's own interrupt controllers cannot have an IOMMU remap
            // their interrupts; QEMU's I/O APIC, with KVM's local APICs, can.
            if self.kvm && self.extra[at + 1].contains("intremap=on") {
                args.extend(["-machine".into(), "kernel-irqchip=split".into()]);
            }
        }
        args.extend(["-m".into(), self.memory.clone(), "-smp".into(), self.cpus.to_string()]);
        let (accel, default_cpu) = if self.kvm { ("kvm", "host") } else { ("tcg", "max") };
        let cpu = self.cpu_model.as_deref().unwrap_or(default_cpu);
        args.extend(["-accel".into(), accel.into(), "-cpu".into(), cpu.into()]);
        if let Some(direct) = &self.direct {
            args.extend(["-kernel".into(), direct.kernel.display().to_string()]);
            args.extend(["-initrd".into(), direct.initrd.display().to_string()]);
            args.extend(["-append".into(), direct.cmdline.clone()]);
        } else {
            self.image_args(&mut args);
        }
        // Radix leaves QEMU through this device when booted with `qemu-exit`.
        args.extend(["-device".into(), "isa-debug-exit,iobase=0xf4,iosize=0x04".into()]);
        args.extend(["-nic".into(), "none".into()]);
        if !self.reboot {
            args.push("-no-reboot".into());
        }
        match &self.serial {
            Serial::Terminal => args.extend(["-serial".into(), "mon:stdio".into()]),
            Serial::Pipe => {
                args.extend(["-serial".into(), "stdio".into(), "-monitor".into(), "none".into()]);
            }
        }
        if let Some(socket) = &self.qmp {
            args.extend([
                "-qmp".into(),
                format!("unix:{},server=on,wait=off", option_value(socket)),
            ]);
        }
        if self.gdb {
            args.extend(["-gdb".into(), format!("tcp:localhost:{GDB_PORT}"), "-S".into()]);
        }
        if !self.display {
            args.extend(["-display".into(), "none".into()]);
        }
        let skip = iommu.map_or(0..0, |at| at..at + 2);
        args.extend(
            self.extra
                .iter()
                .enumerate()
                .filter(|(i, _)| !skip.contains(i))
                .map(|(_, a)| a.clone()),
        );
        args
    }

    /// The firmware and the boot image.
    fn image_args(&self, args: &mut Vec<String>) {
        if let (Firmware::Uefi, Some((code, vars))) = (self.firmware, &self.ovmf) {
            args.push("-drive".into());
            args.push(format!("if=pflash,format=raw,readonly=on,file={}", option_value(code)));
            args.push("-drive".into());
            args.push(format!("if=pflash,format=raw,file={}", option_value(vars)));
        }
        // `snapshot=on` keeps the image file unchanged whatever the guest writes.
        let drive =
            format!("if=none,id=boot,format=raw,snapshot=on,file={}", option_value(&self.image));
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

/// The GDB script `ob run --gdb` writes: it loads the kernel's symbols,
/// connects to QEMU and stops at the first Rust function of the kernel.
pub fn gdb_script(kernel: &Path) -> String {
    format!(
        "# Written by `./ob run --gdb`; see radix-debug(7).\n\
         set architecture i386:x86-64\n\
         file {}\n\
         target remote localhost:{GDB_PORT}\n\
         # A hardware breakpoint survives the switch from the boot loader to\n\
         # the kernel's page tables.\n\
         hbreak radix_arch_entry\n\
         continue\n",
        kernel.display()
    )
}

/// `cmdline` with `flag` added, unless it is there already.
fn with_flag(cmdline: &str, flag: &str) -> String {
    if cmdline.split_ascii_whitespace().any(|word| word == flag) {
        cmdline.to_string()
    } else {
        format!("{} {flag}", cmdline.trim()).trim_start().to_string()
    }
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
    let gdb = args.flag("--gdb");
    let mut overrides = Overrides {
        cmdline: args.value("--cmdline")?,
        microcode: args.value("--microcode")?,
        disk_check: None,
    };
    if gdb {
        // GDB's symbols are at the link address, so the kernel must stay
        // there.
        let cmdline = overrides.cmdline.as_deref().unwrap_or(&ctx.config.boot.cmdline);
        overrides.cmdline = Some(with_flag(cmdline, "nokaslr"));
    }
    let profile = args.profile(&ctx.config)?;
    let extra = args.take_passthrough();
    args.finish()?;

    require_qemu()?;
    let image_path = image::build_release(ctx, profile, false, &overrides)?;
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
        cpu_model: None,
        serial: Serial::Terminal,
        display: !headless,
        // A restart boots the image again, as on a real machine, so the
        // next boot can show the crash log.
        reboot: true,
        gdb,
        qmp: None,
        extra,
        direct: None,
    };
    let accel = if machine.kvm { "KVM" } else { "no KVM, so it runs slower" };
    step(&format!(
        "Booting {} from {media} under {firmware} in QEMU ({accel})",
        ctx.display(&machine.image)
    ));
    if gdb {
        let script = ctx.out.join("qemu/radix.gdb");
        process::create_dir(script.parent().expect("the script has a parent"))?;
        std::fs::write(&script, gdb_script(&kernel::installed(ctx)))
            .context(|| format!("writing {}", script.display()))?;
        eprintln!("    QEMU waits for GDB on localhost:{GDB_PORT}. In another terminal run:");
        eprintln!("        gdb -x {}", ctx.display(&script));
        eprintln!("    GDB stops at radix_arch_entry, the kernel's first Rust function.");
        eprintln!("    KASLR is off (nokaslr), so the kernel runs where GDB's symbols say.");
    }
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
            cpu_model: None,
            serial: Serial::Terminal,
            display: false,
            reboot: false,
            gdb: false,
            qmp: None,
            extra: vec!["-s".into()],
            direct: None,
        }
    }

    #[test]
    fn option_values_double_their_commas() {
        assert_eq!(option_value(Path::new("/a,b/c.img")), "/a,,b/c.img");
        assert_eq!(option_value(Path::new("plain.img")), "plain.img");
    }

    #[test]
    fn uefi_usb_machine() {
        let args = machine(Firmware::Uefi, Media::Usb).args().join(" ");
        assert!(args.contains("if=pflash,format=raw,readonly=on,file=code.fd"));
        assert!(args.contains("usb-storage,bus=xhci.0,drive=boot"));
        assert!(args.contains("snapshot=on"));
        assert!(args.contains("-no-reboot"));
        assert!(args.contains("-serial mon:stdio"));
        assert!(args.contains("-accel tcg -cpu max"));
        assert!(args.ends_with("-display none -s"));
    }

    #[test]
    fn bios_never_loads_ovmf() {
        let args = machine(Firmware::Bios, Media::Cdrom).args().join(" ");
        assert!(!args.contains("pflash"));
        assert!(args.contains("ide-cd,drive=boot,bootindex=0"));
    }

    #[test]
    fn test_options() {
        let mut m = machine(Firmware::Bios, Media::Usb);
        m.reboot = true;
        m.serial = Serial::Pipe;
        m.qmp = Some("/tmp/qmp.sock".into());
        m.cpu_model = Some("qemu64,vendor=GenuineIntel".into());
        m.gdb = true;
        let args = m.args().join(" ");
        assert!(!args.contains("-no-reboot"));
        assert!(args.contains("-serial stdio -monitor none"));
        assert!(args.contains("-qmp unix:/tmp/qmp.sock,server=on,wait=off"));
        assert!(args.contains("-cpu qemu64,vendor=GenuineIntel"));
        assert!(args.contains(&format!("-gdb tcp:localhost:{GDB_PORT} -S")));
    }

    #[test]
    fn direct_boot_skips_the_image() {
        let mut m = machine(Firmware::Uefi, Media::Usb);
        m.direct = Some(DirectBoot {
            kernel: "/tmp/vmlinuz".into(),
            initrd: "/tmp/init.cpio".into(),
            cmdline: "console=ttyS0 panic=-1".into(),
        });
        let args = m.args();
        let joined = args.join(" ");
        assert!(joined.contains("-kernel /tmp/vmlinuz -initrd /tmp/init.cpio -append"));
        assert!(args.contains(&"console=ttyS0 panic=-1".to_string()));
        assert!(!joined.contains("pflash"));
        assert!(!joined.contains("out/test.img"));
    }

    #[test]
    fn gdb_turns_kaslr_off_once() {
        assert_eq!(with_flag("", "nokaslr"), "nokaslr");
        assert_eq!(with_flag("loglevel=debug", "nokaslr"), "loglevel=debug nokaslr");
        assert_eq!(with_flag("nokaslr kdb", "nokaslr"), "nokaslr kdb");
    }

    #[test]
    fn gdb_script_stops_in_the_kernel() {
        let script = gdb_script(Path::new("/src/out/sysroot/boot/radix"));
        assert!(script.contains("file /src/out/sysroot/boot/radix\n"));
        assert!(script.contains(&format!("target remote localhost:{GDB_PORT}\n")));
        assert!(script.contains("hbreak radix_arch_entry\n"));
    }
}
