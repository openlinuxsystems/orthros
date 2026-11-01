// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Grants and the trusted prompt (ADR 0224), the Phase 13 exit's bundle.
//! Alice signs in, as the owner, and installs into her applications a
//! bundle that asks to read and change her documents folder and, if she
//! lets it, to use a serial port. Run the first time, it is asked about
//! on the console's bottom line, which consoled draws: A allows her
//! documents from now on, D leaves out the serial port, and the audit log
//! says so. It writes in her documents but not in her downloads, and is
//! not asked about again. Taking the folder away while it runs stops it;
//! run again it stops at once, saying how to give the folder back. bundle
//! grant gives it her documents to read only, after the prompt's Y, and
//! bundle info shows what she said. The power button ends it.

use super::super::*;

/// The test.
pub const CASE: Case = Case {
    name: "bundle-grants",
    about: "grants and the trusted prompt (ADR 0224): an app asks for alice's documents on its first run, A allows them from now on and D leaves out the serial port; it writes in her documents but not her downloads; bundle revoke stops it at once; bundle grant gives her documents to read only after the prompt's Y",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    // As a normal boot: Spark starts the services, bundled, authd and
    // consoled among them.
    cmdline: "lockdep",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: bundle_grants,
    disks: &[],
    second: None,
};

/// The bundle's command: a script that writes in alice's documents and
/// downloads, and lists her documents; with `wait`, says it started and
/// waits.
const NOTES: &str = "#!/bin/sh\\nif [ \"$1\" = wait ]; then touch $HOME/started; sleep 60; echo woke; \
                     exit 0; fi\\ntouch /users/alice/documents/note\\necho documents $?\\ntouch \
                     /users/alice/downloads/note\\necho downloads $?\\nls /users/alice/documents\\n";

/// Its permissions, as its manifest writes them.
const PERMISSIONS: &str = r#"{"files.folder": {"need": "required", "folders": {"documents": "read-write"}}, "devices.serial": {"need": "optional"}}"#;

fn bundle_grants(vm: &mut Vm, case: &Case) -> Result<(), String> {
    owner_signs_in(vm, case)?;
    vm.expect_anywhere("bundled: 0 bundles installed; answering at /system/live/bundled/socket")?;
    vm.expect_anywhere("asks for passwords on the consoles")?;
    answer_prompt(
        vm,
        "$",
        &format!(
            "clear; cd; mkdir documents downloads; bundle new org.example.notes --name Notes > \
             /dev/null; printf '{NOTES}' > notes/bin/notes; chmod 755 notes/bin/notes; sed -i \
             's/\"permissions\": {{}}/\"permissions\": {PERMISSIONS}/' notes/manifest.json; \
             bundle pack notes -o notes.dkv > /dev/null; bundle install --yes notes.dkv | grep -e \
             yours -e serial -e installed; rm -r notes notes.dkv"
        ),
    )?;
    vm.expect_screen(&[
        "Use these folders of yours: documents (read-write) (on first use)",
        "Use a serial port or USB serial device (on first use, optional)",
        "installed org.example.notes 0.1.0, for you",
    ])?;
    // Its first run asks, on the bottom line, which no program draws.
    answer_prompt(vm, "$", "clear; notes; echo \"status $?\"")?;
    vm.expect_screen(&["[permission] Notes (org.example", "A: always"])?;
    vm.type_text("a")?;
    vm.expect_anywhere(
        "bundled: alice allowed org.example.notes files.folder (documents=read-write), from now \
         on, on pts/",
    )?;
    vm.expect_screen(&["D: don't allow"])?;
    vm.type_text("d")?;
    vm.expect_anywhere(
        "bundled: alice did not allow org.example.notes devices.serial, from now on, on pts/",
    )?;
    // Her documents, not her downloads: the Phase 13 exit.
    vm.expect_screen(&["documents 0", "downloads 1", "note", "status 0"])?;
    // Asked once: the next run just runs.
    answer_prompt(vm, "$", "clear; notes; echo \"again $?\"")?;
    vm.expect_screen(&["documents 0", "again 0"])?;
    // Taken away while it runs, it stops at once; run again, it stops
    // before it starts, saying how to give it back.
    answer_prompt(
        vm,
        "$",
        "clear; sh -c 'notes wait & until [ -e app-data/org.example.notes/started ]; do sleep 1; \
         done; bundle revoke org.example.notes files.folder; wait $!; echo \"status $?\"'",
    )?;
    vm.expect_screen(&[
        "took files.folder away from org.example.notes; 1 running told at once",
        "org.example.notes: stopped, as you took files.folder away from it",
        "status 143",
    ])?;
    vm.expect_anywhere("bundled: alice took files.folder away from org.example.notes")?;
    answer_prompt(vm, "$", "clear; notes; echo \"status $?\"")?;
    vm.expect_screen(&["needs \"Use these folders of yours\" (files.folder),", "status 1"])?;
    // bundle grant gives less than it asks for, after the prompt's Y.
    answer_prompt(
        vm,
        "$",
        "clear; bundle grant org.example.notes files.folder documents=read; echo \"status $?\"",
    )?;
    vm.expect_screen(&["[permission] Give Notes (org.example.notes)", "Y: yes"])?;
    vm.type_text("y")?;
    vm.expect_screen(&["gave org.example.notes files.folder (documents=read)", "status 0"])?;
    answer_prompt(vm, "$", "clear; notes; echo \"status $?\"")?;
    vm.expect_screen(&["documents 1", "downloads 1", "note", "status 0"])?;
    answer_prompt(vm, "$", "clear; bundle info org.example.notes | grep -e said -e allowed")?;
    vm.expect_screen(&[
        "You said:",
        "devices.serial: not allowed, since",
        "files.folder: allowed: documents=read, since",
    ])?;
    vm.press_power_button()?;
    vm.expect("spark: every service has stopped")?;
    vm.expect("Power: `spark` asked to power off the machine; syncing the filesystems")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}
