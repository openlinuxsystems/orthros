// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Phase 12's boot tests from before tests got a file each. A new
//! test goes in test/cases instead (see its README).

use super::*;

/// Phase 12's boot tests, run after those in `super::CASES`.
pub const CASES: &[Case] = &[Case {
    name: "tls",
    about: "TLS on Radix: ring's primitives against OpenSSL's answers and signatures, rustls's handshakes over lo with every version, suite, group and kind of key, the certificates a client must refuse, and Mozilla's root certificates where Linux's programs look for them",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep -- --posix --network /system/tests/posix/tls-test",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: tls,
    disks: &[],
    second: None,
}];

fn tls(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    for line in [
        "tls-test: known answers: SHA-256, SHA-384, SHA-512, HMAC, HKDF, and 36 messages sealed with AES-GCM and ChaCha20-Poly1305 as OpenSSL seals them",
        "tls-test: signatures: 5 of OpenSSL's, ECDSA P-256 and P-384, Ed25519, RSA PKCS #1 and RSA-PSS, checked",
        "tls-test: handshakes: 19 of 19 over lo, each with the version, suite, group and key asked for",
        "tls-test: bulk: 4 MiB went each way through one TLS 1.3 connection",
        "tls-test: refusals: 8 of 8: ",
        "tls-test: roots: ",
    ] {
        vm.expect(line)?;
    }
    let roots = program_line(vm, "")?;
    let count = roots.split(' ').next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
    if count < 100 {
        return Err(format!("only {count} root certificates: {roots}"));
    }
    let code = program_line(vm, "(/system/tests/posix/tls-test) exited with code ")?;
    let log = vm.log();
    let failed: Vec<&str> =
        log.lines().filter(|line| line.contains("tls-test: FAILED: ")).collect();
    if !failed.is_empty() || !log.contains("tls-test: every check passed") || code != "0x0" {
        return Err(format!("the tls test failed ({code}): {}", failed.join("; ")));
    }
    Ok(())
}
