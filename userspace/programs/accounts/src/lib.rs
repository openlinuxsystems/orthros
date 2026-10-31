// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Dukovo's accounts (ADR 0218).
//!
//! An account is a name the owner chooses, and a password, which may be
//! empty: nothing else, and never an age, a birth date or an identity
//! (ADR 0126). Each lives in three files:
//!
//! - [`PASSWD`] and [`GROUP`], `/system/settings/passwd` and `group`,
//!   in Linux's formats: names, numbers, full names, homes and shells,
//!   which every program reads (the C library's `getpwnam`, `ls -l`,
//!   `id`), and no password;
//! - [`PASSWORDS`], `/system/state/authd/passwords`: the Argon2id hash of
//!   each password ([`hash`]), which only authd(8) reads.
//!
//! authd alone changes them, when asked over its socket ([`request`]);
//! `ob image --user` makes one in an image before its first boot. An
//! administrator is an account in the [`ADMINS`] group.
//!
//! Lines of the files that are not entries, or that this does not read,
//! are kept as they are when a file is written again, so that an edit by
//! hand is never lost.

#[cfg(feature = "hash")]
pub mod hash;
pub mod request;
#[cfg(test)]
mod tests;

use std::fmt;

/// The users: Linux's `/etc/passwd`, which `/etc` links to.
pub const PASSWD: &str = "/system/settings/passwd";
/// The groups: Linux's `/etc/group`.
pub const GROUP: &str = "/system/settings/group";
/// Where authd keeps its files, which no other program's view holds.
pub const STATE: &str = "/system/state/authd";
/// The password hashes: a line `name:hash:` each, as Linux's
/// `/etc/shadow` begins, where an empty hash is an empty password and
/// [`DISABLED`] in front marks a disabled account.
pub const PASSWORDS: &str = "/system/state/authd/passwords";
/// The numbers of removed accounts, which no account gets again (ADR
/// 0220): a line `number:name:removed`, the day of the removal, each.
pub const RETIRED: &str = "/system/state/authd/retired";
/// Where the homes of removed accounts are archived, if they are: no
/// account's name starts with a dot.
pub const REMOVED: &str = "/users/.removed";
/// The socket authd answers at.
pub const SOCKET: &str = "/system/live/authd/socket";
/// Where homes are: `/users/<name>`.
pub const USERS: &str = "/users";
/// The shell an account gets: nukesh(1), Dukovo's own.
pub const SHELL: &str = "/system/commands/nukesh";
/// The numbers of people's accounts, as on Debian and Fedora; those below
/// are the system's (root is 0) and, from Phase 13d, services'.
pub const FIRST_USER: u32 = 1000;
/// The last of them.
pub const LAST_USER: u32 = 59999;
/// The group of the administrators, who may elevate (13b.6).
pub const ADMINS: &str = "admins";
/// Its number, as Linux's `wheel` has.
pub const ADMINS_GROUP: u32 = 10;
/// The folders a new home has, as the security design names them: each
/// of the user's own, with `applications` for their bundles and
/// `app-data` for apps' private data (Phase 13d).
pub const HOME_FOLDERS: [&str; 9] = [
    "documents",
    "downloads",
    "desktop",
    "development",
    "music",
    "pictures",
    "videos",
    "applications",
    "app-data",
];
/// The longest name, in bytes, as Linux's useradd takes.
pub const NAME_MAX: usize = 32;
/// The longest password, in bytes. Argon2 takes any length; this bounds
/// what authd reads.
pub const PASSWORD_MAX: usize = 1024;
/// A password shorter than this, in characters, is taken after a warning.
pub const SHORT: usize = 8;

/// A user: a line of [`PASSWD`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    /// The name signed in with.
    pub name: String,
    /// Its number.
    pub uid: u32,
    /// Its group's number.
    pub gid: u32,
    /// Its full name, or whatever else its owner wrote there (GECOS).
    pub full_name: String,
    /// Its home.
    pub home: String,
    /// The shell a sign-in starts.
    pub shell: String,
}

impl User {
    /// A user from a line of [`PASSWD`]: `name:x:uid:gid:full name:home:shell`.
    pub fn parse(line: &str) -> Option<User> {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, uid, gid, full_name, home, shell] = fields[..] else { return None };
        if name.is_empty() {
            return None;
        }
        Some(User {
            name: name.into(),
            uid: uid.parse().ok()?,
            gid: gid.parse().ok()?,
            full_name: full_name.into(),
            home: home.into(),
            shell: shell.into(),
        })
    }

    /// Whether it is a person's account, not the system's.
    pub fn is_person(&self) -> bool {
        (FIRST_USER..=LAST_USER).contains(&self.uid)
    }
}

