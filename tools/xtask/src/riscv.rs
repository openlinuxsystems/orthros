// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob test riscv`: the RISC-V smoke boot (ADR 0270).
//!
//! Builds `radix-riscv64-smoke` (`kernel/arch/riscv64`) for
//! `riscv64gc-unknown-none-elf`, Rust's `core` built from the toolchain's
//! sources as the kernel's is, and boots it on QEMU's `virt` machine
//! under the OpenSBI QEMU ships (`-bios default`), without KVM. The boot
//! passes when its lines come in order and QEMU powers off by itself.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::process::{self, step};
use crate::{Ctx, Error, Result, kernel};

/// Rust's target for the smoke boot.
pub const TARGET: &str = "riscv64gc-unknown-none-elf";

/// The smoke boot's program.
const PROGRAM: &str = "radix-riscv64-smoke";

/// QEMU for RISC-V.
pub const QEMU: &str = "qemu-system-riscv64";

/// How long the boot may take: a second or two under TCG.
const TIMEOUT: Duration = Duration::from_secs(60);

/// What the target needs of Cargo: `core` from the toolchain's sources,
/// with its memory functions.
pub fn target_args() -> [&'static str; 4] {
    [
        "--target",
        TARGET,
        "-Zbuild-std=core,compiler_builtins,alloc",
        "-Zbuild-std-features=compiler-builtins-mem",
    ]
}

/// The lines the boot prints, in order, each by its start.
fn expected() -> [String; 5] {
    [
        format!(
            "Radix {} (riscv64): the smoke boot, on hart 0 of riscv-virtio,qemu",
            env!("CARGO_PKG_VERSION")
        ),
        "SBI: version ".into(),
        "Paging: Sv39 is on; the first gigabyte of RAM is mapped at 0x80000000 and at \
         0xffffffc080000000, and both read the same"
            .into(),
        "Timer: the SBI timer interrupt came ".into(),
        "Power: the smoke boot is done; powering off through SBI".into(),
    ]
}

/// Builds the smoke boot; its ELF file.
fn build(ctx: &Ctx) -> Result<PathBuf> {
    step("Building the RISC-V smoke boot");
    let mut cmd = kernel::cargo(ctx, "build")?;
    cmd.args(["-p", "radix-arch-riscv64", "--bin", PROGRAM, "--release"]).args(target_args());
    process::run(ctx, &mut cmd)?;
    Ok(kernel::target_dir(ctx).join(TARGET).join("release").join(PROGRAM))
}

/// `ob test riscv`.
pub fn test(ctx: &Ctx) -> Result<()> {
    if process::find_program(QEMU).is_none() {
        return Err(Error::new(format!("{QEMU} is not installed"))
            .hint("install QEMU's RISC-V emulator, which brings OpenSBI: sudo apt install qemu-system-misc"));
    }
    let program = build(ctx)?;
    step("Booting it on QEMU's virt machine under OpenSBI, without KVM");
    let log = ctx.out.join("test").join("riscv-smoke.log");
    std::fs::create_dir_all(log.parent().expect("the log is in out/test"))
        .map_err(|err| Error::new(format!("creating out/test: {err}")))?;
    let mut cmd = Command::new(QEMU);
    cmd.args(["-M", "virt", "-m", "128M", "-smp", "1", "-bios", "default", "-kernel"])
        .arg(&program)
        .args(["-display", "none", "-serial", "stdio", "-monitor", "none", "-no-reboot"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if ctx.verbose {
        eprintln!("+ {}", process::render(&cmd));
    }
    let mut child = cmd.spawn().map_err(|err| Error::new(format!("running {QEMU}: {err}")))?;
    let reader = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    };
    let stdout = reader(Box::new(child.stdout.take().expect("stdout is piped")));
    let stderr = reader(Box::new(child.stderr.take().expect("stderr is piped")));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let mut text = String::from_utf8_lossy(&stdout.join().unwrap_or_default()).into_owned();
    text.push_str(&String::from_utf8_lossy(&stderr.join().unwrap_or_default()));
    std::fs::write(&log, &text)
        .map_err(|err| Error::new(format!("writing {}: {err}", log.display())))?;
    check(&text, status.map(|status| status.success()))
        .map_err(|err| err.hint(format!("the boot's output is in {}", log.display())))?;
    println!("    PASS  riscv-smoke     {:.1} s", start.elapsed().as_secs_f64());
    Ok(())
}

/// Whether `output` has every expected line in order, and QEMU powered
/// off by itself (`exited`: whether with status 0, `None` if it was
/// stopped after the timeout).
fn check(output: &str, exited: Option<bool>) -> Result<()> {
    let mut lines = output.lines().map(str::trim_end);
    for want in expected() {
        if !lines.any(|line| line.starts_with(want.as_str())) {
            return Err(Error::new(format!(
                "the RISC-V smoke boot did not print `{want}` where it should"
            )));
        }
    }
    match exited {
        Some(true) => Ok(()),
        Some(false) => Err(Error::new("QEMU ended the RISC-V smoke boot with a failure")),
        None => Err(Error::new(format!(
            "the RISC-V smoke boot did not power off in {} s",
            TIMEOUT.as_secs()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A boot's output, as QEMU gives it: OpenSBI's lines, then the
    /// smoke boot's, each ending in a carriage return.
    fn boot() -> String {
        let [banner, sbi, paging, timer, power] = expected();
        let lines = [
            "OpenSBI v1.3".to_string(),
            "Boot HART ID              : 0".to_string(),
            banner,
            format!("{sbi}1.0, by OpenSBI 1.3; the console is its Debug Console"),
            paging,
            format!("{timer}1191 µs after it was asked for, at 10 MHz"),
            power,
        ];
        lines.iter().flat_map(|line| [line.as_str(), "\r\n"]).collect()
    }

    #[test]
    fn a_whole_boot_passes() {
        assert!(check(&boot(), Some(true)).is_ok());
    }

    #[test]
    fn a_missing_or_late_line_fails() {
        let without = boot().replace("Paging: Sv39", "Paging: Sv48");
        let err = check(&without, Some(true)).unwrap_err();
        assert!(err.to_string().contains("Paging: Sv39 is on"), "{err}");
        let whole = boot();
        let lines: Vec<&str> = whole.lines().collect();
        let mut swapped = lines.clone();
        let (power, timer) = (lines.len() - 1, lines.len() - 2);
        swapped.swap(power, timer);
        assert!(check(&swapped.join("\n"), Some(true)).is_err());
    }

    #[test]
    fn qemu_must_power_off_by_itself() {
        assert!(check(&boot(), Some(false)).is_err());
        let err = check(&boot(), None).unwrap_err();
        assert!(err.to_string().contains("did not power off"), "{err}");
    }
}
