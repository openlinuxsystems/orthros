// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bundle manifests (manifest.json(5), ADR 0221): what an app or service
//! bundle is, what it runs and what it asks for, in the `manifest.json`
//! at the top of its `.dkv` file.
//!
//! Version 1 holds what Dukovo 1.0 uses: the bundle's identity, its
//! programs, commands and services, the permissions it asks for, the data
//! it keeps, the interfaces it provides and uses, and its update feed. It
//! is a preview until 1.0.0, when it freezes, and then only grows: a later
//! version adds fields and never changes what one means.
//!
//! The reading is strict. An unknown field is an error, but for `x-`
//! fields, which are a vendor's own and which Dukovo ignores, and
//! `$schema`, which editors read. The names of what later versions bring
//! (`entries`, `actions`, `display`, `runtime`, `plugins`) are reserved,
//! and so are the permissions later releases define. A manifest for a
//! later version is refused with a message to update Dukovo. Each error
//! says its line and what is wrong, in plain words.
//!
//! The same types make the JSON Schema (`bundle schema`), so the two
//! cannot drift apart.

mod locate;
pub mod permissions;
pub mod spdx;

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use serde::Deserialize;
use serde::de::{self, DeserializeOwned, Deserializer, IgnoredAny, MapAccess, Visitor};

use self::locate::{Step, locate};
use self::permissions::{PERSONAL_FOLDERS, Takes};
use crate::service::{self, Oom, Ports, Restart};
use crate::{Error, line_of};

#[cfg(feature = "schema")]
use alloc::borrow::Cow;
#[cfg(feature = "schema")]
use radix_abi::resource_domain::WEIGHT_MAX;
#[cfg(feature = "schema")]
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

/// The version of the format this reads, the newest there is.
pub const VERSION: u32 = 1;

/// Every field of a manifest, in the order manifest.json(5) describes
/// them.
pub const KEYS: &[&str] = &[
    "manifest_version",
    "id",
    "name",
    "summary",
    "description",
    "version",
    "publisher",
    "license",
    "source",
    "homepage",
    "icon",
    "requires_os",
    "architectures",
    "programs",
    "commands",
    "services",
    "permissions",
    "data",
    "provides",
    "uses",
    "update",
    "translations",
];

/// The fields a manifest must have.
const REQUIRED: &[&str] = &[
    "manifest_version",
    "id",
    "name",
    "summary",
    "version",
    "publisher",
    "license",
    "requires_os",
    "architectures",
    "programs",
];

/// The names later manifest versions define: what the desktop's menus
/// (`entries`, `actions`), windows (`display`), the Linux layer
/// (`runtime`) and plugins bring.
pub const RESERVED: &[&str] = &["entries", "actions", "display", "runtime", "plugins"];

/// The architectures a bundle may carry programs for.
pub const ARCHITECTURES: &[&str] = &["x86_64", "aarch64", "riscv64", "wasm"];

/// The highest `open_files`: Linux's `fs.nr_open`. A process holds no
/// more descriptors than its capability space has slots for, whatever
/// its limit (radix-posix(7)), so a program written for Linux may ask
/// for as many as it does there.
pub const OPEN_FILES_MAX: u32 = 1 << 20;

/// A bundle's manifest, read and checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// Its ID: reverse DNS, lowercase, such as `org.nginx.nginx`.
    pub id: Id,
    /// What people call it.
    pub name: Line,
    /// What it is, in one line.
    pub summary: Line,
    /// More about it, in paragraphs.
    pub description: Option<Text>,
    /// Its version.
    pub version: Version,
    /// Who publishes it; the key comes from its signature.
    pub publisher: Publisher,
    /// Its license, as an SPDX expression.
    pub license: License,
    /// Where its exact source is, which a GNU license needs.
    pub source: Option<Url>,
    /// Its web site.
    pub homepage: Option<Url>,
    /// Its icon: an SVG file in the bundle.
    pub icon: Option<BundlePath>,
    /// The first Dukovo release it runs on.
    pub requires_os: OsVersion,
    /// The architectures it carries programs for.
    pub architectures: Vec<Arch>,
    /// Its programs, by name.
    pub programs: Map<Name, Program>,
    /// The commands it adds to the terminal.
    pub commands: Vec<Command>,
    /// The services it runs.
    pub services: Vec<Service>,
    /// The permissions it asks for, by name.
    pub permissions: Permissions,
    /// What it keeps in its data folder.
    pub data: Data,
    /// The interfaces and sockets it offers other bundles.
    pub provides: Vec<Provide>,
    /// The interfaces of other bundles and of the system it talks to.
    pub uses: Vec<Use>,
    /// Its own update feed, for a bundle outside a catalog.
    pub update: Option<Update>,
    /// Its texts in other languages, by language.
    pub translations: Map<Language, Translation>,
}

/// Reads and checks the manifest `text`: every error found, each with its
/// line where it has one.
pub fn from_json(text: &str) -> Result<Manifest, Vec<Error>> {
    // The version first, so that a later one is refused for what it is
    // and not for the first field this version does not know.
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| vec![shown(json_error(&err))])?;
    let Some(object) = value.as_object() else {
        return Err(vec![Error::at(1, "a manifest is an object, between { and }")]);
    };
    match object.get("manifest_version") {
        None => {
            return Err(vec![Error::new(format!(
                "it has no manifest_version: add \"manifest_version\": {VERSION}"
            ))]);
        }
        Some(serde_json::Value::Number(n))
            if n.as_u64().is_some_and(|v| v > u64::from(VERSION)) =>
        {
            return Err(vec![Error::at(
                at(text, &[Step::Key("manifest_version")]),
                format!(
                    "this bundle is for manifest version {n}, and this Dukovo reads version \
                     {VERSION}: update Dukovo to install it"
                ),
            )]);
        }
        Some(serde_json::Value::Number(n)) if n.as_u64() == Some(u64::from(VERSION)) => {}
        Some(other) => {
            return Err(vec![Error::at(
                at(text, &[Step::Key("manifest_version")]),
                format!("manifest_version is {VERSION}, not {other}"),
            )]);
        }
    }
    let (manifest, wrong) =
        match serde_json::from_str(text).map_err(|err| vec![shown(json_error(&err))])? {
            Top::Whole(manifest, wrong) => (*manifest, wrong),
            Top::Missing(key) => {
                return Err(vec![Error::new(format!(
                    "it has no {key}; manifest.json(5) says what it holds"
                ))]);
            }
        };
    let mut errors: Vec<Error> = wrong
        .iter()
        .map(|(name, why)| {
            Error::at(at(text, &[Step::Key("permissions"), Step::Key(name)]), why.clone())
        })
        .collect();
    let failed: Vec<&str> = wrong.iter().map(|(name, _)| name.as_str()).collect();
    errors.extend(manifest.problems(text, &failed));
    if errors.is_empty() { Ok(manifest) } else { Err(errors.into_iter().map(shown).collect()) }
}