impl fmt::Display for User {
    /// Its line, with `x` where Linux's old files kept the password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let User { name, uid, gid, full_name, home, shell } = self;
        write!(f, "{name}:x:{uid}:{gid}:{full_name}:{home}:{shell}")
    }
}

/// A group: a line of [`GROUP`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// Its name.
    pub name: String,
    /// Its number.
    pub gid: u32,
    /// The users in it besides those whose own group it is.
    pub members: Vec<String>,
}

impl Group {
    /// A group from a line of [`GROUP`]: `name:x:gid:member,member`.
    pub fn parse(line: &str) -> Option<Group> {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, gid, members] = fields[..] else { return None };
        if name.is_empty() {
            return None;
        }
        Some(Group {
            name: name.into(),
            gid: gid.parse().ok()?,
            members: members.split(',').filter(|m| !m.is_empty()).map(String::from).collect(),
        })
    }
}

impl fmt::Display for Group {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:x:{}:{}", self.name, self.gid, self.members.join(","))
    }
}

/// A password's hash: a line of [`PASSWORDS`], `name:hash:`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Password {
    /// The user's name.
    pub name: String,
    /// The hash, a PHC string ([`hash`]); empty for an empty password.
    pub hash: String,
}

/// The mark in front of a disabled account's hash, as Linux's `usermod
/// -L` puts it there: the password is kept, and opens nothing until the
/// mark goes (ADR 0220).
pub const DISABLED: char = '!';

impl Password {
    /// A hash from a line of [`PASSWORDS`]: `name:hash:` and any fields
    /// after, as `/etc/shadow`'s lines have.
    pub fn parse(line: &str) -> Option<Password> {
        let mut fields = line.split(':');
        let name = fields.next().filter(|n| !n.is_empty())?;
        let hash = fields.next()?;
        Some(Password { name: name.into(), hash: hash.into() })
    }

    /// Whether its account is disabled: its hash has [`DISABLED`] in
    /// front.
    pub fn is_disabled(&self) -> bool {
        self.hash.starts_with(DISABLED)
    }

    /// The password's own hash, without the mark of a disabled account.
    pub fn own_hash(&self) -> &str {
        self.hash.strip_prefix(DISABLED).unwrap_or(&self.hash)
    }

    /// Disables its account, or enables it, keeping the password.
    pub fn set_disabled(&mut self, disabled: bool) {
        let own = self.own_hash().to_string();
        self.hash = if disabled { format!("{DISABLED}{own}") } else { own };
    }
}

impl fmt::Display for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:", self.name, self.hash)
    }
}

/// A line of one of the files: an entry, or one kept as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line<T> {
    /// An entry.
    Entry(T),
    /// A line that is not one, or that this does not read.
    Other(String),
}

/// One of the files, as its lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table<T> {
    /// Its lines, in order.
    pub lines: Vec<Line<T>>,
}

impl<T: fmt::Display> Table<T> {
    /// A file read from `text`, each line with `parse`.
    pub fn parse(text: &str, parse: impl Fn(&str) -> Option<T>) -> Table<T> {
        let lines = text
            .lines()
            .map(|line| parse(line).map_or_else(|| Line::Other(line.into()), Line::Entry))
            .collect();
        Table { lines }
    }

    /// Its entries.
    pub fn entries(&self) -> impl Iterator<Item = &T> {
        self.lines.iter().filter_map(|line| match line {
            Line::Entry(entry) => Some(entry),
            Line::Other(_) => None,
        })
    }

    /// Its entries, to change.
    pub fn entries_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.lines.iter_mut().filter_map(|line| match line {
            Line::Entry(entry) => Some(entry),
            Line::Other(_) => None,
        })
    }

    /// Keeps the entries `keep` says to, and every other line.
    pub fn retain_entries(&mut self, keep: impl Fn(&T) -> bool) {
        self.lines.retain(|line| match line {
            Line::Entry(entry) => keep(entry),
            Line::Other(_) => true,
        });
    }

    /// Adds `entry` at the end.
    pub fn push(&mut self, entry: T) {
        self.lines.push(Line::Entry(entry));
    }

    /// Its text: each line, and a newline after each.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for line in &self.lines {
            match line {
                Line::Entry(entry) => text.push_str(&entry.to_string()),
                Line::Other(other) => text.push_str(other),
            }
            text.push('\n');
        }
        text
    }
}

