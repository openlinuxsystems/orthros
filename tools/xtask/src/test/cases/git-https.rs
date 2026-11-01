// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! git over `https://` (ADR 0292), against a smart HTTP server on lo
//! that git-https-test runs (userspace/programs/git-https-test): TLS by
//! rustls with the network-services test's authority, Basic sign-in with
//! a token, and git-http-backend behind it. git must refuse a certificate
//! it cannot trust; fail without a token, and with a wrong one; with the
//! token given once, keep it in memory with credential-cache, the helper
//! the system's gitconfig names, and clone, commit and push without
//! asking, the push landing in the server's repository; and keep the
//! token in no file, asking again once the cache exits.

use super::super::*;

/// The test.
pub const CASE: Case = Case {
    name: "git-https",
    about: "git clones from and pushes to an https:// server on lo with a token that git credential-cache keeps in memory only; an untrusted certificate, no token and a wrong one are refused (ADR 0292)",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep -- --posix --network /system/tests/posix/git-https-test",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: git_https,
    disks: &[],
    second: None,
};

/// The lines git-https-test prints for its parts, in order.
const PARTS: &[&str] = &[
    "git-https-test: git: ",
    "git-https-test: untrusted: ",
    "git-https-test: no token: ",
    "git-https-test: token: ",
    "git-https-test: forgotten: ",
];

fn git_https(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    vm.expect_ahead("spark: started /system/tests/posix/git-https-test as process ")?;
    for part in PARTS {
        vm.expect_ahead(part)?;
    }
    let code = program_line(vm, "(/system/tests/posix/git-https-test) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> =
        log.lines().filter(|line| line.contains("git-https-test: FAILED")).map(str::trim).collect();
    if !failed.is_empty() || !log.contains("git-https-test: every check passed") || code != "0x0" {
        return Err(format!("the git https test failed ({code}): {}", failed.join("; ")));
    }
    Ok(())
}