/// `error` with each control character its message quotes from the
/// manifest written as an escape, so that a manifest cannot write to the
/// terminal that `bundle check` prints to.
fn shown(mut error: Error) -> Error {
    if error.message.chars().any(char::is_control) {
        error.message = error
            .message
            .chars()
            .map(|c| if c.is_control() { c.escape_debug().to_string() } else { c.to_string() })
            .collect();
    }
    error
}

/// The error serde_json found, with its line, in plain words.
fn json_error(err: &serde_json::Error) -> Error {
    let mut message = err.to_string();
    // serde_json ends its messages with where it was.
    if let Some(cut) = message.rfind(" at line ") {
        message.truncate(cut);
    }
    let message = crate::plain(&message)
        .replace("is not a setting here; service.toml(5)", "is not a field here; manifest.json(5)")
        .replace("expected a table", "expected an object")
        .replace("expected value", "expected a value")
        .replace("expected struct ", "expected an object for ")
        .replace("invalid type: ", "");
    match err.line() {
        0 => Error::new(message),
        line => Error { line: Some(line), message },
    }
}

/// The line of what `path` leads to in `text`, or the first line.
fn at(text: &str, path: &[Step<'_>]) -> usize {
    locate(text, path).map_or(1, |offset| line_of(text, offset))
}

impl Manifest {
    /// The checks that need more than one field: each problem, with the
    /// line of the field it is about. The permissions named in `failed`
    /// were asked for, but wrongly, which is said already.
    fn problems(&self, text: &str, failed: &[&str]) -> Vec<Error> {
        let asks = |name: &str| self.permissions.get(name).is_some() || failed.contains(&name);
        let mut errors = Vec::new();
        let mut fail = |path: &[Step<'_>], message: String| {
            errors.push(Error::at(at(text, path), message));
        };
        use Step::{Index, Key};

        // The architectures, and a program for each.
        if self.architectures.is_empty() {
            fail(
                &[Key("architectures")],
                "architectures names at least one, such as x86_64".into(),
            );
        }
        for (i, arch) in self.architectures.iter().enumerate() {
            if self.architectures[..i].contains(arch) {
                fail(
                    &[Key("architectures"), Index(i)],
                    format!("architectures names {arch} twice"),
                );
            }
        }
        if self.programs.is_empty() {
            fail(&[Key("programs")], "programs names at least one program".into());
        }
        for (name, program) in self.programs.iter() {
            let here = [Key("programs"), Key(&name.0), Key("path")];
            for arch in &self.architectures {
                if program.path.get(arch).is_none() {
                    fail(&here, format!("program {name} has no path for {arch}"));
                }
            }
            for (arch, _) in program.path.iter() {
                if !self.architectures.contains(arch) {
                    fail(
                        &here,
                        format!(
                            "program {name} has a path for {arch}, which architectures does not \
                             name"
                        ),
                    );
                }
            }
            for (i, permission) in program.permissions.iter().enumerate() {
                if !asks(permission) {
                    fail(
                        &[Key("programs"), Key(&name.0), Key("permissions"), Index(i)],
                        format!(
                            "program {name} narrows itself to {permission}, which the bundle \
                             does not ask for in permissions"
                        ),
                    );
                }
            }
        }

        // Commands and services run programs it has, under names of their
        // own.
        for (i, command) in self.commands.iter().enumerate() {
            if self.commands[..i].iter().any(|c| c.name == command.name) {
                fail(
                    &[Key("commands"), Index(i)],
                    format!("command {} is named twice", command.name),
                );
            }
            if self.programs.get(&command.program).is_none() {
                fail(
                    &[Key("commands"), Index(i), Key("program")],
                    format!(
                        "command {} runs {}, which programs does not name",
                        command.name, command.program
                    ),
                );
            }
        }
        let sockets: Vec<&Name> = self.provides.iter().filter_map(|p| p.socket.as_ref()).collect();
        for (i, svc) in self.services.iter().enumerate() {
            if self.services[..i].iter().any(|s| s.name == svc.name) {
                fail(&[Key("services"), Index(i)], format!("service {} is named twice", svc.name));
            }
            if self.programs.get(&svc.program).is_none() {
                fail(
                    &[Key("services"), Index(i), Key("program")],
                    format!(
                        "service {} runs {}, which programs does not name",
                        svc.name, svc.program
                    ),
                );
            }
            if svc.account.is_none() && check_account(&svc.name.0).is_err() {
                fail(
                    &[Key("services"), Index(i), Key("name")],
                    format!(
                        "service {} runs as an account of its name unless it names one, and \
                         {} cannot be an account's: add \"account\", such as \"{}\"",
                        svc.name,
                        svc.name,
                        account_like(&svc.name.0)
                    ),
                );
            }
            if svc.start == Start::OnConnection && sockets.is_empty() {
                fail(
                    &[Key("services"), Index(i), Key("start")],
                    format!(
                        "service {} starts on a connection, but provides names no socket to \
                         connect to",
                        svc.name
                    ),
                );
            }
            for (j, need) in svc.needs.iter().enumerate() {
                if svc.needs[..j].contains(need) {
                    fail(
                        &[Key("services"), Index(i), Key("needs"), Index(j)],
                        format!("service {} needs {need} twice", svc.name),
                    );
                }
            }
            if let Some(health) = &svc.health
                && !sockets.contains(&&health.socket)
            {
                fail(
                    &[Key("services"), Index(i), Key("health")],
                    format!(
                        "service {} is checked through the socket {}, which provides does not \
                         name",
                        svc.name, health.socket
                    ),
                );
            }
        }
        for (i, provide) in self.provides.iter().enumerate() {
            let same = |p: &Provide| p.socket == provide.socket && p.interface == provide.interface;
            if self.provides[..i].iter().any(same) {
                fail(&[Key("provides"), Index(i)], "provides names it twice".into());
            }
        }

        for (i, used) in self.uses.iter().enumerate() {
            if self.uses[..i].iter().any(|u| u.interface == used.interface) {
                fail(&[Key("uses"), Index(i)], format!("uses names {} twice", used.interface));
            }
        }

        // The license, and the source a GNU license needs.
        let ids = spdx::check(&self.license.0).unwrap_or_default();
        if self.source.is_none()
            && let Some(gnu) = ids.iter().find(|id| spdx::needs_source(id))
        {
            fail(
                &[Key("license")],
                format!(
                    "a bundle under {gnu} links to its exact source: add \"source\", such as \
                     \"https://example.org/source/app-1.0.tar.xz\""
                ),
            );
        }
        if let Some(icon) = &self.icon
            && !icon.0.rsplit_once('.').is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("svg"))
        {
            fail(&[Key("icon")], format!("icon is an SVG file in the bundle, not {}", icon.0));
        }

        // Translations explain only what it asks for.
        for (language, translation) in self.translations.iter() {
            for (permission, _) in translation.why.iter() {
                if !asks(permission) {
                    fail(
                        &[Key("translations"), Key(&language.0), Key("why"), Key(permission)],
                        format!(
                            "the {language} translation explains {permission}, which the bundle \
                             does not ask for"
                        ),
                    );
                }
            }
        }
        errors
    }

    /// What `bundle check` points out without refusing it.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        for (name, request) in self.permissions.iter() {
            if request.why.is_none() {
                notes.push(format!(
                    "{name} has no `why`: people see only the system's words for it, not why \
                     {} asks",
                    self.name
                ));
            }
        }
        notes
    }
}