/// The users of [`PASSWD`]'s text.
pub fn users(text: &str) -> Table<User> {
    Table::parse(text, User::parse)
}

/// The groups of [`GROUP`]'s text.
pub fn groups(text: &str) -> Table<Group> {
    Table::parse(text, Group::parse)
}

/// The hashes of [`PASSWORDS`]'s text.
pub fn passwords(text: &str) -> Table<Password> {
    Table::parse(text, Password::parse)
}

/// Why `name` cannot be an account's name, if it cannot: from 1 to
/// [`NAME_MAX`] bytes of lowercase letters, digits, `_`, `-` and `.`,
/// starting with a letter or `_`, as Debian's adduser takes them, since
/// mail, shells and `chown name:group` misread others.
pub fn check_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first_fits = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    let rest_fits = chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-' | '.'));
    if name.is_empty() {
        Err("a name needs a letter at least".into())
    } else if name.len() > NAME_MAX {
        Err(format!("a name has at most {NAME_MAX} characters"))
    } else if !first_fits || !rest_fits {
        Err(format!(
            "`{name}` cannot be a name: use lowercase letters, digits, `_`, `-` and `.`, \
             starting with a letter"
        ))
    } else {
        Ok(())
    }
}

/// Why `full_name` cannot be an account's full name, if it cannot: it
/// cannot hold a colon or a line break, which end the passwd file's
/// fields and lines.
pub fn check_full_name(full_name: &str) -> Result<(), String> {
    if full_name.contains([':', '\n', '\r']) || full_name.len() > 256 {
        Err("a full name has at most 256 bytes, and no colon or line break".into())
    } else {
        Ok(())
    }
}

/// The plain warning a password gets before it is taken, if it gets one:
/// for an empty one, and one shorter than [`SHORT`] characters. Dukovo
/// forces no rules on passwords (ADR 0218): the person warned decides.
pub fn password_warning(name: &str, password: &str) -> Option<String> {
    let length = password.chars().count();
    if length == 0 {
        Some(format!("With no password, anyone at this machine can sign in as {name}."))
    } else if length < SHORT {
        Some(format!(
            "This password has {length} characters. A longer one is harder for someone else to \
             guess."
        ))
    } else {
        None
    }
}

/// The numbers in the text of [`RETIRED`].
pub fn retired(text: &str) -> Vec<u32> {
    text.lines().filter_map(|line| line.split(':').next()?.parse().ok()).collect()
}

/// The line of [`RETIRED`] for `user`, removed on `day`.
pub fn retired_line(user: &User, day: &str) -> String {
    format!("{}:{}:{day}\n", user.uid, user.name)
}

/// The number a new account gets: the lowest from [`FIRST_USER`] that no
/// user and no group has, and that no removed account had (`retired`), so
/// that the files a removed account left are never another's.
pub fn free_number(users: &Table<User>, groups: &Table<Group>, retired: &[u32]) -> Option<u32> {
    (FIRST_USER..=LAST_USER).find(|&n| {
        !users.entries().any(|u| u.uid == n)
            && !groups.entries().any(|g| g.gid == n)
            && !retired.contains(&n)
    })
}

/// Adds the account `name` to `users` and `groups`, as authd(8) and `ob
/// image --user` make one: with the lowest free number not `retired`, a
/// group of its own of that number, its home in [`USERS`] and the
/// [`SHELL`], and if `admin`, in the [`ADMINS`] group, which is made if
/// there is none. Its user, or why it cannot be made.
pub fn add(
    users: &mut Table<User>,
    groups: &mut Table<Group>,
    name: &str,
    full_name: &str,
    admin: bool,
    retired: &[u32],
) -> Result<User, String> {
    check_name(name)?;
    check_full_name(full_name)?;
    if users.entries().any(|u| u.name == name) {
        return Err(format!("there is an account {name} already"));
    }
    if groups.entries().any(|g| g.name == name) {
        return Err(format!("there is a group {name} already"));
    }
    let number = free_number(users, groups, retired).ok_or("every account number is taken")?;
    if admin {
        set_admin(groups, name, true)?;
    }
    groups.push(Group { name: name.into(), gid: number, members: Vec::new() });
    let user = User {
        name: name.into(),
        uid: number,
        gid: number,
        full_name: full_name.into(),
        home: format!("{USERS}/{name}"),
        shell: SHELL.into(),
    };
    users.push(user.clone());
    Ok(user)
}

