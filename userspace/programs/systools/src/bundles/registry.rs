// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What bundled(8) keeps in `/system/state/bundled` (ADR 0223), which only
//! it writes: the bundles it installed, the key each ID is bound to, and
//! each person's developer key. Each is a file of lines, tab-separated,
//! read strictly: a line that does not read is an error, never skipped.

use std::fmt;

use super::minisign::PublicKey;

/// Where bundled keeps them.
pub const STATE: &str = "/system/state/bundled";
/// The bundles it installed.
pub const INSTALLED: &str = "installed";
/// The key each ID is bound to, in each place.
pub const BOUND: &str = "bound";
/// Each person's developer key.
pub const DEVELOPER_KEYS: &str = "developer-keys";

/// Where a bundle is installed: for everyone, or for one person.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Place {
    /// `/applications`, for everyone.
    Everyone,
    /// The `applications` folder of the home of the user with this number.
    User(u32),
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Place::Everyone => f.write_str("everyone"),
            Place::User(uid) => write!(f, "{uid}"),
        }
    }
}

impl Place {
    fn parse(text: &str) -> Result<Place, String> {
        if text == "everyone" {
            return Ok(Place::Everyone);
        }
        number(text).map(Place::User)
    }
}

/// A number as the files write it: decimal, no sign, no leading zero.
fn number<T: std::str::FromStr>(text: &str) -> Result<T, String> {
    let plain = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    plain.then(|| text.parse().ok()).flatten().ok_or_else(|| format!("{text:?} is not a number"))
}

/// A field: printable, without tabs or newlines.
fn field(text: &str) -> Result<&str, String> {
    if text.is_empty() || text.chars().any(char::is_control) {
        return Err(format!("{text:?} is not a field"));
    }
    Ok(text)
}

/// A bundle bundled installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub place: Place,
    pub id: String,
    pub version: String,
    pub key: PublicKey,
    /// The SHA-256 of its signed list, `files.sha256`, in hex.
    pub list: String,
    /// When, in seconds since 1970.
    pub when: u64,
}

/// An ID bound to a key in a place, from its first install there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub place: Place,
    pub id: String,
    pub key: PublicKey,
    /// Since when, in seconds since 1970.
    pub since: u64,
}

/// What bundled keeps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registry {
    pub installed: Vec<Installed>,
    pub bound: Vec<Bound>,
    /// Each person's developer key, by user number.
    pub developer_keys: Vec<(u32, PublicKey)>,
}

/// Splits `line` into exactly `N` tab-separated fields.
fn fields<const N: usize>(line: &str) -> Result<[&str; N], String> {
    let parts: Vec<&str> = line.split('\t').collect();
    let parts: [&str; N] = parts
        .try_into()
        .map_err(|parts: Vec<&str>| format!("{} fields where {N} belong", parts.len()))?;
    for part in parts {
        field(part)?;
    }
    Ok(parts)
}

/// Reads each line of `text`, a file's, with `read`; says which line is
/// wrong.
fn lines<T>(
    name: &str,
    text: &str,
    read: impl Fn(&str) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(format!("{name}: its last line has no end"));
    }
    text.lines()
        .enumerate()
        .map(|(n, line)| read(line).map_err(|why| format!("{name}:{}: {why}", n + 1)))
        .collect()
}

fn checked_id(id: &str) -> Result<String, String> {
    manifest::bundle::check_id(id)?;
    Ok(id.into())
}

fn checked_hex(text: &str) -> Result<String, String> {
    let hex = text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    hex.then(|| text.into()).ok_or_else(|| format!("{text:?} is not a SHA-256"))
}

impl Registry {
    /// Reads the three files' texts.
    pub fn read(installed: &str, bound: &str, developer_keys: &str) -> Result<Registry, String> {
        let installed = lines(INSTALLED, installed, |line| {
            let [place, id, version, key, list, when] = fields(line)?;
            Ok(Installed {
                place: Place::parse(place)?,
                id: checked_id(id)?,
                version: version.into(),
                key: PublicKey::decode(key)?,
                list: checked_hex(list)?,
                when: number(when)?,
            })
        })?;
        let bound = lines(BOUND, bound, |line| {
            let [place, id, key, since] = fields(line)?;
            Ok(Bound {
                place: Place::parse(place)?,
                id: checked_id(id)?,
                key: PublicKey::decode(key)?,
                since: number(since)?,
            })
        })?;
        let developer_keys = lines(DEVELOPER_KEYS, developer_keys, |line| {
            let [user, key] = fields(line)?;
            Ok((number(user)?, PublicKey::decode(key)?))
        })?;
        let registry = Registry { installed, bound, developer_keys };
        registry.check()?;
        Ok(registry)
    }