/// The manifest's object, read field by field: an unknown field is an
/// error, an `x-` field and `$schema` are skipped, and each field is
/// written once. A missing field is said without a line, which the end
/// of the object would be.
enum Top {
    /// The manifest, and each permission whose fields were wrong for it,
    /// with why: left out of the manifest, and said with its line.
    Whole(Box<Manifest>, Vec<(String, String)>),
    Missing(&'static str),
}

impl<'de> Deserialize<'de> for Top {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Top, D::Error> {
        struct Fields;

        impl<'de> Visitor<'de> for Fields {
            type Value = Top;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object, the manifest")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Top, A::Error> {
                let mut seen = BTreeSet::new();
                let mut id = None;
                let mut name = None;
                let mut summary = None;
                let mut description = None;
                let mut version = None;
                let mut publisher = None;
                let mut license = None;
                let mut source = None;
                let mut homepage = None;
                let mut icon = None;
                let mut requires_os = None;
                let mut architectures = None;
                let mut programs = None;
                let mut commands = None;
                let mut services = None;
                let mut permissions = None;
                let mut data = None;
                let mut provides = None;
                let mut uses = None;
                let mut update = None;
                let mut translations = None;
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(de::Error::custom(format!("{key} is written twice")));
                    }
                    match key.as_str() {
                        "manifest_version" => {
                            map.next_value::<IgnoredAny>()?;
                        }
                        "$schema" => {
                            map.next_value::<String>()?;
                        }
                        x if x.starts_with("x-") => {
                            map.next_value::<IgnoredAny>()?;
                        }
                        "id" => id = Some(map.next_value()?),
                        "name" => name = Some(map.next_value()?),
                        "summary" => summary = Some(map.next_value()?),
                        "description" => description = Some(map.next_value()?),
                        "version" => version = Some(map.next_value()?),
                        "publisher" => publisher = Some(map.next_value()?),
                        "license" => license = Some(map.next_value()?),
                        "source" => source = Some(map.next_value()?),
                        "homepage" => homepage = Some(map.next_value()?),
                        "icon" => icon = Some(map.next_value()?),
                        "requires_os" => requires_os = Some(map.next_value()?),
                        "architectures" => architectures = Some(map.next_value()?),
                        "programs" => programs = Some(map.next_value()?),
                        "commands" => commands = Some(map.next_value()?),
                        "services" => services = Some(map.next_value()?),
                        "permissions" => permissions = Some(map.next_value::<RawPermissions>()?),
                        "data" => data = Some(map.next_value()?),
                        "provides" => provides = Some(map.next_value()?),
                        "uses" => uses = Some(map.next_value()?),
                        "update" => update = Some(map.next_value()?),
                        "translations" => translations = Some(map.next_value()?),
                        reserved if RESERVED.contains(&reserved) => {
                            return Err(de::Error::custom(format!(
                                "{reserved} comes in a later manifest version: version \
                                 {VERSION} reserves its name"
                            )));
                        }
                        other => {
                            return Err(de::Error::custom(format!(
                                "{other} is not a field of a manifest; manifest.json(5) lists \
                                 them, and a vendor's own fields start with x-"
                            )));
                        }
                    }
                }
                let (Some(id), Some(name), Some(summary), Some(version), Some(publisher)) =
                    (id, name, summary, version, publisher)
                else {
                    return Ok(Top::Missing(missing(&seen)));
                };
                let (Some(license), Some(requires_os), Some(architectures), Some(programs)) =
                    (license, requires_os, architectures, programs)
                else {
                    return Ok(Top::Missing(missing(&seen)));
                };
                if !seen.contains("manifest_version") {
                    return Ok(Top::Missing("manifest_version"));
                }
                let mut asked = Vec::new();
                let mut wrong = Vec::new();
                for (name, raw) in permissions.into_iter().flat_map(|p| p.0) {
                    let takes = permissions::find(&name).map_or(Takes::Nothing, |p| p.takes);
                    match request(&name, takes, raw) {
                        Ok(request) => asked.push((name, request)),
                        Err(why) => wrong.push((name, why)),
                    }
                }
                Ok(Top::Whole(
                    Box::new(Manifest {
                        id,
                        name,
                        summary,
                        description,
                        version,
                        publisher,
                        license,
                        source,
                        homepage,
                        icon,
                        requires_os,
                        architectures,
                        programs,
                        commands: commands.unwrap_or_default(),
                        services: services.unwrap_or_default(),
                        permissions: Permissions(asked),
                        data: data.unwrap_or_default(),
                        provides: provides.unwrap_or_default(),
                        uses: uses.unwrap_or_default(),
                        update,
                        translations: translations.unwrap_or_default(),
                    }),
                    wrong,
                ))
            }
        }

        d.deserialize_map(Fields)
    }
}

/// The first field [`REQUIRED`] names that `seen` lacks.
fn missing(seen: &BTreeSet<String>) -> &'static str {
    REQUIRED.iter().find(|key| !seen.contains(**key)).copied().unwrap_or("manifest_version")
}

/// An object whose members' names are `K`, each once, in the order
/// written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Map<K, V>(pub Vec<(K, V)>);

impl<K, V> Default for Map<K, V> {
    fn default() -> Self {
        Map(Vec::new())
    }
}

impl<K: PartialEq, V> Map<K, V> {
    /// The value of member `key`.
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: PartialEq<Q>,
        Q: ?Sized,
    {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The members, in the order written.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.0.iter().map(|(k, v)| (k, v))
    }

    /// How many members it has.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it has none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de, K, V> Deserialize<'de> for Map<K, V>
where
    K: DeserializeOwned + PartialEq + fmt::Display,
    V: DeserializeOwned,
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Members<K, V>(PhantomData<(K, V)>);

