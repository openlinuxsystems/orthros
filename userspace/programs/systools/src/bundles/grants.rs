// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Grants (ADR 0224): what each person said about each app's permissions,
//! and what the runner gives a program from it.
//!
//! A permission given at install, or always, is given unless the person
//! took it away. One asked on first use, or with a warning, is given once
//! they allowed it: asked through the trusted prompt the first time the
//! app runs, or given ahead with `bundle grant`. A permission that takes
//! more, folders or apps, is said in words: `NAME=read` or
//! `NAME=read-write` for a folder, an ID for an app. A grant keeps the
//! words it gives and the words the app asked for when it was said, so a
//! later version that asks for more is asked again, and one that asks for
//! the same or less is not.
//!
//! bundled(8) keeps the grants in [`FOLDER`], a file for each person by
//! user number, with the publisher's key of the app each was said of: a
//! bundle signed by another key gets none of them. Only bundled writes
//! them, and only after the prompt, so a program cannot give itself
//! anything by writing a file or running a command.

use std::collections::BTreeMap;

use manifest::bundle::permissions::{self, Permission, Takes, Tier};
use manifest::bundle::{Need, Request as Asked};

use super::minisign::PublicKey;

/// Where bundled keeps the grants: a file for each person, named by their
/// user number.
pub const FOLDER: &str = "/system/state/grants";

/// The permission a command that runs with your access needs, whether or
/// not its manifest names it.
pub const YOUR_ACCESS: &str = "runs-with-your-access";

/// Permissions manifest version 1 defines that this Dukovo cannot give
/// yet: reaching only the local network waits for discovery (12c), and
/// debugging other programs for a debugger's capability.
pub const NOT_YET: &[&str] = &["network.local", "processes.debug"];

/// The most grants bundled keeps for one person.
pub const GRANTS_MAX: usize = 4096;

/// What a person said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Said {
    /// Allow it.
    Allow,
    /// Don't allow it, and don't ask again.
    Never,
}

impl Said {
    pub fn word(self) -> &'static str {
        match self {
            Said::Allow => "allow",
            Said::Never => "never",
        }
    }

    pub fn from_word(word: &str) -> Option<Said> {
        [Said::Allow, Said::Never].into_iter().find(|s| s.word() == word)
    }
}

/// What a person said about one permission of one app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The permission, by its name in the list.
    pub permission: String,
    pub said: Said,
    /// When, in seconds since 1970.
    pub since: u64,
    /// What it gives, in words; none for a permission that takes nothing
    /// more, or for one not allowed.
    pub given: Vec<String>,
    /// What the app asked for when it was said, in words.
    pub asked: Vec<String>,
}

/// A grant as bundled keeps it: of which app, signed by which key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub id: String,
    pub key: PublicKey,
    pub grant: Grant,
}

/// What `request` asks for, in words, sorted: `NAME=read` or
/// `NAME=read-write` for each folder, the ID of each app; none for a
/// permission that takes nothing more. Ports are not words: they are
/// given at install, with the permission.
pub fn words(request: &Asked) -> Vec<String> {
    let mut words: Vec<String> =
        request.folders.iter().map(|(name, access)| format!("{name}={}", access.word())).collect();
    words.extend(request.apps.iter().map(|id| id.0.clone()));
    words.sort();
    words.dedup();
    words
}

/// Whether `have` covers each word of `want`: a folder to read and change
/// covers the same folder to read.
pub fn covers(have: &[String], want: &[String]) -> bool {
    want.iter().all(|w| {
        have.contains(w)
            || w.strip_suffix("=read")
                .is_some_and(|name| have.contains(&format!("{name}=read-write")))
    })
}

/// What of `asked` `given` gives: each word given, or a folder to read
/// where only reading it was given.
pub fn within(given: &[String], asked: &[String]) -> Vec<String> {
    asked
        .iter()
        .filter_map(|w| {
            if covers(given, std::slice::from_ref(w)) {
                return Some(w.clone());
            }
            let read = format!("{}=read", w.strip_suffix("=read-write")?);
            given.contains(&read).then_some(read)
        })
        .collect()
}