    /// Each kind at most once: a bundle per place and ID, a binding per
    /// place and ID, a developer key per person.
    fn check(&self) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for i in &self.installed {
            if !seen.insert((0, i.place, i.id.as_str())) {
                return Err(format!("{INSTALLED}: {} is there twice for {}", i.id, i.place));
            }
        }
        for b in &self.bound {
            if !seen.insert((1, b.place, b.id.as_str())) {
                return Err(format!("{BOUND}: {} is there twice for {}", b.id, b.place));
            }
        }
        for (user, _) in &self.developer_keys {
            if !seen.insert((2, Place::User(*user), "")) {
                return Err(format!("{DEVELOPER_KEYS}: user {user} is there twice"));
            }
        }
        Ok(())
    }

    /// The three files' texts, in [`Registry::read`]'s order, sorted.
    pub fn texts(&self) -> [String; 3] {
        let mut installed: Vec<String> = self
            .installed
            .iter()
            .map(|i| {
                let key = i.key.encoded();
                format!("{}\t{}\t{}\t{key}\t{}\t{}\n", i.place, i.id, i.version, i.list, i.when)
            })
            .collect();
        installed.sort();
        let mut bound: Vec<String> = self
            .bound
            .iter()
            .map(|b| format!("{}\t{}\t{}\t{}\n", b.place, b.id, b.key.encoded(), b.since))
            .collect();
        bound.sort();
        let mut keys: Vec<String> = self
            .developer_keys
            .iter()
            .map(|(user, key)| format!("{user}\t{}\n", key.encoded()))
            .collect();
        keys.sort();
        [installed.concat(), bound.concat(), keys.concat()]
    }

    /// The bundle `id` installed in `place`.
    pub fn installed(&self, place: Place, id: &str) -> Option<&Installed> {
        self.installed.iter().find(|i| i.place == place && i.id == id)
    }

    /// The key `id` is bound to in `place`.
    pub fn bound(&self, place: Place, id: &str) -> Option<&Bound> {
        self.bound.iter().find(|b| b.place == place && b.id == id)
    }

    /// The developer key of the user `user`.
    pub fn developer_key(&self, user: u32) -> Option<PublicKey> {
        self.developer_keys.iter().find(|(u, _)| *u == user).map(|(_, key)| *key)
    }

    /// Records `installed`, in place of what was there for its place and
    /// ID, and binds its ID to its key there if it is not bound yet.
    pub fn record(&mut self, installed: Installed) {
        if self.bound(installed.place, &installed.id).is_none() {
            self.bound.push(Bound {
                place: installed.place,
                id: installed.id.clone(),
                key: installed.key,
                since: installed.when,
            });
        }
        self.installed.retain(|i| i.place != installed.place || i.id != installed.id);
        self.installed.push(installed);
    }

    /// Forgets the bundle `id` installed in `place`; its binding stays.
    pub fn forget(&mut self, place: Place, id: &str) -> Option<Installed> {
        let at = self.installed.iter().position(|i| i.place == place && i.id == id)?;
        Some(self.installed.remove(at))
    }

    /// Keeps `key` as the developer key of `user`.
    pub fn set_developer_key(&mut self, user: u32, key: PublicKey) {
        self.developer_keys.retain(|(u, _)| *u != user);
        self.developer_keys.push((user, key));
    }
}

/// Why a bundle's key is trusted, or that it is not known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// Its ID is bound to this key in the place, since then.
    Bound { since: u64 },
    /// Its ID is bound to this key for everyone, since then.
    BoundForEveryone { since: u64 },
    /// It is the asker's own developer key.
    Developer,
    /// No ID is bound here yet, and the key is not one this computer
    /// knows: it installs after a warning.
    Unknown,
}