        impl<'de, K, V> Visitor<'de> for Members<K, V>
        where
            K: DeserializeOwned + PartialEq + fmt::Display,
            V: DeserializeOwned,
        {
            type Value = Map<K, V>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Map<K, V>, A::Error> {
                let mut members: Vec<(K, V)> = Vec::new();
                while let Some(key) = map.next_key::<K>()? {
                    if members.iter().any(|(k, _)| *k == key) {
                        return Err(de::Error::custom(format!("{key} is written twice")));
                    }
                    let value = map.next_value()?;
                    members.push((key, value));
                }
                Ok(Map(members))
            }
        }

        d.deserialize_map(Members(PhantomData))
    }
}

#[cfg(feature = "schema")]
impl<K: JsonSchema, V: JsonSchema> JsonSchema for Map<K, V> {
    fn schema_name() -> Cow<'static, str> {
        format!("Map_of_{}", V::schema_name()).into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let names = generator.subschema_for::<K>();
        let values = generator.subschema_for::<V>();
        json_schema!({"type": "object", "propertyNames": names, "additionalProperties": values})
    }
}

/// Defines a text field's type, which checks what it holds as it is read
/// with `check`, and whose schema is a string described by `what`, with
/// `pattern` when one is given.
macro_rules! text {
    ($(#[$doc:meta])* $name:ident, $what:literal, $(@pattern $pattern:literal,)? $check:expr) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
        pub struct $name(pub String);

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<$name, D::Error> {
                let text = String::deserialize(d)?;
                let check: fn(&str) -> Result<(), String> = $check;
                check(&text).map_err(de::Error::custom)?;
                Ok($name(text))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                self.0 == *other
            }
        }

        #[cfg(feature = "schema")]
        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                #[allow(unused_mut, reason = "a text without a pattern changes nothing")]
                let mut schema = json_schema!({"type": "string", "description": $what});
                $(schema.insert("pattern".into(), $pattern.into());)?
                schema
            }
        }
    };
}

text!(
    /// A bundle's ID: reverse DNS, lowercase, such as `org.nginx.nginx`.
    Id,
    "Reverse DNS, lowercase, such as org.nginx.nginx",
    @pattern "^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+$",
    check_id
);

text!(
    /// One line of text, as people read it: not empty, and at most 200
    /// characters.
    Line,
    "One line of text, at most 200 characters",
    @pattern "^[^\\n\\r]{1,200}$",
    |text| check_line(text, 200)
);

text!(
    /// Text of paragraphs, at most 4000 characters.
    Text,
    "Text, in paragraphs, at most 4000 characters",
    check_text
);

text!(
    /// A version of the bundle: numbers between dots, such as `17.2` or
    /// `1.27.3`, with a `-` pre-release and a `+` build as semantic
    /// versioning has them.
    Version,
    "A version, such as 1.27.3 or 17.2, as semantic versioning writes it",
    @pattern "^(0|[1-9][0-9]*)(\\.(0|[1-9][0-9]*)){0,3}(-[0-9A-Za-z.-]+)?(\\+[0-9A-Za-z.-]+)?$",
    check_version
);

text!(
    /// A web address, `https://`.
    Url,
    "A web address, starting https://",
    @pattern "^https://[^\\s]+$",
    check_url
);

text!(
    /// A license, as an SPDX expression such as `MIT OR Apache-2.0`.
    License,
    "An SPDX license expression, such as MIT or GPL-2.0-only",
    |text| spdx::check(text).map(|_| ())
);

text!(
    /// A path of a file in the bundle, such as `bin/nginx`.
    BundlePath,
    "A path inside the bundle, such as bin/nginx",
    @pattern "^[^/\\\\][^\\\\]*$",
    check_bundle_path
);

text!(
    /// The name of a program, command or socket: letters, digits, `.`,
    /// `_`, `+` and `-`, starting with a letter or digit.
    Name,
    "A name of letters, digits, '.', '_', '+' and '-', starting with a letter or digit",
    @pattern "^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$",
    |text| check_name(text, "a name")
);

text!(
    /// A service's name, as service files have them.
    ServiceName,
    "A service's name: letters, digits, '-', '_' and '.'",
    @pattern "^[A-Za-z0-9_][A-Za-z0-9_.-]*$",
    service::check_name
);

text!(
    /// An account's name, as Debian's: lowercase letters, digits, `_`,
    /// `-` and `.`, starting with a letter or `_`.
    Account,
    "An account's name: lowercase letters, digits, '_', '-' and '.', up to 32",
    @pattern "^[a-z_][a-z0-9_.-]{0,31}$",
    check_account
);

text!(
    /// An interface's name and major version, such as
    /// `dukovo.certificates/1`.
    Interface,
    "An interface's name and major version, such as dukovo.certificates/1",
    @pattern "^[a-z0-9][a-z0-9.-]*/[1-9][0-9]*$",
    check_interface
);

text!(
    /// A language, as a BCP 47 tag such as `de` or `pt-BR`.
    Language,
    "A language, as a BCP 47 tag such as de or pt-BR",
    @pattern "^[a-z]{2,3}(-[A-Za-z0-9]{2,8})*$",
    check_language
);

text!(
    /// A minisign public key, as `minisign -G` writes it.
    PublicKey,
    "A minisign public key, as minisign -G writes it",
    @pattern "^RW[A-Za-z0-9+/]{54}$",
    check_public_key
);

text!(
    /// The name of a secret, as service files have them.
    SecretName,
    "A secret's name",
    service::check_secret
);

/// Checks a bundle's ID: reverse DNS, lowercase, such as
/// `org.nginx.nginx`.
pub fn check_id(id: &str) -> Result<(), String> {
    let labels: Vec<&str> = id.split('.').collect();
    let label_ok = |l: &str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    if id.len() <= 255 && labels.len() >= 2 && labels.iter().all(|l| label_ok(l)) {
        Ok(())
    } else {
        Err(format!(
            "id is a reverse DNS name in lowercase, such as org.example.app, from a domain its \
             publisher holds; not `{}`",
            id.escape_debug()
        ))
    }
}

fn check_line(text: &str, max: usize) -> Result<(), String> {
    if text.trim().is_empty() {
        Err("this text is empty: write it, or leave the field out where it may be".into())
    } else if text.chars().any(char::is_control) {
        Err(format!("`{}` is one line, without control characters", text.escape_debug()))
    } else if text.chars().count() > max {
        Err(format!("`{text}` is longer than {max} characters"))
    } else {
        Ok(())
    }
}

fn check_text(text: &str) -> Result<(), String> {
    if text.chars().any(|c| c.is_control() && c != '\n') {
        Err("it holds a control character other than a line's end".into())
    } else if text.chars().count() > 4000 {
        Err("it is longer than 4000 characters".into())
    } else {
        Ok(())
    }
}

