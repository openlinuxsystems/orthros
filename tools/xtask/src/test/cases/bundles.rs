// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! bundled installs, seals and removes bundles (ADR 0223).
//! Alice and bob sign in as in signin. Alice makes a bundle, packs it
//! and installs it for herself: it is signed by her developer key, which
//! bundle new gave bundled, so the sheet warns of nothing; installed, it
//! is root's and read-only, and bundle list and info show it. Bob
//! installs her file only after the warning that this computer does not
//! know its key: no, then --yes. Through elevate alice installs it for
//! everyone, which binds its ID to her key for everyone: bob's own bundle
//! of that ID is refused. Removing asks about the data: alice's goes,
//! bob keeps his, and the one for everyone goes through elevate. The
//! power button ends it.

use super::super::*;

/// The test.
pub const CASE: Case = Case {
    name: "bundles",
    about: "bundled installs, seals and removes bundles (ADR 0223): alice makes, packs and installs one signed by her developer key with no warning; it is root's and read-only, and bundle list and info show it; bob installs her file only after the warning that this computer does not know its key; an administrator installs it for everyone through elevate, after which bob's own bundle of the same ID is refused; removing asks about the data, and deletes it or keeps it",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    // As signin: Spark starts the services, bundled among them, and
    // signintest as root, which makes bob once alice is made.
    cmdline: "lockdep -- --services --posix /system/tests/posix/signintest",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &[],
    script: bundles,
    disks: &[],
    second: None,
};

/// Makes the test's bundle, `org.example.notes`, in the current folder,
/// and packs it into `FILE`, signed by the developer key.
fn make_bundle(file: &str) -> String {
    format!(
        "bundle new org.example.notes notes > /dev/null; echo echo notes > notes/bin/notes; chmod \
         755 notes/bin/notes; bundle pack notes -o {file} > /dev/null"
    )
}

/// A row of `bundle list`, as it lines the test's bundle up.
fn listed(version: &str, whose: &str, name: &str) -> String {
    format!("{:17}  {:7}  {whose:9}  {name}", "org.example.notes", version)
}