impl Registry {
    /// Whether `key` may sign `id` in `place` for user `asker`: its trust,
    /// or why not, when the ID is bound to another key.
    pub fn trust(
        &self,
        place: Place,
        asker: u32,
        id: &str,
        key: &PublicKey,
    ) -> Result<Trust, String> {
        let everyone = self.bound(Place::Everyone, id);
        let own = if place == Place::Everyone { None } else { self.bound(place, id) };
        for (binding, whose) in [(everyone, "for everyone"), (own, "here")] {
            if let Some(b) = binding
                && b.key != *key
            {
                return Err(format!(
                    "{id} is signed by key {}, but it is bound to key {} {whose}, since its first \
                     install: only its publisher's key may install it",
                    key.encoded(),
                    b.key.encoded()
                ));
            }
        }
        if let Some(b) = own {
            return Ok(Trust::Bound { since: b.since });
        }
        if let Some(b) = everyone {
            return Ok(if place == Place::Everyone {
                Trust::Bound { since: b.since }
            } else {
                Trust::BoundForEveryone { since: b.since }
            });
        }
        if self.developer_key(asker) == Some(*key) {
            return Ok(Trust::Developer);
        }
        Ok(Trust::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundles::minisign::SecretKey;

    fn key(n: u8) -> PublicKey {
        let mut random = [n; 40];
        random[0] = n.wrapping_add(1);
        SecretKey::generate(&random).unwrap().public()
    }

    fn installed(place: Place, id: &str, n: u8) -> Installed {
        Installed {
            place,
            id: id.into(),
            version: "1.0.0".into(),
            key: key(n),
            list: "ab".repeat(32),
            when: 1_793_000_000,
        }
    }

    #[test]
    fn the_files_read_back() {
        let mut registry = Registry::default();
        registry.record(installed(Place::User(1000), "org.example.notes", 1));
        registry.record(installed(Place::Everyone, "org.example.tea", 2));
        registry.set_developer_key(1000, key(1));
        let [a, b, c] = registry.texts();
        let again = Registry::read(&a, &b, &c).unwrap();
        assert_eq!(again.texts(), registry.texts());
        assert_eq!(again.installed.len(), 2);
        assert_eq!(again.bound.len(), 2);
        assert!(a.starts_with("1000\torg.example.notes\t1.0.0\tRW"));
        assert!(b.contains("everyone\torg.example.tea\tRW"));
        assert_eq!(again.developer_key(1000), Some(key(1)));
        assert_eq!(Registry::read("", "", "").unwrap(), Registry::default());
    }

    #[test]
    fn a_wrong_line_is_refused() {
        let good = Registry::default().texts();
        let line = "1000\torg.example.notes\t1.0.0\t".to_string() + &key(1).encoded();
        let list = "ab".repeat(32);
        for bad in [
            format!("{line}\t{list}\t17"),
            format!("{line}\t{list}\t17\n{line}\t{list}\t18\n"),
            format!("{line}\t{list}\n"),
            format!("{line}\t{list}\t017\n"),
            format!("{line}\t{}\t17\n", "AB".repeat(32)),
            format!("01000{}\t{list}\t17\n", &line[4..]),
            format!("{}\t{list}\t17\n", line.replace("org.example.notes", "Notes")),
            format!("{}\t{list}\t17\n", line.replace("1.0.0", "1.0.0\u{1b}")),
            format!("all{}\t{list}\t17\n", &line[4..]),
        ] {
            assert!(Registry::read(&bad, &good[1], &good[2]).is_err(), "{bad:?}");
        }
        let two = format!("5\t{}\n5\t{}\n", key(1).encoded(), key(2).encoded());
        assert!(Registry::read("", "", &two).unwrap_err().contains("twice"));
    }

    #[test]
    fn keys_are_trusted_as_bound() {
        let mut registry = Registry::default();
        let alice = Place::User(1000);
        let bob = Place::User(1001);
        let id = "org.example.notes";
        // Nothing bound: her developer key is hers alone to trust.
        registry.set_developer_key(1000, key(1));
        assert_eq!(registry.trust(alice, 1000, id, &key(1)), Ok(Trust::Developer));
        assert_eq!(registry.trust(bob, 1001, id, &key(1)), Ok(Trust::Unknown));
        assert_eq!(registry.trust(alice, 1000, id, &key(2)), Ok(Trust::Unknown));
        // Bound for her: only that key, for her; bob is free.
        registry.record(installed(alice, id, 1));
        assert_eq!(
            registry.trust(alice, 1000, id, &key(1)),
            Ok(Trust::Bound { since: 1_793_000_000 })
        );
        assert!(registry.trust(alice, 1000, id, &key(2)).unwrap_err().contains("bound to key"));
        assert_eq!(registry.trust(bob, 1001, id, &key(2)), Ok(Trust::Unknown));
        // Bound for everyone: that key, for everyone and each person.
        registry.record(installed(Place::Everyone, "org.example.tea", 3));
        let tea = "org.example.tea";
        assert!(registry.trust(bob, 1001, tea, &key(2)).is_err());
        assert_eq!(
            registry.trust(bob, 1001, tea, &key(3)),
            Ok(Trust::BoundForEveryone { since: 1_793_000_000 })
        );
        assert_eq!(
            registry.trust(Place::Everyone, 0, tea, &key(3)),
            Ok(Trust::Bound { since: 1_793_000_000 })
        );
        // Forgetting keeps the binding.
        assert!(registry.forget(alice, id).is_some());
        assert!(registry.installed(alice, id).is_none());
        assert!(registry.trust(alice, 1000, id, &key(2)).is_err());
    }
}