/// A folder's name as words take it: 1 to 64 letters, digits, `.`, `_`,
/// `+` and `-`, starting with a letter or a digit, as manifests do.
fn folder_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        && name.as_bytes()[0].is_ascii_alphanumeric()
}

/// Checks that `word` is one `permission` takes.
pub fn check_word(permission: &Permission, word: &str) -> Result<(), String> {
    let folder = |personal: bool| {
        let (name, access) = word.split_once('=').unwrap_or((word, ""));
        let named = if personal {
            permissions::PERSONAL_FOLDERS.contains(&name)
        } else {
            folder_name(name)
        };
        named && matches!(access, "read" | "read-write")
    };
    let fits = match permission.takes {
        Takes::PersonalFolders => folder(true),
        Takes::SharedFolders => folder(false),
        Takes::Apps => manifest::bundle::check_id(word).is_ok(),
        Takes::Nothing | Takes::Ports => false,
    };
    if fits {
        return Ok(());
    }
    Err(match permission.takes {
        Takes::PersonalFolders => format!(
            "{word:?} is not FOLDER=read or FOLDER=read-write, FOLDER one of {}",
            permissions::PERSONAL_FOLDERS.join(", ")
        ),
        Takes::SharedFolders => format!("{word:?} is not NAME=read or NAME=read-write"),
        Takes::Apps => format!("{word:?} is not an app's ID"),
        Takes::Nothing | Takes::Ports => format!("{} takes nothing more", permission.name),
    })
}

/// What a permission given with `words` lets the app do, as the prompts
/// say it after "asks to:".
pub fn phrase(permission: &Permission, request: Option<&Asked>, words: &[String]) -> String {
    let folders = |shared: bool| {
        let each: Vec<String> = words
            .iter()
            .filter_map(|w| {
                let (name, access) = w.split_once('=')?;
                let what = if access == "read-write" { "read and change it" } else { "read it" };
                Some(if shared {
                    format!("the folder /shared/{name}, to {what}")
                } else {
                    format!("your {name} folder, to {what}")
                })
            })
            .collect();
        format!("use {}", each.join(", and "))
    };
    match permission.name {
        "files.folder" if !words.is_empty() => folders(false),
        "files.shared" if !words.is_empty() => folders(true),
        "files.app-data" if !words.is_empty() => {
            format!("read the private data of {}", words.join(", "))
        }
        "files.home" => "use all of your files, which every other app keeps there too".into(),
        YOUR_ACCESS => "run with everything you can reach, without a sandbox".into(),
        "network.listen" if request.is_some_and(|r| !r.ports.is_empty()) => {
            let ports: Vec<String> =
                request.map_or(&[][..], |r| &r.ports).iter().map(ToString::to_string).collect();
            format!("accept connections on ports {}", ports.join(", "))
        }
        _ => {
            let mut chars = permission.lets.chars();
            let first = chars.next().map_or_else(String::new, |c| c.to_lowercase().to_string());
            first + chars.as_str()
        }
    }
}

/// What the runner does about one permission its program keeps.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Gives it, with these words.
    Give(Vec<String>),
    /// Asks the person, through bundled and the trusted prompt.
    Ask,
    /// Runs without it, as it is optional.
    Without,
}

/// Whether a permission can be given by this Dukovo at all, and why not.
pub fn givable(permission: &Permission) -> Result<(), String> {
    if permission.reserved_until.is_some() || NOT_YET.contains(&permission.name) {
        return Err("which this version of Dukovo cannot give yet".into());
    }
    if permission.tier == Tier::Admin {
        return Err("which only an administrator gives, and not yet to apps".into());
    }
    Ok(())
}