fn check_version(version: &str) -> Result<(), String> {
    let (rest, build) = version.split_once('+').map_or((version, None), |(a, b)| (a, Some(b)));
    let (core, pre) = rest.split_once('-').map_or((rest, None), |(a, b)| (a, Some(b)));
    let tag_ok = |tag: &str| {
        !tag.is_empty()
            && tag.split('.').all(|part| {
                !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
    };
    let number_ok = |n: &str| {
        !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && (n == "0" || !n.starts_with('0'))
    };
    let parts: Vec<&str> = core.split('.').collect();
    if (1..=4).contains(&parts.len())
        && parts.iter().all(|n| number_ok(n))
        && pre.is_none_or(tag_ok)
        && build.is_none_or(tag_ok)
    {
        Ok(())
    } else {
        Err(format!(
            "version is numbers between dots, such as 1.27.3 or 17.2, perhaps with a -beta.1 \
             and a +build as semantic versioning has them; not `{}`",
            version.escape_debug()
        ))
    }
}

fn check_url(url: &str) -> Result<(), String> {
    match url.strip_prefix("https://") {
        Some(rest)
            if !rest.is_empty() && !rest.chars().any(|c| c.is_whitespace() || c.is_control()) =>
        {
            Ok(())
        }
        _ => Err(format!("`{}` is not a web address starting https://", url.escape_debug())),
    }
}

fn check_bundle_path(path: &str) -> Result<(), String> {
    let fits = !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..");
    if fits {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not a path in the bundle: write one such as bin/app, without a / first, \
             `.`, `..`, `//`, `\\` or control characters",
            path.escape_debug()
        ))
    }
}

fn check_name(name: &str, what: &str) -> Result<(), String> {
    let fits = (1..=64).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        && name.as_bytes()[0].is_ascii_alphanumeric();
    if fits {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not {what}: it has 1 to 64 letters, digits, `.`, `_`, `+` and `-`, and \
             starts with a letter or a digit",
            name.escape_debug()
        ))
    }
}

/// `name` as an account's name may be: lowercase, with what an account's
/// name cannot hold left out.
fn account_like(name: &str) -> String {
    let lower: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "_-.".contains(*c))
        .map(|c| c.to_ascii_lowercase())
        .take(32)
        .collect();
    match lower.chars().next() {
        Some(c) if c.is_ascii_lowercase() || c == '_' => lower,
        _ => format!("_{}", &lower[..lower.len().min(31)]),
    }
}

fn check_account(name: &str) -> Result<(), String> {
    let fits = (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.".contains(&b))
        && (name.as_bytes()[0].is_ascii_lowercase() || name.starts_with('_'));
    if fits {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not an account's name: it has 1 to 32 lowercase letters, digits, `_`, `-` \
             and `.`, and starts with a letter or `_`",
            name.escape_debug()
        ))
    }
}

fn check_interface(text: &str) -> Result<(), String> {
    let fits = text.split_once('/').is_some_and(|(name, major)| {
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b))
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && !major.is_empty()
            && !major.starts_with('0')
            && major.bytes().all(|b| b.is_ascii_digit())
    });
    if fits {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not an interface: write its name and major version, such as \
             dukovo.certificates/1",
            text.escape_debug()
        ))
    }
}

fn check_language(tag: &str) -> Result<(), String> {
    let mut parts = tag.split('-');
    let first = parts.next().unwrap_or("");
    let fits = (2..=3).contains(&first.len())
        && first.bytes().all(|b| b.is_ascii_lowercase())
        && parts
            .all(|p| (2..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()));
    if fits {
        Ok(())
    } else {
        Err(format!("`{}` is not a language tag such as de or pt-BR", tag.escape_debug()))
    }
}

/// The bytes base64 `text` holds, if it is base64.
fn base64(text: &str) -> Option<Vec<u8>> {
    let value = |b: u8| match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let text = text.trim_end_matches('=');
    let mut out = Vec::new();
    let (mut bits, mut acc) = (0u32, 0u32);
    for b in text.bytes() {
        acc = (acc << 6) | u32::from(value(b)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn check_public_key(key: &str) -> Result<(), String> {
    // minisign: "Ed", an 8-byte key ID and the 32-byte Ed25519 key.
    match base64(key) {
        Some(bytes) if bytes.len() == 42 && bytes.starts_with(b"Ed") => Ok(()),
        _ => Err(format!(
            "`{}` is not a minisign public key: it is the line after the comment in the .pub \
             file minisign -G writes, starting RW",
            key.escape_debug()
        )),
    }
}

/// Defines a field that holds one of a few words.
macro_rules! words {
    ($(#[$doc:meta])* $name:ident, $field:literal, [$($(#[$vdoc:meta])* $variant:ident = $word:literal),+ $(,)?]) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name {
            $($(#[$vdoc])* $variant),+
        }

        impl $name {
            /// The words a manifest writes them with.
            pub const WORDS: &[&str] = &[$($word),+];
            const ALL: &[$name] = &[$($name::$variant),+];

            /// The word a manifest writes it with.
            pub fn word(self) -> &'static str {
                Self::WORDS[Self::ALL.iter().position(|v| *v == self).unwrap_or(0)]
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.word())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<$name, D::Error> {
                Ok(Self::ALL[service::word(d, $field, Self::WORDS)?])
            }
        }

        #[cfg(feature = "schema")]
        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({"type": "string", "enum": Self::WORDS})
            }
        }
    };
}

words!(
    /// An architecture a bundle carries programs for.
    Arch,
    "architectures",
    [
        /// 64-bit x86.
        X86_64 = "x86_64",
        /// 64-bit Arm (Dukovo 1.16).
        Aarch64 = "aarch64",
        /// 64-bit RISC-V (Dukovo 1.17).
        Riscv64 = "riscv64",
        /// WebAssembly (Dukovo 1.9).
        Wasm = "wasm",
    ]
);

words!(
    /// How a command runs.
    RunsWith,
    "runs_with",
    [
        /// In the bundle's sandbox, with what its permissions give.
        Sandbox = "sandbox",
        /// With everything the shell that starts it holds, as a program
        /// of the system's.
        YourAccess = "your-access",
    ]
);

words!(
    /// When a service starts.
    Start,
    "start",
    [
        /// When an administrator enables it.
        WhenEnabled = "when-enabled",
        /// At boot, once it is installed.
        AtBoot = "at-boot",
        /// On the first connection to a socket it provides.
        OnConnection = "on-connection",
    ]
);

words!(
    /// Whether a permission or an interface is needed.
    Need,
    "need",
    [
        /// The bundle does not work without it, and is not installed
        /// without it.
        Required = "required",
        /// The bundle works without it, with less.
        Optional = "optional",
    ]
);

words!(
    /// What a folder permission lets the program do.
    Access,
    "folders",
    [
        /// Read it.
        Read = "read",
        /// Read and change it.
        ReadWrite = "read-write",
    ]
);

words!(
    /// Who may connect to what a bundle provides.
    To,
    "to",
    [
        /// The programs an administrator, or the person, approves.
        Approved = "approved",
        /// Any bundle.
        Anyone = "anyone",
        /// The publisher's other bundles.
        Publisher = "publisher",
        /// The system's programs only.
        System = "system",
    ]
);

/// The first Dukovo release a bundle runs on: `1.0`, or `1.4.1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OsVersion(pub u32, pub u32, pub u32);

impl fmt::Display for OsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.2 {
            0 => write!(f, "{}.{}", self.0, self.1),
            patch => write!(f, "{}.{}.{patch}", self.0, self.1),
        }
    }
}

