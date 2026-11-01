// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A USB stick that BitLocker To Go encrypted, with AES-CBC of 128 bits as
//! Windows encrypts removable drives, a password and a recovery key, its
//! NTFS holding a file. Spark must find it locked at boot and say how to
//! unlock it. At the console, *unlock* must refuse a wrong password and
//! unlock it with the right one, showing neither, and volumed must mount
//! the unlocked volume at its label; its file must read back, and one
//! written there is synced. On the host, *dislocker-file* must decrypt the
//! stick with its recovery key, and ntfs-3g must find both files and the
//! volume sound (ADR 0240). dislocker and ntfs-3g must be installed (*ob
//! doctor* says how).

use std::process::Command;

use radix_bitlocker::cipher::Method;
use radix_bitlocker::make::{self, Options, RESERVED};

use super::super::*;

pub const CASE: Case = Case {
    name: "bitlocker",
    about: "A USB stick BitLocker To Go encrypted, its NTFS holding a file: Spark finds it locked, unlock refuses a wrong password at the console and unlocks it with the right one, showing neither, and volumed mounts it; the file reads back, and dislocker decrypts on the host what Radix wrote",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "console-sign-in=off",
    cpus: 1,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: bitlocker,
    disks: BITLOCKER_DISKS,
    second: None,
};

/// The stick: 32 MiB on a SuperSpeed port.
const BITLOCKER_DISKS: &[Disk] = &[Disk {
    kib: 32 * 1024,
    fat: BITLOCKER,
    cluster: 4096,
    label: "SECRET",
    table: Table::None,
    bus: Bus::Usb("2"),
    sector: 512,
    device: "",
    errors: None,
    read_only: false,
    check: decrypted,
}];

/// The password that unlocks the stick.
const PASSWORD: &str = "correct horse battery staple";
/// What the stick's file holds, as Windows wrote it.
const HELLO: &str = "Locked away by BitLocker.\n";
/// What the test writes to the stick, at the console.
const WRITTEN: &str = "42 written on Dukovo\n";

/// The stick's recovery key, as Windows prints it.
fn recovery() -> String {
    make::recovery_key(2026)
}

/// Makes the stick's image at `path`: an NTFS `mkntfs` made of all but its
/// last [`RESERVED`] bytes, holding `hello.txt`, which `ntfscp` wrote, then
/// encrypted as BitLocker To Go encrypts it, which takes those bytes.
pub fn make(disk: &Disk, path: &Path) -> Result<(), String> {
    let bytes = disk.kib * 1024;
    std::fs::File::create(path)
        .and_then(|file| file.set_len(bytes))
        .map_err(|err| format!("making {}: {err}", path.display()))?;
    let sectors = (bytes - RESERVED) / 512;
    let mut mkntfs = Command::new(fat_tool("mkntfs"));
    mkntfs.args(["-q", "-F", "-Q", "-L", disk.label, "-s", "512"]);
    mkntfs.args(["-c", &disk.cluster.to_string()]).arg(path).arg(sectors.to_string());
    host_tool(&mut mkntfs)?;
    let source = path.with_extension("hello");
    std::fs::write(&source, HELLO).map_err(|err| format!("writing {}: {err}", source.display()))?;
    let mut ntfscp = Command::new(fat_tool("ntfscp"));
    let copied = host_tool(ntfscp.arg("-q").arg(path).arg(&source).arg("/hello.txt"));
    let _ = std::fs::remove_file(&source);
    copied?;
    let mut image =
        std::fs::read(path).map_err(|err| format!("reading {}: {err}", path.display()))?;
    let method = Method::Cbc128;
    let fvek: Vec<u8> = (0..method.key_bytes()).map(|i| (i * 37 + 11) as u8).collect();
    let recovery = recovery();
    make::make(
        &mut image,
        &Options {
            method,
            fvek: &fvek,
            vmk: [0x5a; 32],
            password: Some(PASSWORD),
            recovery: Some(&recovery),
            clear: false,
            to_go: true,
            sector: disk.sector,
            description: "DUKOVO-TEST SECRET 2026-10-31",
        },
    );
    std::fs::write(path, image).map_err(|err| format!("writing {}: {err}", path.display()))
}