/// What to do about the permission `name` that app `id` asks for with
/// `request`, from what the person said of it (`grant`); why the program
/// cannot run, for one it requires and does not get.
pub fn step(id: &str, name: &str, request: &Asked, grant: Option<&Grant>) -> Result<Step, String> {
    let required = request.need == Need::Required;
    let Some(permission) = permissions::find(name) else { return Ok(Step::Without) };
    let stop = |why: String| {
        if required {
            Err(format!("{id} needs \"{}\" ({name}), {why}", permission.lets))
        } else {
            Ok(Step::Without)
        }
    };
    if let Err(why) = givable(permission) {
        return stop(why);
    }
    let asked = words(request);
    let said = grant.filter(|g| covers(&g.asked, &asked));
    match said {
        Some(g) if g.said == Said::Never => {
            stop(format!("which you did not allow: bundle grant {id} {name} gives it"))
        }
        _ if matches!(permission.tier, Tier::AtInstall | Tier::Always) => Ok(Step::Give(asked)),
        Some(g) => Ok(Step::Give(within(&g.given, &asked))),
        None => Ok(Step::Ask),
    }
}

/// What the person's answer at the prompt, `word`, makes of the
/// permission `name` that app `id` asks for with `request`: once and
/// always give it as asked; don't allow, closing the question, or no
/// console to ask on leave it out, or stop a program that requires it.
pub fn answered(id: &str, name: &str, request: &Asked, word: &str) -> Result<Step, String> {
    let why = match word {
        "once" | "always" => return Ok(Step::Give(words(request))),
        "never" | "cancelled" => "which you did not allow".to_string(),
        "elsewhere" => format!(
            "and no console here can ask you: run bundle grant {id} {name} at one, where no \
             program can answer for you"
        ),
        _ => return Err(format!("bundled answered {word:?}")),
    };
    if request.need == Need::Required {
        let lets = permissions::find(name).map_or(name, |p| p.lets);
        Err(format!("{id} needs \"{lets}\" ({name}), {why}"))
    } else {
        Ok(Step::Without)
    }
}

/// Every person's grants, as bundled keeps them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grants(pub BTreeMap<u32, Vec<Record>>);

fn word_list(words: &[String]) -> String {
    if words.is_empty() { "-".into() } else { words.join(",") }
}

fn read_words(field: &str) -> Vec<String> {
    if field == "-" { Vec::new() } else { field.split(',').map(String::from).collect() }
}

impl Grants {
    /// One person's records from the text of their file: a line for each,
    /// `ID KEY PERMISSION SAID SINCE GIVEN ASKED`, tab-separated, the
    /// words separated by commas, `-` for none.
    pub fn read(text: &str) -> Result<Vec<Record>, String> {
        let mut records: Vec<Record> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let bad = |why: &str| format!("line {}: {why}", n + 1);
            let fields: Vec<&str> = line.split('\t').collect();
            let [id, key, permission, said, since, given, asked] = fields.as_slice() else {
                return Err(bad("not seven fields"));
            };
            manifest::bundle::check_id(id).map_err(|why| bad(&why))?;
            let key = PublicKey::decode(key).map_err(|why| bad(&why))?;
            let found = permissions::find(permission).ok_or_else(|| bad("no such permission"))?;
            let said = Said::from_word(said).ok_or_else(|| bad("neither allow nor never"))?;
            let since = since.parse().map_err(|_| bad("not a time"))?;
            let (given, asked) = (read_words(given), read_words(asked));
            for word in given.iter().chain(&asked) {
                check_word(found, word).map_err(|why| bad(&why))?;
            }
            let grant = Grant { permission: found.name.into(), said, since, given, asked };
            if records.iter().any(|r| r.id == *id && r.grant.permission == grant.permission) {
                return Err(bad("a permission said twice"));
            }
            records.push(Record { id: (*id).into(), key, grant });
        }
        Ok(records)
    }

    /// The text of a person's file.
    pub fn text(records: &[Record]) -> String {
        let mut lines: Vec<String> = records
            .iter()
            .map(|r| {
                let g = &r.grant;
                format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    r.id,
                    r.key.encoded(),
                    g.permission,
                    g.said.word(),
                    g.since,
                    word_list(&g.given),
                    word_list(&g.asked)
                )
            })
            .collect();
        lines.sort();
        lines.concat()
    }

    /// What `uid` said of app `id` signed by `key`.
    pub fn of(&self, uid: u32, id: &str, key: &PublicKey) -> Vec<Grant> {
        self.0
            .get(&uid)
            .into_iter()
            .flatten()
            .filter(|r| r.id == id && r.key == *key)
            .map(|r| r.grant.clone())
            .collect()
    }

    /// Keeps what `uid` said of app `id`, in place of what they said of
    /// the same permission before.
    pub fn set(&mut self, uid: u32, id: &str, key: PublicKey, grant: Grant) -> Result<(), String> {
        let records = self.0.entry(uid).or_default();
        records.retain(|r| !(r.id == id && r.grant.permission == grant.permission));
        if records.len() >= GRANTS_MAX {
            return Err(format!("you have {GRANTS_MAX} grants already"));
        }
        records.push(Record { id: id.into(), key, grant });
        Ok(())
    }

    /// Forgets what `uid` said of app `id`; whether there was anything.
    pub fn forget(&mut self, uid: u32, id: &str) -> bool {
        let Some(records) = self.0.get_mut(&uid) else { return false };
        let before = records.len();
        records.retain(|r| r.id != id);
        before != records.len()
    }
}

