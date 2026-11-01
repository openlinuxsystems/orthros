// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The permission list (ADR 0221): every permission a bundle may ask for,
//! the words the system shows for it, which an app cannot reword, how it
//! is asked for, and what it takes. Manifest version 1 defines those that
//! arrive before Dukovo 1.0; the rest are reserved, so that no bundle uses
//! the name for something else, and are defined in the manifest version
//! of the release that brings them.

/// How a permission is given (the security design's five tiers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Shown when the bundle is installed, and given with it.
    AtInstall,
    /// Asked the first time the app uses it.
    FirstUse,
    /// Asked with a warning, as it reaches what other apps keep.
    Warning,
    /// An administrator's to give, through elevate.
    Admin,
    /// Always given; each person may turn it off for the app.
    Always,
}

impl Tier {
    /// How manifest.json(5) and `bundle check` say it.
    pub fn words(self) -> &'static str {
        match self {
            Tier::AtInstall => "at install",
            Tier::FirstUse => "on first use",
            Tier::Warning => "with a warning",
            Tier::Admin => "by an administrator",
            Tier::Always => "always, unless turned off",
        }
    }
}

/// What a permission takes besides `need` and `why`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Takes {
    /// Nothing more.
    Nothing,
    /// `folders`: your folders ([`PERSONAL_FOLDERS`]), each `read` or
    /// `read-write`.
    PersonalFolders,
    /// `folders`: folders of `/shared` by name, each `read` or
    /// `read-write`.
    SharedFolders,
    /// `ports`: ports and ranges of ports, such as `80` or `"8000-8009"`.
    Ports,
    /// `apps`: the IDs of other bundles.
    Apps,
}

/// A permission of the list.
#[derive(Clone, Copy, Debug)]
pub struct Permission {
    /// Its name, as a manifest writes it.
    pub name: &'static str,
    /// What it lets the program do, as the system says it.
    pub lets: &'static str,
    /// How it is given.
    pub tier: Tier,
    /// What it takes besides `need` and `why`.
    pub takes: Takes,
    /// The Dukovo release that defines it, for a reserved one; none for
    /// one manifest version 1 defines.
    pub reserved_until: Option<&'static str>,
}

const fn defined(name: &'static str, lets: &'static str, tier: Tier, takes: Takes) -> Permission {
    Permission { name, lets, tier, takes, reserved_until: None }
}

const fn reserved(
    name: &'static str,
    lets: &'static str,
    tier: Tier,
    release: &'static str,
) -> Permission {
    Permission { name, lets, tier, takes: Takes::Nothing, reserved_until: Some(release) }
}

use Takes::{Apps, Nothing, PersonalFolders, Ports, SharedFolders};
use Tier::{Admin, Always, AtInstall, FirstUse, Warning};

/// Every permission, in the order manifest.json(5) lists them.
pub const LIST: &[Permission] = &[
    defined("files.folder", "Use these folders of yours", FirstUse, PersonalFolders),
    defined("files.volumes", "Use drives you plug in", FirstUse, Nothing),
    defined("files.shared", "Use these folders of /shared", FirstUse, SharedFolders),
    defined("files.home", "Use all of your files", Warning, Nothing),
    defined("files.app-data", "Read these other apps' private data", Warning, Apps),
    defined(
        "network.outbound",
        "Connect to the internet and to services on this computer",
        AtInstall,
        Nothing,
    ),
    defined("network.listen", "Accept connections on these ports", AtInstall, Ports),
    defined(
        "network.local",
        "Reach devices on your local network and see what discovery finds",
        FirstUse,
        Nothing,
    ),
    defined("network.raw", "Send raw packets and capture network traffic", Admin, Nothing),
    defined("devices.serial", "Use a serial port or USB serial device", FirstUse, Nothing),
    defined("start-at-login", "Start when you sign in", AtInstall, Nothing),
    defined("notifications", "Show notifications", Always, Nothing),
    defined("processes.debug", "Trace or debug your other programs", Warning, Nothing),
    defined(
        "runs-with-your-access",
        "Run with everything you can reach, without a sandbox",
        Warning,
        Nothing,
    ),
    defined("admin.users", "Administer accounts", Admin, Nothing),
    defined("admin.services", "Administer services", Admin, Nothing),
    defined("admin.network", "Administer the network", Admin, Nothing),
    defined("admin.firewall", "Administer the firewall", Admin, Nothing),
    defined("admin.storage", "Administer disks and volumes", Admin, Nothing),
    defined("admin.updates", "Administer updates", Admin, Nothing),
    defined("admin.apps", "Administer apps for everyone", Admin, Nothing),
    defined("admin.logs", "Read the logs", Admin, Nothing),
    defined("admin.settings", "Change the system's settings", Admin, Nothing),
    defined("admin.power", "Shut down and restart the computer", Admin, Nothing),
    defined("admin.printers", "Administer printers", Admin, Nothing),
    defined("admin.hardware", "Drive the computer's fans, LEDs and lights", Admin, Nothing),
    defined("admin.time", "Set the computer's clock", Admin, Nothing),
    defined("admin.users-files", "Reach every person's files", Admin, Nothing),
    reserved("devices.usb", "Use a USB device directly", FirstUse, "1.6"),
    reserved("devices.camera", "Use a camera", FirstUse, "1.6.6"),
    reserved("devices.microphone", "Record sound", FirstUse, "1.7"),
    reserved("devices.bluetooth", "Use Bluetooth devices", FirstUse, "1.6.5"),
    reserved("devices.scanner", "Use a scanner directly", FirstUse, "1.7.1"),
    reserved("devices.gpu-compute", "Compute on the GPU or NPU without a window", AtInstall, "1.6"),
    reserved("devices.hardware", "Drive hardware directly, as a driver", Admin, "1.8"),
    reserved("display", "Show windows", AtInstall, "1.1"),
    reserved("screen.capture", "Record or share the screen", FirstUse, "1.5.2"),
    reserved("input.control", "Control other apps' input", Warning, "1.5"),
    reserved(
        "shortcuts.global",
        "Use its own keyboard shortcuts while another app is in front",
        FirstUse,
        "1.5",
    ),
    reserved("clipboard.read", "Read the clipboard while not in front", FirstUse, "1.1"),
    reserved("background", "Keep running when its windows are closed", AtInstall, "1.1"),
    reserved("location", "Know where the computer is", FirstUse, "1.6"),
    reserved("contacts", "Use your contacts", FirstUse, "1.5.6"),
    reserved("calendar", "Use your calendars", FirstUse, "1.5.6"),
    reserved("reminders", "Use your reminders", FirstUse, "1.5.6"),
    reserved("mail", "Use your mail", FirstUse, "1.5.6"),
    reserved("keychain.shared", "Use keychain items another app saved", Warning, "1.4.1"),
    reserved("ai.ask", "Ask the local assistant", FirstUse, "1.10"),
    reserved(
        "memory.jit",
        "Generate machine code as it runs, as JavaScript engines do",
        AtInstall,
        "1.2",
    ),
];

/// The permission `name`, if the list has it.
pub fn find(name: &str) -> Option<&'static Permission> {
    LIST.iter().find(|p| p.name == name)
}

/// Your folders, which `files.folder` names: those every home has.
pub const PERSONAL_FOLDERS: &[&str] =
    &["documents", "downloads", "desktop", "development", "music", "pictures", "videos"];
