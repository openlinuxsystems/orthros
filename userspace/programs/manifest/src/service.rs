// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Service files (service.toml(5), ADR 0216): what Spark starts and keeps
//! running.
//!
//! Each `NAME.toml` in `/system/settings/spark/services` describes the
//! service NAME, in TOML. The file says which version of the format it
//! was written for in `service_version`; a later Dukovo reads every
//! earlier version, and a file for a later one is refused with a message
//! to update Dukovo. An unknown setting is an error, so a misspelt one is
//! never silently ignored.
//!
//! ```toml
//! service_version = 1
//! description = "System log"
//! program = "/system/commands/logd"
//! order = 5
//! restart = "always"
//!
//! [limits]
//! memory = "64M"
//!
//! [permissions]
//! network.listen = [514]
//! ```

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use radix_abi::resource_domain::{NAME_MAX, WEIGHT_DEFAULT, WEIGHT_MAX};
use radix_abi::startup::NETWORK_SLOTS;
use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, Visitor};

use crate::Error;

/// The version of the format this reads, the newest there is.
pub const VERSION: u32 = 1;

/// Every setting of a service file, a table's after the table: what
/// service.toml(5) must document.
pub const KEYS: &[&str] = &[
    "service_version",
    "description",
    "program",
    "arguments",
    "environment",
    "order",
    "enabled",
    "restart",
    "limits",
    "cpu_weight",
    "memory",
    "oom",
    "secrets",
    "permissions",
    "network.outbound",
    "network.listen",
    "files.read",
    "files.read-write",
    "files.mount",
    "files.shared",
    "runs-with-your-access",
    "admin.network",
    "admin.hardware",
    "names",
];

/// The order of a service whose file gives none.
pub const ORDER_DEFAULT: u32 = 50;

/// When a service that ended is started again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Restart {
    /// Whatever its end.
    #[default]
    Always,
    /// When it failed: an exit code other than 0, or a signal.
    OnFailure,
    /// Never.
    Never,
}

impl Restart {
    const WORDS: &[&str] = &["always", "on-failure", "never"];

    /// The word a file says it with.
    pub fn name(self) -> &'static str {
        Self::WORDS[self as usize]
    }
}

/// Which programs the out-of-memory policy stops first (ADR 0103).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Oom {
    /// A service's: stopped after apps'.
    #[default]
    Service,
    /// An app's.
    App,
}

impl Oom {
    const WORDS: &[&str] = &["service", "app"];

    /// The word a file says it with.
    pub fn name(self) -> &'static str {
        Self::WORDS[self as usize]
    }
}

/// A service file, read.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "File")]
pub struct Service {
    /// What it is, in a few words, for `service status`.
    pub description: String,
    /// The program's absolute path.
    pub program: String,
    /// The arguments after the program's name.
    pub arguments: Vec<String>,
    /// More of the environment, as names and values, by name.
    pub environment: Vec<(String, String)>,
    /// Lower starts first; shutdown stops the highest first.
    pub order: u32,
    /// Whether Spark starts it at boot.
    pub enabled: bool,
    /// When it is started again after it ends.
    pub restart: Restart,
    /// What its resource domain may use.
    pub limits: Limits,
    /// The names of the secrets Spark hands it as read-only files.
    pub secrets: Vec<String>,
    /// What it may do beyond running.
    pub permissions: Permissions,
}

impl Service {
    /// How many network capabilities it gets: one to connect out, one for
    /// each range of ports it listens on, and one for admin.network.
    pub fn network_grants(&self) -> usize {
        self.permissions.network.grants() + usize::from(self.permissions.admin.network)
    }
}

/// What its resource domain may use.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Its share of the CPUs, from 1 to 10000; 100 is everyone's.
    #[serde(default = "weight_default", deserialize_with = "weight")]
    pub cpu_weight: u32,
    /// Its memory limit, in bytes, if it has one.
    #[serde(default, deserialize_with = "memory")]
    pub memory: Option<u64>,
    /// Which programs the out-of-memory policy stops first.
    #[serde(default, deserialize_with = "oom")]
    pub oom: Oom,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { cpu_weight: WEIGHT_DEFAULT, memory: None, oom: Oom::Service }
    }
}