#[cfg(test)]
mod tests {
    use manifest::bundle::Access;

    use super::*;

    fn asked(need: Need, folders: &[(&str, Access)]) -> Asked {
        Asked {
            need,
            why: None,
            folders: folders.iter().map(|(n, a)| ((*n).into(), *a)).collect(),
            ports: Vec::new(),
            apps: Vec::new(),
        }
    }

    fn grant(permission: &str, said: Said, given: &[&str], asked: &[&str]) -> Grant {
        let words = |w: &[&str]| w.iter().map(|w| (*w).to_string()).collect();
        Grant {
            permission: permission.into(),
            said,
            since: 1,
            given: words(given),
            asked: words(asked),
        }
    }

    #[test]
    fn words_say_the_folders_and_cover_less() {
        let a = asked(Need::Required, &[("music", Access::Read), ("documents", Access::ReadWrite)]);
        assert_eq!(words(&a), ["documents=read-write", "music=read"]);
        let have = words(&a);
        assert!(covers(&have, &["documents=read".into()]));
        assert!(covers(&have, &["music=read".into()]));
        assert!(!covers(&have, &["music=read-write".into()]));
        assert!(!covers(&have, &["videos=read".into()]));
        assert_eq!(
            within(&["documents=read".into()], &have),
            ["documents=read"],
            "a folder given to read is read, whatever was asked"
        );
    }

    #[test]
    fn the_steps_follow_what_was_said() {
        let docs = asked(Need::Required, &[("documents", Access::ReadWrite)]);
        let id = "org.example.notes";
        assert_eq!(step(id, "files.folder", &docs, None).unwrap(), Step::Ask);
        let always = grant(
            "files.folder",
            Said::Allow,
            &["documents=read-write"],
            &["documents=read-write"],
        );
        assert_eq!(
            step(id, "files.folder", &docs, Some(&always)).unwrap(),
            Step::Give(vec!["documents=read-write".into()])
        );
        // Given to read only: it reads, and is not asked again.
        let read =
            grant("files.folder", Said::Allow, &["documents=read"], &["documents=read-write"]);
        assert_eq!(
            step(id, "files.folder", &docs, Some(&read)).unwrap(),
            Step::Give(vec!["documents=read".into()])
        );
        // A version that asks for more is asked again.
        let more =
            asked(Need::Required, &[("documents", Access::ReadWrite), ("music", Access::Read)]);
        assert_eq!(step(id, "files.folder", &more, Some(&always)).unwrap(), Step::Ask);
        // Not allowed: required, it stops; optional, it runs without.
        let never = grant("files.folder", Said::Never, &[], &["documents=read-write"]);
        let why = step(id, "files.folder", &docs, Some(&never)).unwrap_err();
        assert!(why.contains("bundle grant org.example.notes files.folder"), "{why}");
        let optional = asked(Need::Optional, &[("documents", Access::ReadWrite)]);
        assert_eq!(step(id, "files.folder", &optional, Some(&never)).unwrap(), Step::Without);
        // Given at install unless taken away.
        let net = asked(Need::Required, &[]);
        assert_eq!(step(id, "network.outbound", &net, None).unwrap(), Step::Give(vec![]));
        let off = grant("network.outbound", Said::Never, &[], &[]);
        assert!(step(id, "network.outbound", &net, Some(&off)).is_err());
        // Not yet, or an administrator's.
        assert!(step(id, "processes.debug", &net, None).unwrap_err().contains("cannot give yet"));
        assert!(step(id, "admin.logs", &net, None).unwrap_err().contains("administrator"));
        let optional = asked(Need::Optional, &[]);
        assert_eq!(step(id, "admin.logs", &optional, None).unwrap(), Step::Without);
    }

