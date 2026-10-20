//! `ob test`: host unit tests and QEMU boot tests.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::config::{Firmware, Media};
use crate::image::{self, ImageSpec};
use crate::process::{self, step};
use crate::{Args, Ctx, Error, Result, VERSION, kernel, qemu};

/// QEMU's exit status after the kernel writes 0x10 to isa-debug-exit.
const QEMU_EXIT_SUCCESS: i32 = 33;

/// The boot matrix: every firmware from both kinds of removable media.
const BOOTS: &[(Firmware, Media)] = &[
    (Firmware::Uefi, Media::Usb),
    (Firmware::Bios, Media::Usb),
    (Firmware::Uefi, Media::Cdrom),
    (Firmware::Bios, Media::Cdrom),
];

/// `ob test`.
pub fn command(ctx: &Ctx, args: &mut Args) -> Result<()> {
    let which = args.positional();
    let timeout = match args.value("--timeout")? {
        Some(value) => value.parse::<u64>().ok().filter(|&secs| secs > 0).ok_or_else(|| {
            Error::new(format!("--timeout takes a number of seconds, not `{value}`"))
        })?,
        None => 300,
    };
    let profile = args.profile(&ctx.config)?;
    args.finish()?;
    let (unit, boot) = match which.as_deref() {
        None => (true, true),
        Some("unit") => (true, false),
        Some("boot") => (false, true),
        Some(other) => crate::bail!("unknown test suite `{other}`; use `unit` or `boot`"),
    };

    if unit {
        unit_tests(ctx)?;
    }
    if boot {
        let kernel = kernel::build(ctx, profile)?;
        boot_tests(ctx, &kernel, Duration::from_secs(timeout))?;
    }
    Ok(())
}

fn unit_tests(ctx: &Ctx) -> Result<()> {
    step("Running unit tests for the build tool");
    let mut cmd = process::cargo(ctx);
    cmd.args(["test", "--workspace"]);
    process::run(ctx, &mut cmd)?;

    step("Running unit tests for the kernel libraries on the host");
    let mut cmd = process::cargo(ctx);
    cmd.args(["test", "--manifest-path", "kernel/Cargo.toml", "-p", "radix-bootinfo"])
        .arg("--target-dir")
        .arg(ctx.target.join("kernel"));
    process::run(ctx, &mut cmd)
}

fn boot_tests(ctx: &Ctx, kernel: &Path, timeout: Duration) -> Result<()> {
    qemu::require_qemu()?;
    let dir = ctx.out.join("test");
    let test_image = dir.join("orthros-test.img");
    image::build(
        ctx,
        &ImageSpec {
            kernel,
            timeout: 0,
            cmdline: "qemu-exit",
            staging: dir.join("image-root"),
            output: test_image.clone(),
        },
    )?;

    let kvm = qemu::kvm_available();
    step(&format!(
        "Booting the test image {} times in QEMU ({})",
        BOOTS.len(),
        if kvm { "KVM" } else { "no KVM, so each boot takes longer" }
    ));
    let mut failures = 0;
    for &(firmware, media) in BOOTS {
        let name = format!("{}-{media}", firmware.to_string().to_lowercase());
        let log = dir.join(format!("boot-{name}.log"));
        let _ = std::fs::remove_file(&log);
        let machine = qemu::test_machine(ctx, &test_image, firmware, media, log.clone())?;
        let started = Instant::now();
        let stderr_log = dir.join(format!("boot-{name}.qemu.log"));
        let outcome = boot_once(ctx, &machine, timeout, &stderr_log).and_then(|()| {
            check_boot_log(&std::fs::read_to_string(&log).unwrap_or_default(), firmware)
        });
        let secs = started.elapsed().as_secs_f32();
        match outcome {
            Ok(()) => eprintln!("    PASS  {firmware} from {media} ({secs:.1} s)"),
            Err(reason) => {
                failures += 1;
                eprintln!("    FAIL  {firmware} from {media} ({secs:.1} s): {reason}");
                eprintln!("          serial log: {}", ctx.display(&log));
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                for line in
                    clean_log(&text).lines().rev().take(15).collect::<Vec<_>>().into_iter().rev()
                {
                    eprintln!("          | {line}");
                }
            }
        }
    }
    if failures > 0 {
        crate::bail!("{failures} of {} boot tests failed", BOOTS.len());
    }
    step(&format!("All {} boot tests passed", BOOTS.len()));
    Ok(())
}

/// Runs one machine to completion and checks QEMU's exit status.
fn boot_once(
    ctx: &Ctx,
    machine: &qemu::Machine,
    timeout: Duration,
    stderr_log: &Path,
) -> Result<(), String> {
    let stderr = std::fs::File::create(stderr_log)
        .map_err(|err| format!("creating {}: {err}", stderr_log.display()))?;
    let mut cmd = machine.command();
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr);
    if ctx.verbose {
        eprintln!("+ {}", process::render(&cmd));
    }
    let mut child = cmd.spawn().map_err(|err| format!("cannot start QEMU: {err}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no result within {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(err) => return Err(format!("waiting for QEMU: {err}")),
        }
    };
    match status.code() {
        Some(QEMU_EXIT_SUCCESS) => Ok(()),
        Some(35) => Err("the kernel panicked".into()),
        _ => {
            let stderr = std::fs::read_to_string(stderr_log).unwrap_or_default();
            Err(format!("QEMU exited with {status}: {}", stderr.trim()))
        }
    }
}

/// Removes carriage returns and terminal escape sequences from a serial log.
pub fn clean_log(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {}
            '\u{1b}' => {
                // CSI sequences end with a byte in 0x40..=0x7e.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// Checks that the kernel booted, identified the firmware and finished.
pub fn check_boot_log(raw: &str, firmware: Firmware) -> Result<(), String> {
    let log = clean_log(raw);
    if let Some(line) = log.lines().find(|line| line.starts_with("Radix panic")) {
        return Err(format!("kernel panic: {line}"));
    }
    let expected = [
        format!("Radix {VERSION} ("),
        format!("Firmware: {firmware}"),
        "Boot complete.".to_string(),
    ];
    for needle in &expected {
        if !log.contains(needle.as_str()) {
            return Err(format!("serial output lacks \"{needle}\""));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_log(firmware: &str) -> String {
        format!(
            "\u{1b}[2J\u{1b}[01;01HBdsDxe: loading Boot0001\r\n\
             Radix {VERSION} (OrthrOS Phase 0 stub kernel)\r\n\
             Firmware: {firmware}\r\nBoot complete.\r\n"
        )
    }

    #[test]
    fn accepts_a_good_boot() {
        check_boot_log(&good_log("UEFI"), Firmware::Uefi).unwrap();
        check_boot_log(&good_log("BIOS"), Firmware::Bios).unwrap();
    }

    #[test]
    fn rejects_wrong_firmware_and_panics() {
        assert!(check_boot_log(&good_log("BIOS"), Firmware::Uefi).is_err());
        let panicked = format!("{}Radix panic: oops\n", good_log("UEFI"));
        assert!(check_boot_log(&panicked, Firmware::Uefi).unwrap_err().contains("oops"));
        assert!(check_boot_log("", Firmware::Uefi).is_err());
    }

    #[test]
    fn strips_escapes() {
        assert_eq!(clean_log("\u{1b}[1;1HRadix\r\n\u{1b}[=3hok"), "Radix\nok");
    }
}