fn bundles(vm: &mut Vm, case: &Case) -> Result<(), String> {
    owner_signs_in(vm, case)?;
    bob_signs_in(vm)?;
    vm.expect_anywhere("signintest: every check passed")?;
    vm.expect_anywhere("bundled: 0 bundles installed; answering at /system/live/bundled/socket")?;
    // Alice makes, packs and installs a bundle of her own.
    vm.press(&["alt-f1"])?;
    answer_prompt(
        vm,
        "$",
        &format!("clear; cd; {}; bundle install notes.dkv", make_bundle("notes.dkv")),
    )?;
    vm.expect_screen(&[
        "notes 0.1.0, for yourself",
        "  org.example.notes, by alice",
        "  Your developer key, which this computer trusts for you alone.",
        "  It asks for no permission.",
        "  Commands: notes",
    ])?;
    vm.expect_screen_without("Warning")?;
    answer_prompt(vm, "Install it? [y/N]", "y")?;
    vm.expect_screen(&["installed org.example.notes 0.1.0, for you"])?;
    vm.expect_anywhere(
        "bundled: alice installed org.example.notes 0.1.0 for alice, signed by key RW",
    )?;
    vm.expect_anywhere("(their developer key)")?;
    // Sealed: root's, read-only, and nothing is added.
    answer_prompt(
        vm,
        "$",
        "clear; ls -ld applications/org.example.notes | cut -c1-10; stat -c '%U %a' \
         applications/org.example.notes/bin/notes; touch applications/org.example.notes/x; echo \
         \"touch $?\"; bundle list; bundle info org.example.notes | tail -3",
    )?;
    vm.expect_screen(&[
        "dr-xr-xr-x",
        "root 555",
        "Permission denied",
        "touch 1",
        &format!("{:17}  {:7}  {:9}  NAME", "ID", "VERSION", "FOR"),
        &listed("0.1.0", "you", "notes"),
        "  Installed on ",
        "  In /users/alice/applications/org.example.notes",
        "  Its data: /users/alice/app-data/org.example.notes",
    ])?;
    // Bob gets her file, and is warned of its key: no, then yes.
    answer_prompt(
        vm,
        "$",
        "clear; sudo cp notes.dkv /users/bob/; sudo chown bob /users/bob/notes.dkv; echo \"given $?\"",
    )?;
    answer_prompt(vm, "cp notes.dkv /users/bob/:", "queen of hearts")?;
    // Checking the password takes a while: bob looks once it is his.
    vm.expect_screen(&["given 0"])?;
    vm.press(&["alt-f2"])?;
    answer_prompt(vm, "$", "clear; cd; bundle install notes.dkv; echo \"status $?\"")?;
    vm.expect_screen(&[
        "notes 0.1.0, for yourself",
        "  Warning: this computer does not know this key. That the bundle comes",
        "  from alice is its own word, which nothing has checked:",
    ])?;
    answer_prompt(vm, "Install it? [y/N]", "n")?;
    vm.expect_screen(&["bundle: notes.dkv: not installed, as you said", "status 1"])?;
    answer_prompt(vm, "$", "clear; bundle install --yes notes.dkv | tail -1")?;
    vm.expect_screen(&["installed org.example.notes 0.1.0, for you"])?;
    vm.expect_anywhere("(a key this computer did not know)")?;
    // For everyone, through elevate: the ID is now bound to her key for
    // everyone, so bob's own bundle of that ID is refused.
    vm.press(&["alt-f1"])?;
    answer_prompt(
        vm,
        "$",
        "clear; elevate bundle install --for-everyone --yes notes.dkv | tail -1; ls /applications",
    )?;
    vm.expect_screen(&["installed org.example.notes 0.1.0, for everyone", "org.example.notes"])?;
    vm.press(&["alt-f2"])?;
    answer_prompt(
        vm,
        "$",
        &format!(
            "clear; cd; mkdir mine; cd mine; {}; bundle install --yes mine.dkv 2> ../why; echo \
             \"status $?\"; cd; grep -q 'bound to key RW.* for everyone, since its first install' \
             why && echo 'refused: bound'",
            make_bundle("mine.dkv")
        ),
    )?;
    vm.expect_screen(&["status 1", "refused: bound"])?;
    // Removing asks about the data: alice's goes with it, bob keeps his.
    vm.press(&["alt-f1"])?;
    answer_prompt(
        vm,
        "$",
        "clear; mkdir app-data/org.example.notes; echo kept > app-data/org.example.notes/notes; \
         bundle remove org.example.notes",
    )?;
    answer_prompt(vm, "[k]eep or [d]elete:", "d")?;
    vm.expect_screen(&["removed org.example.notes 0.1.0, and its data"])?;
    // Only the folder of its commands stays (ADR 0225).
    answer_prompt(
        vm,
        "$",
        "clear; echo \"left $( (ls app-data; ls applications; ls applications/commands) | grep \
         -cvx commands)\"; bundle list",
    )?;
    vm.expect_screen(&["left 0", &listed("0.1.0", "everyone", "notes")])?;
    vm.press(&["alt-f2"])?;
    answer_prompt(
        vm,
        "$",
        "clear; mkdir app-data/org.example.notes; bundle remove --keep-data org.example.notes; ls \
         app-data",
    )?;
    vm.expect_screen(&[
        "removed org.example.notes 0.1.0",
        "its settings and data stay; a later version of it picks them up",
        "org.example.notes",
    ])?;
    vm.press(&["alt-f1"])?;
    answer_prompt(vm, "$", "clear; elevate bundle remove --for-everyone org.example.notes; bundle list")?;
    vm.expect_screen(&["removed org.example.notes 0.1.0", "no bundles are installed"])?;
    vm.expect_anywhere("bundled: root removed org.example.notes 0.1.0 for everyone")?;
    vm.press_power_button()?;
    vm.expect("spark: every service has stopped")?;
    vm.expect("Power: `spark` asked to power off the machine; syncing the filesystems")?;
    vm.expect_exit(QEMU_EXIT_POWER_OFF)
}