impl<'de> Deserialize<'de> for OsVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<OsVersion, D::Error> {
        let text = String::deserialize(d)?;
        let numbers: Option<Vec<u32>> = text
            .split('.')
            .map(|n| if n.bytes().all(|b| b.is_ascii_digit()) { n.parse().ok() } else { None })
            .collect();
        match numbers.as_deref() {
            Some(&[major, minor]) => Ok(OsVersion(major, minor, 0)),
            Some(&[major, minor, patch]) => Ok(OsVersion(major, minor, patch)),
            _ => Err(de::Error::custom(format!(
                "requires_os is a Dukovo release, such as \"1.0\" or \"1.4.1\", not `{}`",
                text.escape_debug()
            ))),
        }
    }
}

#[cfg(feature = "schema")]
impl JsonSchema for OsVersion {
    fn schema_name() -> Cow<'static, str> {
        "OsVersion".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "A Dukovo release, such as 1.0 or 1.4.1",
            "pattern": "^[0-9]+\\.[0-9]+(\\.[0-9]+)?$"
        })
    }
}

/// Who publishes a bundle.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Publisher {
    /// Their name, as people know them.
    pub name: Line,
    /// Their web site.
    #[serde(default)]
    pub url: Option<Url>,
}

/// A program of the bundle.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Program {
    /// Its file for each architecture.
    pub path: Map<Arch, BundlePath>,
    /// The permissions it runs with, if fewer than the bundle's: each one
    /// the bundle asks for.
    #[serde(default)]
    pub permissions: Vec<String>,
}

/// A command the bundle adds to the terminal.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// What people type.
    pub name: Name,
    /// The program it runs.
    pub program: Name,
    /// Whether it runs in the bundle's sandbox or with your access.
    pub runs_with: RunsWith,
}

/// A service the bundle runs: the same settings as a service file
/// (service.toml(5)), with its program by name.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Its name, which `service` takes.
    pub name: ServiceName,
    /// The program it runs.
    pub program: Name,
    /// The arguments after the program's name.
    #[serde(default)]
    pub arguments: Vec<String>,
    /// More of the environment, as names and values.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(with = "Map<String, String>"))]
    pub environment: Environment,
    /// The account it runs as, its own; the service's name when left out.
    #[serde(default)]
    pub account: Option<Account>,
    /// When it is started again after it ends.
    #[serde(default, deserialize_with = "service::restart")]
    #[cfg_attr(feature = "schema", schemars(with = "RestartWord"))]
    pub restart: Restart,
    /// What it may use.
    #[serde(default)]
    pub limits: Limits,
    /// The names of the secrets it gets as read-only files.
    #[serde(default, deserialize_with = "service::secret_names")]
    #[cfg_attr(feature = "schema", schemars(with = "Vec<SecretName>"))]
    pub secrets: Vec<String>,
    /// When it starts.
    #[serde(default = "when_enabled")]
    pub start: Start,
    /// The interfaces that must be up before it starts.
    #[serde(default)]
    pub needs: Vec<Interface>,
    /// How to tell that it works.
    #[serde(default)]
    pub health: Option<Health>,
}

fn when_enabled() -> Start {
    Start::WhenEnabled
}

/// A service's environment: names and values, in the order written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Environment(pub Vec<(String, String)>);

impl<'de> Deserialize<'de> for Environment {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Environment, D::Error> {
        service::Environment::deserialize(d).map(|e| Environment(e.0))
    }
}

/// `restart`'s words, for the schema.
#[cfg(feature = "schema")]
struct RestartWord;

#[cfg(feature = "schema")]
impl JsonSchema for RestartWord {
    fn schema_name() -> Cow<'static, str> {
        "Restart".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "enum": Restart::WORDS})
    }
}

/// What a service's domain may use: a service file's limits, and how many
/// files each process may have open.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Its share of the CPUs, from 1 to 10000; 100 is everyone's.
    #[serde(default = "service::weight_default", deserialize_with = "service::weight")]
    pub cpu_weight: u32,
    /// Its memory limit, in bytes, if it has one.
    #[serde(default, deserialize_with = "service::memory")]
    pub memory: Option<u64>,
    /// Which programs the out-of-memory policy stops first.
    #[serde(default, deserialize_with = "service::oom")]
    pub oom: Oom,
    /// How many files each of its processes may have open at once
    /// (RLIMIT_NOFILE), if not a program's 1024.
    #[serde(default, deserialize_with = "open_files")]
    pub open_files: Option<u32>,
}

impl Default for Limits {
    fn default() -> Limits {
        let service::Limits { cpu_weight, memory, oom } = service::Limits::default();
        Limits { cpu_weight, memory, oom, open_files: None }
    }
}

fn open_files<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    match service::whole(d, "open_files")? {
        n @ 1.. if n <= u64::from(OPEN_FILES_MAX) => Ok(Some(n as u32)),
        n => Err(de::Error::custom(format!("open_files is from 1 to {OPEN_FILES_MAX}, not {n}"))),
    }
}

#[cfg(feature = "schema")]
impl JsonSchema for Limits {
    fn schema_name() -> Cow<'static, str> {
        "Limits".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "properties": {
                "cpu_weight": {"type": "integer", "minimum": 1, "maximum": WEIGHT_MAX},
                "memory": {
                    "description": "A size such as \"512M\" or \"4G\", a number of bytes, or \"none\"",
                    "anyOf": [
                        {"type": "integer", "minimum": 1},
                        {"type": "string", "pattern": "^([0-9]+([KMGkmg]|KiB|MiB|GiB)?|none)$"}
                    ]
                },
                "oom": {"type": "string", "enum": Oom::WORDS},
                "open_files": {"type": "integer", "minimum": 1, "maximum": OPEN_FILES_MAX}
            },
            "additionalProperties": false
        })
    }
}

/// How to tell that a service works.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Health {
    /// A socket it provides, which answers once it works.
    pub socket: Name,
}

/// The permissions a bundle asks for, by name, each with what it takes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Permissions(pub Vec<(String, Request)>);