/// What it may do beyond running: the permission list's names, in groups.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawPermissions")]
pub struct Permissions {
    /// `network.*`: what of the network it may use.
    pub network: Network,
    /// `files.*`: the places of the tree its view holds, beyond
    /// [`ALWAYS`].
    pub files: Files,
    /// `runs-with-your-access`: the whole tree as Spark holds it, with
    /// the bypass right, in place of a view.
    pub runs_with_your_access: bool,
    /// `admin.*`: what of the machine it may administer.
    pub admin: Admin,
    /// `names`: the names capability (radix-names(2), ADR 0218), which
    /// gives processes and capabilities another user's name: authd's, to
    /// build sessions with.
    pub names: bool,
}

impl Permissions {
    /// Every place its view holds beyond [`ALWAYS`], in the order Spark
    /// attaches them.
    pub fn places(&self) -> Vec<Place> {
        let files = &self.files;
        let read = files.read.iter().map(|path| Place { path: path.clone(), write: false });
        let write = files.read_write.iter().chain(&files.mount);
        let write = write.map(|path| Place { path: path.clone(), write: true });
        let shared = files.shared.iter().map(|name| Place { path: shared(name), write: true });
        read.chain(write).chain(shared).collect()
    }
}

/// The administration group: `admin.network` and `admin.hardware`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admin {
    /// Whether it may set the network interfaces up, their addresses and
    /// routers, through a network capability with only the control right
    /// (ADR 0190).
    #[serde(default)]
    pub network: bool,
    /// Whether it gets the platform capability, which sets the machine's
    /// fans, LEDs and ARGB header (ADR 0208).
    #[serde(default)]
    pub hardware: bool,
}

/// The files group: the places of the tree a service's view holds, each
/// at its real path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Files {
    /// `files.read`: places it may read.
    #[serde(default, deserialize_with = "places")]
    pub read: Vec<String>,
    /// `files.read-write`: places it may read and change.
    #[serde(default, rename = "read-write", deserialize_with = "places")]
    pub read_write: Vec<String>,
    /// `files.mount`: places it may read, change and mount filesystems
    /// beneath, which it gets as descriptors from 3 too.
    #[serde(default, deserialize_with = "places")]
    pub mount: Vec<String>,
    /// `files.shared`: folders of `/shared` it may read and change, by
    /// name.
    #[serde(default, deserialize_with = "folders")]
    pub shared: Vec<String>,
}

/// A place of the tree in a service's view: a directory or a file, at its
/// real path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// Its absolute path.
    pub path: String,
    /// Whether the service may change it, or only read it.
    pub write: bool,
}

/// What every service's view holds without asking (ADR 0217): the
/// system's programs, libraries, resources and settings, the processes'
/// and the kernel's files, read-only; the devices every program uses;
/// notifyd's folder, as notifications are always allowed; and netd's,
/// whose resolv.conf `/etc/resolv.conf` leads to (ADR 0191). Its program,
/// its own `/tmp`, the root's links and its secrets come on top. A place
/// a file names that is one of these takes its place.
pub const ALWAYS: &[(&str, bool)] = &[
    ("/system/commands", false),
    ("/system/libraries", false),
    ("/system/resources", false),
    ("/system/settings", false),
    ("/system/live/notify", false),
    ("/system/live/netd", false),
    ("/processes", false),
    ("/kernel", false),
    ("/devices/null", true),
    ("/devices/zero", false),
    ("/devices/random", false),
    ("/devices/urandom", false),
];

/// Where Spark keeps its own files: no service holds them, or what is
/// in them, the secrets among them.
pub const SPARK_STATE: &str = "/system/state/spark";

/// Where Spark keeps the secrets of service `name`, and where the
/// service finds them, as `SECRETS_DIRECTORY` says.
pub fn secrets_dir(name: &str) -> String {
    format!("{SPARK_STATE}/secrets/{name}")
}

/// The most places `files.mount` names: they are descriptors 3 and on.
pub const MOUNT_MAX: usize = 4;

/// The path of the folder `name` of `/shared`.
fn shared(name: &str) -> String {
    format!("/shared/{name}")
}