/// The `bitlocker` test: Spark leaves the stick locked; unlock(8) at the
/// console refuses a wrong password and takes the right one, and volumed
/// mounts the volume it unlocks, which is read and written there.
fn bitlocker(vm: &mut Vm, case: &Case) -> Result<(), String> {
    disks_found(vm, case)?;
    let (name, label) = (&volume_names(case.disks)[0], case.disks[0].label);
    vm.expect(&format!("spark: {name} is a locked BitLocker volume; `unlock {name}` unlocks it"))?;
    vm.expect("spark: started consoled (/system/commands/consoled) as process ")?;
    vm.expect_screen(&[&format!("{RELEASE} {VERSION} (tty1)"), "# "])?;
    vm.type_text(&format!("unlock {name}\n"))?;
    vm.expect_screen(&[
        &format!("unlock: {name} is a BitLocker volume: DUKOVO-TEST SECRET 2026-10-31"),
        &format!("Password or recovery key for {name}:"),
    ])?;
    vm.type_text("not the password\n")?;
    vm.expect_screen(&["that password or recovery key does not open it"])?;
    vm.type_text(&format!("{PASSWORD}\n"))?;
    vm.expect_screen(&[&format!("unlock: {name} is unlocked as /devices/{name}-unlocked")])?;
    vm.expect(&format!("Disk: {name}-unlocked: the BitLocker volume on {name} unlocked, AES 128, "))?;
    vm.expect(&format!("volumed: mounted the NTFS filesystem of {name}-unlocked at /volumes/{label}"))?;
    // Typed without an echo.
    vm.expect_screen_without(PASSWORD)?;
    vm.expect_screen_without("not the password")?;
    vm.type_text(&format!("cat /volumes/{label}/hello.txt\n"))?;
    vm.expect_screen(&[HELLO.trim_end()])?;
    // Needles that are not in the lines typed.
    vm.type_text(&format!(
        "echo \"$((6 * 7)) written on Dukovo\" > /volumes/{label}/written.txt && sync && echo \"$((6 * 7)) synced\"\n"
    ))?;
    vm.expect_screen(&["42 synced"])?;
    vm.type_text("poweroff\n")?;
    vm.expect("spark: asked to power off the machine")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}

/// Cuts the decrypted stick at `path` to its NTFS, which BitLocker's
/// parts follow here (Windows keeps them in files of the NTFS instead):
/// ntfs-3g looks for the NTFS's backup boot sector at the device's end.
fn ntfs_alone(path: &Path) -> Result<(), String> {
    let mut boot = [0u8; 512];
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .and_then(|mut file| file.read_exact(&mut boot).map(|()| file))
        .map_err(|err| format!("reading {}: {err}", path.display()))?;
    let sectors = u64::from_le_bytes(boot[0x28..0x30].try_into().unwrap_or_default());
    file.set_len((sectors + 1) * 512).map_err(|err| format!("cutting {}: {err}", path.display()))
}

/// Checks the stick after the boot: `dislocker-file` decrypts it with its
/// recovery key, and ntfs-3g finds the volume sound ([`ntfs_checked`]),
/// the file Windows wrote as it was, and the one Radix wrote.
///
/// [`ntfs_checked`]: phase11::ntfs_checked
fn decrypted(disk: &DiskImage, _log: &str) -> Result<(), String> {
    let out = disk.path.with_extension("decrypted");
    let _ = std::fs::remove_file(&out);
    let mut dislocker = Command::new("dislocker-file");
    dislocker.arg("-V").arg(&disk.path).arg(format!("-p{}", recovery())).arg("--").arg(&out);
    let result = host_tool(&mut dislocker)
        .map_err(|err| format!("{}: dislocker-file: {err}", disk.path.display()))
        .and_then(|_| ntfs_alone(&out))
        .and_then(|()| {
            let plain = DiskImage {
                path: out.clone(),
                offset: 0,
                bytes: disk.bytes,
                sector: disk.sector,
                check: disk.check,
                btrfs: None,
            };
            phase11::ntfs_checked(&plain, "")?;
            for (name, want) in [("/hello.txt", HELLO), ("/written.txt", WRITTEN)] {
                let mut ntfscat = Command::new(fat_tool("ntfscat"));
                let got = host_tool(ntfscat.arg("-f").arg(&out).arg(name))?;
                if got != want.as_bytes() {
                    return Err(format!(
                        "{name} on {} decrypted reads {:?}",
                        disk.path.display(),
                        String::from_utf8_lossy(&got)
                    ));
                }
            }
            Ok(())
        });
    let _ = std::fs::remove_file(&out);
    result
}