impl Permissions {
    /// What it asks of permission `name`.
    pub fn get(&self, name: &str) -> Option<&Request> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, r)| r)
    }

    /// The permissions, in the order written.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Request)> {
        self.0.iter().map(|(n, r)| (n.as_str(), r))
    }
}

/// What a bundle asks of a permission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Whether it works without it.
    pub need: Need,
    /// Why it asks, in its own words, shown under the system's.
    pub why: Option<Line>,
    /// `files.folder` and `files.shared`: the folders, each with what it
    /// may do there.
    pub folders: Vec<(String, Access)>,
    /// `network.listen`: the ports and ranges of ports.
    pub ports: Vec<Ports>,
    /// `files.app-data`: the other apps whose data it reads.
    pub apps: Vec<Id>,
}

/// A permission's object as written, before what it takes is checked
/// against the permission.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
struct RawRequest {
    /// Whether the bundle works without it.
    need: Need,
    /// Why it asks, in its own words.
    #[serde(default)]
    why: Option<Line>,
    /// files.folder and files.shared: each folder, and read or read-write.
    #[serde(default)]
    folders: Option<Map<String, Access>>,
    /// network.listen: ports, and ranges such as "8000-8009".
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(with = "Option<Vec<PortsSchema>>"))]
    ports: Option<Vec<Ports>>,
    /// files.app-data: other bundles' IDs.
    #[serde(default)]
    apps: Option<Vec<Id>>,
}

/// `ports`' items, for the schema.
#[cfg(feature = "schema")]
struct PortsSchema;

#[cfg(feature = "schema")]
impl JsonSchema for PortsSchema {
    fn schema_name() -> Cow<'static, str> {
        "Ports".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A port, or a range such as \"8000-8009\"",
            "anyOf": [
                {"type": "integer", "minimum": 1, "maximum": 65535},
                {"type": "string", "pattern": "^[0-9]+-[0-9]+$"}
            ]
        })
    }
}

/// `permissions` as written: each permission's name checked against the
/// list as it is read, and its fields not yet against what it takes.
struct RawPermissions(Vec<(String, RawRequest)>);

impl<'de> Deserialize<'de> for RawPermissions {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<RawPermissions, D::Error> {
        struct Members;

        impl<'de> Visitor<'de> for Members {
            type Value = RawPermissions;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of permissions, by name")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawPermissions, A::Error> {
                let mut asked: Vec<(String, RawRequest)> = Vec::new();
                while let Some(name) = map.next_key::<String>()? {
                    if asked.iter().any(|(n, _)| *n == name) {
                        return Err(de::Error::custom(format!("{name} is written twice")));
                    }
                    let permission = permissions::find(&name).ok_or_else(|| {
                        de::Error::custom(format!(
                            "{name} is not a permission; manifest.json(5) lists them"
                        ))
                    })?;
                    if let Some(release) = permission.reserved_until {
                        return Err(de::Error::custom(format!(
                            "{name} comes with Dukovo {release}, in a later manifest version: \
                             version {VERSION} reserves its name"
                        )));
                    }
                    asked.push((name, map.next_value()?));
                }
                Ok(RawPermissions(asked))
            }
        }

        d.deserialize_map(Members)
    }
}

/// What `raw` asks of permission `name`, which takes `takes`.
fn request(name: &str, takes: Takes, raw: RawRequest) -> Result<Request, String> {
    let mut request = Request {
        need: raw.need,
        why: raw.why,
        folders: Vec::new(),
        ports: Vec::new(),
        apps: Vec::new(),
    };
    let takes_word = match takes {
        Takes::Nothing => None,
        Takes::PersonalFolders | Takes::SharedFolders => Some("folders"),
        Takes::Ports => Some("ports"),
        Takes::Apps => Some("apps"),
    };
    for (given, word) in [
        (raw.folders.is_some(), "folders"),
        (raw.ports.is_some(), "ports"),
        (raw.apps.is_some(), "apps"),
    ] {
        if given && takes_word != Some(word) {
            return Err(format!("{name} takes no {word}"));
        }
    }
    match takes {
        Takes::Nothing => {}
        Takes::PersonalFolders | Takes::SharedFolders => {
            let folders = raw.folders.unwrap_or_default();
            if folders.is_empty() {
                return Err(format!(
                    "{name} names its folders, such as \"folders\": {{ \"documents\": \"read\" }}"
                ));
            }
            for (folder, _) in folders.iter() {
                if takes == Takes::PersonalFolders && !PERSONAL_FOLDERS.contains(&folder.as_str()) {
                    return Err(format!(
                        "{folder} is not one of your folders; files.folder takes {}",
                        PERSONAL_FOLDERS.join(", ")
                    ));
                }
                if takes == Takes::SharedFolders {
                    check_name(folder, "a folder of /shared")?;
                }
            }
            request.folders = folders.0;
        }
        Takes::Ports => {
            let ports = raw.ports.unwrap_or_default();
            if ports.is_empty() {
                return Err(format!("{name} names its ports, such as \"ports\": [80, 443]"));
            }
            for (i, a) in ports.iter().enumerate() {
                if let Some(b) = ports[..i].iter().find(|b| a.first <= b.last && b.first <= a.last)
                {
                    return Err(format!("{name}: {a} overlaps {b}"));
                }
            }
            request.ports = ports;
        }
        Takes::Apps => {
            let apps = raw.apps.unwrap_or_default();
            if let Some(twice) = apps.iter().enumerate().find(|(i, a)| apps[..*i].contains(a)) {
                return Err(format!("{name} names {} twice", twice.1));
            }
            if apps.is_empty() {
                return Err(format!(
                    "{name} names the apps, such as \"apps\": [\"org.example.app\"]"
                ));
            }
            request.apps = apps;
        }
    }
    Ok(request)
}

/// What a bundle keeps in its data folder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Data {
    /// Settings, which backups keep.
    #[serde(default)]
    pub settings: bool,
    /// Data, which backups keep.
    #[serde(default)]
    pub data: bool,
    /// A cache, never backed up or snapshotted.
    #[serde(default)]
    pub cache: bool,
}

/// A socket or an interface a bundle offers.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawProvide")]
pub struct Provide {
    /// A socket, by name.
    pub socket: Option<Name>,
    /// An interface, by name and major version.
    pub interface: Option<Interface>,
    /// Who may connect.
    pub to: To,
}