/// Whether `inner` is `outer` or beneath it.
fn within(inner: &str, outer: &str) -> bool {
    inner.strip_prefix(outer).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The `permissions` table as written, before the checks of the whole.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermissions {
    #[serde(default)]
    network: Network,
    #[serde(default)]
    files: Files,
    #[serde(default, rename = "runs-with-your-access")]
    runs_with_your_access: bool,
    #[serde(default)]
    admin: Admin,
    #[serde(default)]
    names: bool,
}

impl TryFrom<RawPermissions> for Permissions {
    type Error = String;

    fn try_from(raw: RawPermissions) -> Result<Permissions, String> {
        let permissions = Permissions {
            network: raw.network,
            files: raw.files,
            runs_with_your_access: raw.runs_with_your_access,
            admin: raw.admin,
            names: raw.names,
        };
        let grants = permissions.network.grants() + usize::from(permissions.admin.network);
        if grants > NETWORK_SLOTS as usize {
            return Err(format!(
                "it asks for {grants} network capabilities, one to connect out, one for each \
                 range of ports it listens on and one for admin.network; a program gets at most \
                 {NETWORK_SLOTS}"
            ));
        }
        let places = permissions.places();
        if permissions.runs_with_your_access && !places.is_empty() {
            return Err("runs-with-your-access holds every file already: leave out files.*".into());
        }
        if permissions.files.mount.len() > MOUNT_MAX {
            return Err(format!("files.mount names at most {MOUNT_MAX} places"));
        }
        for (i, place) in places.iter().enumerate() {
            let path = place.path.as_str();
            if let Some(other) =
                places[..i].iter().find(|o| within(path, &o.path) || within(&o.path, path))
            {
                return Err(if path == other.path {
                    format!("files names {path} twice")
                } else {
                    format!("files names {path} and {}, one within the other: name one", other.path)
                });
            }
            if within(path, SPARK_STATE) || within(SPARK_STATE, path) {
                return Err(format!(
                    "no service holds {SPARK_STATE}, where Spark keeps its files and the \
                     services' secrets, so it cannot hold {path}"
                ));
            }
            if let Some((always, _)) = ALWAYS.iter().find(|(a, _)| path != *a && within(path, a)) {
                return Err(format!(
                    "every service reads {always} already, so its view cannot hold {path} \
                     apart; name {always} with files.read-write to change what is in it"
                ));
            }
        }
        Ok(permissions)
    }
}

/// The network group: `network.outbound` and `network.listen`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawNetwork")]
pub struct Network {
    /// Whether it may connect out, to any address and port.
    pub outbound: bool,
    /// The ranges of ports it may listen on, none overlapping.
    pub listen: Vec<Ports>,
}

impl Network {
    fn grants(&self) -> usize {
        usize::from(self.outbound) + self.listen.len()
    }
}

/// A range of ports, from `first` to `last`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ports {
    /// The first port.
    pub first: u16,
    /// The last port, `first` for a range of one.
    pub last: u16,
}

impl Ports {
    /// How many ports it holds.
    pub fn count(self) -> u32 {
        u32::from(self.last) - u32::from(self.first) + 1
    }
}

impl fmt::Display for Ports {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.first == self.last {
            write!(f, "{}", self.first)
        } else {
            write!(f, "{}-{}", self.first, self.last)
        }
    }
}

/// The file `service new NAME` writes: every setting, explained, for a
/// service that is not started at boot until it is ready. `NAME` stands
/// for the service's name.
const TEMPLATE: &str = r#"# The NAME service. service.toml(5) explains each setting. Check this file
# with `service check NAME`; `service edit NAME` checks it as you save.
# Start the service with `service NAME start`.

service_version = 1
description = "What NAME does, in a few words"
# The program, by its absolute path, and the arguments after its name.
program = "/system/commands/true"
arguments = []
# Lower starts first at boot, and stops last; the system's own services
# take 5 to 50.
order = 60
# Whether Spark starts it at boot: true once it is ready.
enabled = false
# When it is started again after it ends: always, on-failure or never.
restart = "on-failure"
# The secrets Spark hands it as read-only files, in the folder
# SECRETS_DIRECTORY names, never in its environment; set each with
# `service secret set NAME SECRET`.
secrets = []

# More of its environment, as NAME = "value".
[environment]

# What its domain may use: a CPU weight from 1 to 10000, where 100 is
# everyone's; a memory limit such as "64M", or "none"; and "service" or
# "app", which the out-of-memory policy stops first.
[limits]
cpu_weight = 100
memory = "none"
oom = "service"

# What it may do beyond running: connect out to the network, listen on
# ports and ranges of ports, such as 80 or "8000-8009", set the network
# interfaces up, and set the machine's fans, LEDs and ARGB header.
[permissions]
network.outbound = false
network.listen = []
# The places it may read, and read and change, beyond the system's
# programs, libraries, resources and settings, which every service reads;
# the places it mounts filesystems beneath; and folders of /shared, by
# name. It sees nothing else of the tree.
files.read = []
files.read-write = []
files.mount = []
files.shared = []
# The whole tree as Spark holds it, in place of files: only for a service
# that starts other people's shells.
runs-with-your-access = false
# Whether it may set the network interfaces' addresses and routers, as
# ip(8) does.
admin.network = false
# Whether it may set the machine's fans, LEDs and ARGB header.
admin.hardware = false
# Giving processes and files another user's name: only for authd, which
# signs people in.
names = false
"#;