    #[test]
    fn answers_give_or_stop() {
        let docs = asked(Need::Required, &[("documents", Access::ReadWrite)]);
        let id = "org.example.notes";
        for word in ["once", "always"] {
            assert_eq!(
                answered(id, "files.folder", &docs, word).unwrap(),
                Step::Give(vec!["documents=read-write".into()])
            );
        }
        for word in ["never", "cancelled"] {
            let why = answered(id, "files.folder", &docs, word).unwrap_err();
            assert!(why.contains("you did not allow"), "{why}");
        }
        let why = answered(id, "files.folder", &docs, "elsewhere").unwrap_err();
        assert!(why.contains("bundle grant org.example.notes files.folder"), "{why}");
        let optional = asked(Need::Optional, &[("documents", Access::Read)]);
        assert_eq!(answered(id, "files.folder", &optional, "never").unwrap(), Step::Without);
        assert!(answered(id, "files.folder", &optional, "maybe").is_err());
    }

    #[test]
    fn grants_read_what_they_write() {
        let key =
            PublicKey::decode("RWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaE").unwrap();
        let mut grants = Grants::default();
        let g = grant("files.folder", Said::Allow, &["documents=read"], &["documents=read-write"]);
        grants.set(1000, "org.example.notes", key, g.clone()).unwrap();
        let off = grant("network.outbound", Said::Never, &[], &[]);
        grants.set(1000, "org.example.notes", key, off.clone()).unwrap();
        let text = Grants::text(&grants.0[&1000]);
        let again = Grants::read(&text).unwrap();
        assert_eq!(Grants::text(&again), text);
        assert_eq!(grants.of(1000, "org.example.notes", &key), [g.clone(), off]);
        assert!(grants.of(1001, "org.example.notes", &key).is_empty());
        // Said again, it replaces what was said.
        let never = grant("files.folder", Said::Never, &[], &["documents=read-write"]);
        grants.set(1000, "org.example.notes", key, never).unwrap();
        assert_eq!(grants.0[&1000].len(), 2);
        assert!(grants.forget(1000, "org.example.notes"));
        assert!(grants.of(1000, "org.example.notes", &key).is_empty());
        for bad in [
            "org.example.notes\tnokey\tfiles.folder\tallow\t1\t-\t-",
            &text.replace("allow", "maybe"),
            &text.replace("documents=read\t", "secrets=read\t"),
            &format!("{text}{text}"),
        ] {
            assert!(Grants::read(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_prompts_say_it_in_the_systems_words() {
        let folder = permissions::find("files.folder").unwrap();
        assert_eq!(
            phrase(folder, None, &["documents=read-write".into()]),
            "use your documents folder, to read and change it"
        );
        assert_eq!(
            phrase(folder, None, &["documents=read-write".into(), "music=read".into()]),
            "use your documents folder, to read and change it, and your music folder, to read it"
        );
        let home = permissions::find("files.home").unwrap();
        assert_eq!(
            phrase(home, None, &[]),
            "use all of your files, which every other app keeps there too"
        );
        let serial = permissions::find("devices.serial").unwrap();
        assert_eq!(phrase(serial, None, &[]), "use a serial port or USB serial device");
        assert!(check_word(folder, "documents=read").is_ok());
        assert!(check_word(folder, "secrets=read").is_err());
        assert!(check_word(folder, "documents=write").is_err());
        assert!(check_word(serial, "x").is_err());
        let shared = permissions::find("files.shared").unwrap();
        assert!(check_word(shared, "team=read-write").is_ok());
        assert!(check_word(shared, "../etc=read").is_err());
    }
}