/// Puts `name` in the [`ADMINS`] group, made if there is none, or takes
/// it out: not as its own group, which only another `gid` changes.
pub fn set_admin(groups: &mut Table<Group>, name: &str, admin: bool) -> Result<(), String> {
    if let Some(admins) = groups.entries_mut().find(|g| g.name == ADMINS) {
        let listed = admins.members.iter().any(|m| m == name);
        if admin && !listed {
            admins.members.push(name.into());
        } else if !admin {
            admins.members.retain(|m| m != name);
        }
        return Ok(());
    }
    if !admin {
        return Ok(());
    }
    if groups.entries().any(|g| g.gid == ADMINS_GROUP) {
        return Err(format!(
            "there is no {ADMINS} group, and its number, {ADMINS_GROUP}, is another group's"
        ));
    }
    groups.push(Group { name: ADMINS.into(), gid: ADMINS_GROUP, members: vec![name.into()] });
    Ok(())
}

/// Takes the account `name` out of `users` and `groups`: its user, its
/// own group (of its name and number) and its name in every group's
/// members, as Linux's `userdel` does. Its user, or why there is none.
/// Its home is the caller's to keep, archive or delete.
pub fn remove(
    users: &mut Table<User>,
    groups: &mut Table<Group>,
    name: &str,
) -> Result<User, String> {
    let user = users
        .entries()
        .find(|u| u.name == name)
        .cloned()
        .ok_or_else(|| format!("there is no account {name}"))?;
    users.retain_entries(|u| u.name != name);
    groups.retain_entries(|g| !(g.name == name && g.gid == user.gid));
    for group in groups.entries_mut() {
        group.members.retain(|m| m != name);
    }
    Ok(user)
}

/// Renames the account `name` to `new` in `users` and `groups`: its user,
/// its own group, its name in every group's members, and its home if it
/// is `/users/<name>`, as Linux's `usermod -l new -d /users/new -m`
/// does. The user as it was and as it is, or why it cannot be renamed.
/// Moving the home is the caller's.
pub fn rename(
    users: &mut Table<User>,
    groups: &mut Table<Group>,
    name: &str,
    new: &str,
) -> Result<(User, User), String> {
    check_name(new)?;
    if users.entries().any(|u| u.name == new) {
        return Err(format!("there is an account {new} already"));
    }
    if groups.entries().any(|g| g.name == new) {
        return Err(format!("there is a group {new} already"));
    }
    let user = users
        .entries_mut()
        .find(|u| u.name == name)
        .ok_or_else(|| format!("there is no account {name}"))?;
    let before = user.clone();
    user.name = new.into();
    if user.home == format!("{USERS}/{name}") {
        user.home = format!("{USERS}/{new}");
    }
    let after = user.clone();
    for group in groups.entries_mut() {
        if group.name == name && group.gid == before.gid {
            group.name = new.into();
        }
        for member in &mut group.members {
            if member == name {
                *member = new.into();
            }
        }
    }
    Ok((before, after))
}

/// Whether the user `name` is an administrator: in the [`ADMINS`] group,
/// or root.
pub fn is_admin(user: &User, groups: &Table<Group>) -> bool {
    user.uid == 0
        || groups
            .entries()
            .any(|g| g.name == ADMINS && (g.gid == user.gid || g.members.contains(&user.name)))
}

/// The groups `user` is in besides its own, as initgroups(3) finds
/// them: each group that lists it, once, without its own group, and at
/// most `max`.
pub fn other_groups(user: &User, groups: &Table<Group>, max: usize) -> Vec<u32> {
    let mut found: Vec<u32> = Vec::new();
    for group in groups.entries() {
        if group.gid != user.gid
            && group.members.contains(&user.name)
            && !found.contains(&group.gid)
        {
            found.push(group.gid);
        }
    }
    found.truncate(max);
    found
}

/// Overwrites `secret`'s bytes with zeros before it goes, so that a
/// password lasts in memory no longer than it is used.
pub fn wipe(secret: &mut Vec<u8>) {
    secret.fill(0);
    std::hint::black_box(&secret);
    secret.clear();
}