/// The file `service new` writes for the service `name`.
pub fn template(name: &str) -> String {
    TEMPLATE.replace("NAME", name)
}

/// Reads a service file.
pub fn from_toml(text: &str) -> Result<Service, Error> {
    toml::from_str(text).map_err(|err| Error::from_toml(text, &err))
}

/// Checks a service's name, the name of its file without `.toml`: it
/// names its resource domain and its log lines too.
pub fn check_name(name: &str) -> Result<(), String> {
    let fits = (1..=NAME_MAX as usize).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && !name.starts_with(['.', '-']);
    if fits {
        Ok(())
    } else {
        Err(format!(
            "`{name}` is not a service's name: it has 1 to {NAME_MAX} letters, digits, `-`, `_` \
             and `.`, and starts with a letter, a digit or `_`"
        ))
    }
}

/// A service file as it is written, before the checks that need more than
/// one setting.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    service_version: Option<Version>,
    #[serde(default)]
    description: String,
    program: Option<Program>,
    #[serde(default)]
    arguments: Vec<String>,
    #[serde(default)]
    environment: Environment,
    #[serde(default = "order_default")]
    order: u32,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default, deserialize_with = "restart")]
    restart: Restart,
    #[serde(default)]
    limits: Limits,
    #[serde(default, deserialize_with = "secret_names")]
    secrets: Vec<String>,
    #[serde(default)]
    permissions: Permissions,
}

impl TryFrom<File> for Service {
    type Error = String;

    fn try_from(file: File) -> Result<Service, String> {
        if file.service_version.is_none() {
            return Err(format!("it has no service_version: add `service_version = {VERSION}`"));
        }
        let Some(Program(program)) = file.program else {
            return Err("it names no program: add a line such as \
                        `program = \"/system/commands/logd\"`"
                .into());
        };
        if let Some(bad) = file.arguments.iter().find(|a| a.contains('\0')) {
            return Err(format!("the argument `{}` holds a NUL", bad.escape_debug()));
        }
        Ok(Service {
            description: file.description,
            program,
            arguments: file.arguments,
            environment: file.environment.0,
            order: file.order,
            enabled: file.enabled,
            restart: file.restart,
            limits: file.limits,
            secrets: file.secrets,
            permissions: file.permissions,
        })
    }
}

fn order_default() -> u32 {
    ORDER_DEFAULT
}

fn yes() -> bool {
    true
}

fn weight_default() -> u32 {
    WEIGHT_DEFAULT
}

/// `service_version`: a version this reads.
struct Version;

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Version, D::Error> {
        match whole(d, "service_version")? {
            1 => Ok(Version),
            n if n > u64::from(VERSION) => Err(de::Error::custom(format!(
                "this file is for service_version {n}, and this Dukovo reads version {VERSION} \
                 and earlier: update Dukovo to use it"
            ))),
            _ => Err(de::Error::custom(format!("service_version is {VERSION}"))),
        }
    }
}

/// `program`: an absolute path.
struct Program(String);

impl<'de> Deserialize<'de> for Program {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Program, D::Error> {
        let path = String::deserialize(d)?;
        if !path.starts_with('/') || path.contains('\0') {
            return Err(de::Error::custom(format!(
                "program is an absolute path, such as /system/commands/logd, not `{}`",
                path.escape_debug()
            )));
        }
        Ok(Program(path))
    }
}

/// `environment`: a table of names and values, in the order of their
/// names.
#[derive(Default)]
struct Environment(Vec<(String, String)>);

impl<'de> Deserialize<'de> for Environment {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Environment, D::Error> {
        struct Table;
        impl<'de> Visitor<'de> for Table {
            type Value = Environment;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a table of names and values, such as `TZ = \"UTC\"`")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Environment, A::Error> {
                let mut variables = Vec::new();
                while let Some((Variable(name), value)) = map.next_entry::<Variable, String>()? {
                    if value.contains('\0') {
                        return Err(de::Error::custom(format!("{name}'s value holds a NUL")));
                    }
                    variables.push((name, value));
                }
                Ok(Environment(variables))
            }
        }
        d.deserialize_map(Table)
    }
}

