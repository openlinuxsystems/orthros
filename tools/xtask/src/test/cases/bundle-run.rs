// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bundles run in a sandbox of their own (ADR 0225). Alice
//! signs in, as the owner, and installs two bundles of her own. The first
//! asks for no permission; its command is a script that runs in its
//! sandbox: its home is its data folder, which it may change; it sees
//! only its own folders of alice's home, not her files; it cannot change
//! the system's settings; and it holds no network capability, so it
//! cannot make a socket (netgrant none). The second asks to connect out
//! and to listen on ports 8000 to 8009 and 9000, and gets exactly that
//! (netgrant granted). Each command is on her PATH, a link to bundle(1)
//! in her applications/commands, run by its name or by bundle run.
//! Unsealed by an administrator, a bundle no longer runs; removing one
//! removes its command. The power button ends it.

use super::super::*;

/// The test.
pub const CASE: Case = Case {
    name: "bundle-run",
    about: "bundles run in a sandbox of their own (ADR 0225): one that asks for no permission cannot make a socket, sees only its own data of alice's home and cannot change the system's settings; one that asks to connect out and listen on ports gets exactly that; commands are on PATH as links to bundle(1); an unsealed bundle does not run",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    // As a normal boot: Spark starts the services, bundled among them.
    cmdline: "lockdep",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: bundle_run,
    disks: &[],
    second: None,
};

/// The first bundle's command: a script that tells what its sandbox
/// holds, then runs netgrant, which it carries, to check that it holds no
/// network capability.
const OFFLINE: &str = "#!/bin/sh\\necho home $HOME\\nls /users/alice\\ncat /users/alice/secret.txt\\n\
                       echo secret $?\\ntouch $HOME/kept\\necho data $?\\ntouch /system/settings/x\\n\
                       echo settings $?\\nexec $(dirname $0)/netgrant none\\n";

/// The second bundle's permissions, as its manifest writes them.
const ONLINE: &str = r#"{"network.outbound": {"need": "required"}, "network.listen": {"need": "required", "ports": ["8000-8009", 9000]}}"#;

fn bundle_run(vm: &mut Vm, case: &Case) -> Result<(), String> {
    owner_signs_in(vm, case)?;
    vm.expect_anywhere("bundled: 0 bundles installed; answering at /system/live/bundled/socket")?;
    // The first: no permission, a script, and netgrant beside it. The
    // test image's homes are in the root's tmpfs, 1 MiB in all, so what
    // is installed goes as soon as it is.
    answer_prompt(
        vm,
        "$",
        &format!(
            "clear; cd; echo secret > secret.txt; bundle new org.example.offline offline > \
             /dev/null; cp /system/tests/posix/netgrant offline/bin/; printf '{OFFLINE}' > \
             offline/bin/offline; chmod 755 offline/bin/offline; bundle pack offline -o \
             offline.dkv > /dev/null; bundle install --yes offline.dkv | tail -1; rm -r offline \
             offline.dkv"
        ),
    )?;
    vm.expect_screen(&["installed org.example.offline 0.1.0, for you"])?;
    answer_prompt(vm, "$", "clear; bundle run org.example.offline; echo \"status $?\"")?;
    vm.expect_screen(&[
        "home /users/alice/app-data/org.example.offline",
        "app-data  applications",
        "secret 1",
        "data 0",
        "settings 1",
        "netgrant: Spark gave it no network",
        "netgrant: every check passed",
        "status 0",
    ])?;
    // Its data stays in alice's app-data; its command is on her PATH.
    answer_prompt(
        vm,
        "$",
        "clear; ls app-data/org.example.offline; command -v offline; readlink \
         applications/commands/offline",
    )?;
    vm.expect_screen(&[
        "kept",
        "/users/alice/applications/commands/offline",
        "/system/commands/bundle",
    ])?;
    // Unsealed by an administrator, it no longer runs.
    answer_prompt(
        vm,
        "$",
        "clear; sudo chmod 755 applications/org.example.offline; offline; echo \"status $?\"",
    )?;
    answer_prompt(vm, "chmod 755 applications/org.example.offline:", "queen of hearts")?;
    vm.expect_screen(&["is not sealed", "status 1"])?;
    // Removing it removes its command.
    answer_prompt(
        vm,
        "$",
        "clear; bundle remove --purge org.example.offline; echo \"links $(ls \
         applications/commands | wc -l)\"",
    )?;
    vm.expect_screen(&["removed org.example.offline 0.1.0, and its data", "links 0"])?;
    // The second: to connect out and listen on its ports, and no more.
    answer_prompt(
        vm,
        "$",
        &format!(
            "clear; bundle new org.example.online online > /dev/null; cp \
             /system/tests/posix/netgrant online/bin/online; sed -i 's/\"permissions\": \
             {{}}/\"permissions\": {ONLINE}/' online/manifest.json; bundle pack online -o \
             online.dkv > /dev/null; bundle install --yes online.dkv | grep -e Accept -e \
             installed; rm -r online online.dkv"
        ),
    )?;
    vm.expect_screen(&[
        "Accept connections on these ports: 8000-8009, 9000 (at install)",
        "installed org.example.online 0.1.0, for you",
    ])?;
    answer_prompt(vm, "$", "clear; online granted; echo \"status $?\"")?;
    vm.expect_screen(&[
        "netgrant: Spark gave it the network to connect out and to listen on ports",
        "netgrant: every check passed",
        "status 0",
    ])?;
    vm.press_power_button()?;
    vm.expect("spark: every service has stopped")?;
    vm.expect("Power: `spark` asked to power off the machine; syncing the filesystems")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}