#[cfg(feature = "schema")]
impl JsonSchema for Provide {
    fn schema_name() -> Cow<'static, str> {
        "Provide".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let socket = generator.subschema_for::<Name>();
        let interface = generator.subschema_for::<Interface>();
        let to = generator.subschema_for::<To>();
        json_schema!({
            "description": "A socket or an interface the bundle offers, one of the two, and who may connect",
            "type": "object",
            "properties": {"socket": socket, "interface": interface, "to": to},
            "required": ["to"],
            "oneOf": [{"required": ["socket"]}, {"required": ["interface"]}],
            "additionalProperties": false
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvide {
    #[serde(default)]
    socket: Option<Name>,
    #[serde(default)]
    interface: Option<Interface>,
    to: To,
}

impl TryFrom<RawProvide> for Provide {
    type Error = String;

    fn try_from(raw: RawProvide) -> Result<Provide, String> {
        if raw.socket.is_some() == raw.interface.is_some() {
            return Err("each of provides names a socket or an interface, one of the two".into());
        }
        Ok(Provide { socket: raw.socket, interface: raw.interface, to: raw.to })
    }
}

/// An interface a bundle talks to.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Use {
    /// The interface, by name and major version.
    pub interface: Interface,
    /// Whether the bundle works without it.
    pub need: Need,
}

/// A bundle's own update feed.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Update {
    /// Where its signed update metadata is.
    pub feed: Url,
    /// The key that signs it.
    pub key: PublicKey,
}

/// A bundle's texts in another language.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Translation {
    /// Its name.
    #[serde(default)]
    pub name: Option<Line>,
    /// Its summary.
    #[serde(default)]
    pub summary: Option<Line>,
    /// Its description.
    #[serde(default)]
    pub description: Option<Text>,
    /// Why it asks for each permission.
    #[serde(default)]
    pub why: Map<String, Line>,
}

/// The shape of a manifest, for its JSON Schema: every field, those that
/// may be left out as options. [`from_json`] reads the same fields.
#[cfg(feature = "schema")]
#[derive(JsonSchema)]
#[schemars(deny_unknown_fields, transform = extensions, rename = "Manifest")]
#[allow(dead_code)]
struct Shape {
    /// The version of this format: 1.
    manifest_version: VersionOne,
    id: Id,
    name: Line,
    summary: Line,
    description: Option<Text>,
    version: Version,
    publisher: Publisher,
    license: License,
    source: Option<Url>,
    homepage: Option<Url>,
    icon: Option<BundlePath>,
    requires_os: OsVersion,
    architectures: Vec<Arch>,
    programs: Map<Name, Program>,
    commands: Option<Vec<Command>>,
    services: Option<Vec<Service>>,
    permissions: Option<PermissionsSchema>,
    data: Option<Data>,
    provides: Option<Vec<Provide>>,
    uses: Option<Vec<Use>>,
    update: Option<Update>,
    translations: Option<Map<Language, Translation>>,
}

/// `manifest_version`, for the schema.
#[cfg(feature = "schema")]
struct VersionOne;

#[cfg(feature = "schema")]
impl JsonSchema for VersionOne {
    fn schema_name() -> Cow<'static, str> {
        "ManifestVersion".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"const": VERSION})
    }
}

/// `permissions`, for the schema: each permission version 1 defines,
/// with the fields it takes.
#[cfg(feature = "schema")]
struct PermissionsSchema;

#[cfg(feature = "schema")]
impl JsonSchema for PermissionsSchema {
    fn schema_name() -> Cow<'static, str> {
        "Permissions".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let request = generator.subschema_for::<RawRequest>();
        let mut properties = serde_json::Map::new();
        for permission in permissions::LIST.iter().filter(|p| p.reserved_until.is_none()) {
            let takes = match permission.takes {
                Takes::Nothing => {
                    serde_json::json!({"properties": {"folders": false, "ports": false, "apps": false}})
                }
                Takes::PersonalFolders => serde_json::json!({
                    "required": ["folders"],
                    "properties": {
                        "ports": false,
                        "apps": false,
                        "folders": {"propertyNames": {"enum": PERSONAL_FOLDERS}}
                    }
                }),
                Takes::SharedFolders => {
                    serde_json::json!({"required": ["folders"], "properties": {"ports": false, "apps": false}})
                }
                Takes::Ports => {
                    serde_json::json!({"required": ["ports"], "properties": {"folders": false, "apps": false}})
                }
                Takes::Apps => {
                    serde_json::json!({"required": ["apps"], "properties": {"folders": false, "ports": false}})
                }
            };
            let mut one = serde_json::Map::new();
            one.insert("description".into(), permission.lets.into());
            one.insert("allOf".into(), serde_json::json!([request.clone(), takes]));
            properties.insert(permission.name.into(), one.into());
        }
        json_schema!({"type": "object", "properties": properties, "additionalProperties": false})
    }
}

/// Lets a manifest hold `$schema` and `x-` fields.
#[cfg(feature = "schema")]
fn extensions(schema: &mut Schema) {
    if let Some(serde_json::Value::Object(properties)) = schema.get_mut("properties") {
        properties.insert("$schema".into(), serde_json::json!({"type": "string"}));
    }
    schema.insert("patternProperties".into(), serde_json::json!({"^x-": true}));
}

/// The JSON Schema of manifest version 1, as `bundle schema` prints it.
#[cfg(feature = "schema")]
pub fn schema() -> String {
    let mut schema = schemars::schema_for!(Shape);
    schema.insert(
        "$id".into(),
        serde_json::json!(format!("https://dukovo.org/schemas/manifest-{VERSION}.json")),
    );
    schema.insert(
        "description".into(),
        "A Dukovo bundle's manifest.json, version 1 (a preview until Dukovo 1.0); \
         manifest.json(5) explains each field"
            .into(),
    );
    serde_json::to_string_pretty(&schema).unwrap_or_default()
}

/// The manifest `bundle new` writes for a bundle `id` named `name`, by
/// `publisher`, under `license`: one program and its command, which
/// manifest.json(5) explains with every other field. `id` is checked
/// already; the texts are written as JSON strings.
pub fn template(id: &str, name: &str, publisher: &str, license: &str) -> String {
    let program = id.rsplit('.').next().unwrap_or(id);
    let quote = |text: &str| serde_json::to_string(text).unwrap_or_default();
    format!(
        r#"{{
  "manifest_version": {VERSION},
  "id": {id},
  "name": {name},
  "summary": {summary},
  "version": "0.1.0",
  "publisher": {{ "name": {publisher} }},
  "license": {license},
  "requires_os": "1.0",
  "architectures": ["x86_64"],
  "programs": {{
    {program}: {{ "path": {{ "x86_64": {path} }} }}
  }},
  "commands": [
    {{ "name": {program}, "program": {program}, "runs_with": "sandbox" }}
  ],
  "permissions": {{}},
  "data": {{ "settings": true }}
}}
"#,
        id = quote(id),
        name = quote(name),
        summary = quote(&format!("What {name} does, in a few words")),
        publisher = quote(publisher),
        license = quote(license),
        program = quote(program),
        path = quote(&format!("bin/{program}")),
    )
}

#[cfg(test)]
mod tests;