/// A variable's name in `environment`: neither empty nor holding `=`.
struct Variable(String);

impl<'de> Deserialize<'de> for Variable {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Variable, D::Error> {
        let name = String::deserialize(d)?;
        if name.is_empty() || name.contains(['=', '\0']) {
            return Err(de::Error::custom(format!(
                "`{}` cannot be a variable's name",
                name.escape_debug()
            )));
        }
        Ok(Variable(name))
    }
}

/// A whole number, for `key`.
fn whole<'de, D: Deserializer<'de>>(d: D, key: &'static str) -> Result<u64, D::Error> {
    struct Whole(&'static str);
    impl Visitor<'_> for Whole {
        type Value = u64;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "a whole number for {}", self.0)
        }

        fn visit_u64<E: de::Error>(self, n: u64) -> Result<u64, E> {
            Ok(n)
        }

        fn visit_i64<E: de::Error>(self, n: i64) -> Result<u64, E> {
            u64::try_from(n).map_err(|_| E::custom(format!("{} is not below 0", self.0)))
        }
    }
    d.deserialize_any(Whole(key))
}

/// One of `words`, for `key`: its index.
fn word<'de, D: Deserializer<'de>>(
    d: D,
    key: &'static str,
    words: &'static [&'static str],
) -> Result<usize, D::Error> {
    struct Word(&'static str, &'static [&'static str]);
    impl Word {
        fn list(&self) -> String {
            match self.1 {
                [] => String::new(),
                [one] => (*one).to_owned(),
                [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
            }
        }
    }
    impl Visitor<'_> for Word {
        type Value = usize;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{} for {}", self.list(), self.0)
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<usize, E> {
            self.1.iter().position(|w| *w == value).ok_or_else(|| {
                E::custom(format!("{} must be {}, not `{}`", self.0, self.list(), value))
            })
        }
    }
    d.deserialize_str(Word(key, words))
}

fn restart<'de, D: Deserializer<'de>>(d: D) -> Result<Restart, D::Error> {
    Ok([Restart::Always, Restart::OnFailure, Restart::Never][word(d, "restart", Restart::WORDS)?])
}

fn oom<'de, D: Deserializer<'de>>(d: D) -> Result<Oom, D::Error> {
    Ok([Oom::Service, Oom::App][word(d, "oom", Oom::WORDS)?])
}

fn weight<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    match whole(d, "cpu_weight")? {
        n @ 1.. if n <= u64::from(WEIGHT_MAX) => Ok(n as u32),
        n => Err(de::Error::custom(format!("cpu_weight is from 1 to {WEIGHT_MAX}, not {n}"))),
    }
}

/// `memory`: a size such as `"64M"`, a number of bytes, or `"none"`.
fn memory<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    struct Memory;
    impl Visitor<'_> for Memory {
        type Value = Option<u64>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a size such as \"64M\", or \"none\", for memory")
        }

        fn visit_u64<E: de::Error>(self, bytes: u64) -> Result<Option<u64>, E> {
            match bytes {
                0 => Err(E::custom("memory is more than 0 bytes; \"none\" sets no limit")),
                bytes => Ok(Some(bytes)),
            }
        }

        fn visit_i64<E: de::Error>(self, bytes: i64) -> Result<Option<u64>, E> {
            let bytes = u64::try_from(bytes).map_err(|_| E::custom("memory is not below 0"))?;
            self.visit_u64(bytes)
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Option<u64>, E> {
            match size(value) {
                Some(0) => self.visit_u64(0),
                Some(bytes) => Ok(Some(bytes)),
                None if value == "none" => Ok(None),
                None => Err(E::custom(format!(
                    "memory is a size such as \"64M\", \"512K\" or \"2G\", or \"none\", not `{value}`"
                ))),
            }
        }
    }
    d.deserialize_any(Memory)
}

/// `files.read`, `files.read-write` and `files.mount`: absolute paths of
/// places beneath `/`, each written once, without `.`, `..` or an empty
/// part.
fn places<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let paths = Vec::<String>::deserialize(d)?;
    for path in &paths {
        let parts = path.strip_prefix('/').map(|p| p.split('/'));
        let fits = parts.is_some_and(|mut parts| {
            parts
                .all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains('\0'))
        });
        if path == "/" {
            return Err(de::Error::custom(
                "files holds places beneath /, not / itself: runs-with-your-access gives a \
                 service the whole tree",
            ));
        }
        if !fits {
            return Err(de::Error::custom(format!(
                "files takes absolute paths without `.`, `..` or `//`, such as /system/state/log, \
                 not `{}`",
                path.escape_debug()
            )));
        }
    }
    Ok(paths)
}

/// `files.shared`: names of folders of `/shared`.
fn folders<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let names = Vec::<String>::deserialize(d)?;
    for name in &names {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
            return Err(de::Error::custom(format!(
                "files.shared takes names of folders in /shared, such as \"media\", not `{}`",
                name.escape_debug()
            )));
        }
    }
    Ok(names)
}

/// `secrets`: names of secrets, each as a service's name is written,
/// and each once.
fn secret_names<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let names = Vec::<String>::deserialize(d)?;
    for (i, name) in names.iter().enumerate() {
        check_secret(name).map_err(de::Error::custom)?;
        if names[..i].contains(name) {
            return Err(de::Error::custom(format!("secrets names {name} twice")));
        }
    }
    Ok(names)
}

/// Checks a secret's name, the name of its file in [`secrets_dir`].
pub fn check_secret(name: &str) -> Result<(), String> {
    let fits = (1..=NAME_MAX as usize).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && !name.starts_with(['.', '-']);
    if fits {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not a secret's name: it has 1 to {NAME_MAX} letters, digits, `-`, `_` and \
             `.`, and starts with a letter, a digit or `_`",
            name.escape_debug()
        ))
    }
}

/// The bytes a size such as `64M`, `512K`, `2G` or `4096` says.
pub fn size(value: &str) -> Option<u64> {
    let at = value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len());
    let (digits, unit) = value.split_at(at);
    let scale: u64 = match unit {
        "" => 1,
        "K" | "k" | "KiB" => 1 << 10,
        "M" | "m" | "MiB" => 1 << 20,
        "G" | "g" | "GiB" => 1 << 30,
        _ => return None,
    };
    digits.parse::<u64>().ok()?.checked_mul(scale)
}

/// `network`'s table as written, before the checks of the whole.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetwork {
    #[serde(default)]
    outbound: bool,
    #[serde(default)]
    listen: Vec<Ports>,
}

impl TryFrom<RawNetwork> for Network {
    type Error = String;

    fn try_from(raw: RawNetwork) -> Result<Network, String> {
        for (i, a) in raw.listen.iter().enumerate() {
            if let Some(b) = raw.listen[..i].iter().find(|b| a.first <= b.last && b.first <= a.last)
            {
                return Err(format!("network.listen: {a} overlaps {b}"));
            }
        }
        let network = Network { outbound: raw.outbound, listen: raw.listen };
        if network.grants() > NETWORK_SLOTS as usize {
            return Err(format!(
                "it asks for {} network capabilities, one to connect out and one for each \
                 range of ports it listens on; a program gets at most {NETWORK_SLOTS}",
                network.grants()
            ));
        }
        Ok(network)
    }
}

impl<'de> Deserialize<'de> for Ports {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Ports, D::Error> {
        struct Item;
        impl Item {
            fn port<E: de::Error>(text: &str) -> Result<u16, E> {
                match text.trim().parse::<u16>() {
                    Ok(port @ 1..) => Ok(port),
                    _ => Err(E::custom(format!(
                        "network.listen takes ports from 1 to 65535 and ranges such as \
                         \"8000-8009\", not `{text}`"
                    ))),
                }
            }
        }
        impl Visitor<'_> for Item {
            type Value = Ports;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a port, or a range such as \"8000-8009\"")
            }

            fn visit_i64<E: de::Error>(self, port: i64) -> Result<Ports, E> {
                let port = Item::port(&format!("{port}"))?;
                Ok(Ports { first: port, last: port })
            }

            fn visit_u64<E: de::Error>(self, port: u64) -> Result<Ports, E> {
                let port = Item::port(&format!("{port}"))?;
                Ok(Ports { first: port, last: port })
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Ports, E> {
                let (first, last) = match value.split_once('-') {
                    Some((first, last)) => (Item::port(first)?, Item::port(last)?),
                    None => (Item::port(value)?, Item::port(value)?),
                };
                if last < first {
                    return Err(E::custom(format!("the range \"{value}\" ends before it starts")));
                }
                Ok(Ports { first, last })
            }
        }
        d.deserialize_any(Item)
    }
}
