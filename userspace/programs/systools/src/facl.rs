// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! getfacl(1) and setfacl(1): POSIX access lists (ADR 0256), shown and
//! changed as the acl package's tools do, in the same text, through the
//! extended attributes `system.posix_acl_access` and
//! `system.posix_acl_default` in Linux's `posix_acl_xattr` form.
//!
//! A list names the owner, other users, the group, other groups, the
//! mask and everyone else, with read, write and execute for each. The
//! kernel checks it (radix-directory(2)) and keeps the mode's permission
//! bits in step: the owner's, the mask's (or the group's) and the
//! others'. setfacl works out the whole new list, as libacl does, and
//! sets it at once.

use std::ffi::{CString, OsString, c_char, c_int, c_void};
use std::fmt::Write as _;
use std::io::{self, BufRead, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use crate::args::{Arg, Args};
use crate::util::{self, Fail, fail, usage};

/// The attribute of the list that decides.
const ACCESS: &str = "system.posix_acl_access";
/// The attribute of a directory's list for what is made in it.
const DEFAULT: &str = "system.posix_acl_default";

// The C library's extended attribute calls (sys/xattr.h), which the
// libc crate does not declare for Radix.
unsafe extern "C" {
    fn getxattr(path: *const c_char, name: *const c_char, value: *mut c_void, size: usize)
    -> isize;
    fn lgetxattr(
        path: *const c_char,
        name: *const c_char,
        value: *mut c_void,
        size: usize,
    ) -> isize;
    fn setxattr(
        path: *const c_char,
        name: *const c_char,
        value: *const c_void,
        size: usize,
        flags: c_int,
    ) -> c_int;
    fn removexattr(path: *const c_char, name: *const c_char) -> c_int;
}

/// What an entry names, in the order of a list: Linux's tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tag {
    UserObj = 0x01,
    User = 0x02,
    GroupObj = 0x04,
    Group = 0x08,
    Mask = 0x10,
    Other = 0x20,
}

impl Tag {
    fn from_bits(bits: u16) -> Option<Tag> {
        Some(match bits {
            0x01 => Tag::UserObj,
            0x02 => Tag::User,
            0x04 => Tag::GroupObj,
            0x08 => Tag::Group,
            0x10 => Tag::Mask,
            0x20 => Tag::Other,
            _ => return None,
        })
    }

    /// Whether it has a number: a named user's or group's.
    fn named(self) -> bool {
        matches!(self, Tag::User | Tag::Group)
    }

    /// Whether the mask limits it: the group class.
    fn masked(self) -> bool {
        matches!(self, Tag::User | Tag::GroupObj | Tag::Group)
    }

    fn word(self) -> &'static str {
        match self {
            Tag::UserObj | Tag::User => "user",
            Tag::GroupObj | Tag::Group => "group",
            Tag::Mask => "mask",
            Tag::Other => "other",
        }
    }
}

/// An entry: what it names (with its number if named) and its read (4),
/// write (2) and execute (1) permissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    tag: Tag,
    id: u32,
    perm: u8,
}

/// Linux's number for an entry that names no one.
const NO_ID: u32 = u32::MAX;

/// An access list, its entries in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Acl {
    entries: Vec<Entry>,
}

impl Acl {
    /// The list a mode's permission bits say.
    fn from_mode(mode: u32) -> Acl {
        let perm = |shift: u32| ((mode >> shift) & 7) as u8;
        Acl {
            entries: vec![
                Entry { tag: Tag::UserObj, id: NO_ID, perm: perm(6) },
                Entry { tag: Tag::GroupObj, id: NO_ID, perm: perm(3) },
                Entry { tag: Tag::Other, id: NO_ID, perm: perm(0) },
            ],
        }
    }

    /// The list in an attribute's value; none for one with no entries or
    /// one that does not read.
    fn from_value(value: &[u8]) -> Option<Acl> {
        if value.len() < 4
            || !(value.len() - 4).is_multiple_of(8)
            || value[..4] != 2u32.to_le_bytes()
        {
            return None;
        }
        let mut acl = Acl::default();
        for raw in value[4..].as_chunks::<8>().0 {
            let tag = Tag::from_bits(u16::from_le_bytes([raw[0], raw[1]]))?;
            let perm = u16::from_le_bytes([raw[2], raw[3]]);
            let id = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
            let id = if tag.named() { id } else { NO_ID };
            acl.set(Entry { tag, id, perm: (perm & 7) as u8 });
        }
        (!acl.entries.is_empty()).then_some(acl)
    }

    /// Its attribute value.
    fn to_value(&self) -> Vec<u8> {
        let mut value = 2u32.to_le_bytes().to_vec();
        for e in &self.entries {
            value.extend_from_slice(&(e.tag as u16).to_le_bytes());
            value.extend_from_slice(&u16::from(e.perm).to_le_bytes());
            value.extend_from_slice(&e.id.to_le_bytes());
        }
        value
    }

    fn get(&self, tag: Tag, id: u32) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag && e.id == id)
    }

    /// Sets an entry, in its place.
    fn set(&mut self, entry: Entry) {
        match self.entries.binary_search_by_key(&(entry.tag, entry.id), |e| (e.tag, e.id)) {
            Ok(i) => self.entries[i] = entry,
            Err(i) => self.entries.insert(i, entry),
        }
    }

    fn remove(&mut self, tag: Tag, id: u32) {
        self.entries.retain(|e| !(e.tag == tag && e.id == id));
    }

    fn mask(&self) -> Option<u8> {
        self.get(Tag::Mask, NO_ID).map(|e| e.perm)
    }

    /// Whether it names a user or group, and so needs a mask.
    fn has_named(&self) -> bool {
        self.entries.iter().any(|e| e.tag.named())
    }

    /// Whether it says no more than a mode does.
    fn is_base(&self) -> bool {
        self.entries.iter().all(|e| matches!(e.tag, Tag::UserObj | Tag::GroupObj | Tag::Other))
    }

    /// Its owner's, group's and others' entries alone, as `setfacl -b`
    /// leaves them.
    fn base(&self) -> Acl {
        Acl {
            entries: self
                .entries
                .iter()
                .filter(|e| matches!(e.tag, Tag::UserObj | Tag::GroupObj | Tag::Other))
                .copied()
                .collect(),
        }
    }

    /// The mask that lets every entry it limits have what it names: all
    /// their permissions together (libacl's `acl_calc_mask`).
    fn calc_mask(&self) -> u8 {
        self.entries.iter().filter(|e| e.tag.masked()).fold(0, |m, e| m | e.perm)
    }

    /// The first entry missing for a whole list, counted from 1 as
    /// setfacl counts them; none if it is whole.
    fn missing(&self) -> Option<usize> {
        let has = |tag| self.entries.iter().any(|e| e.tag == tag);
        if !has(Tag::UserObj) {
            return Some(1);
        }
        if !has(Tag::GroupObj) {
            return Some(2);
        }
        if !has(Tag::Other) {
            return Some(3);
        }
        None
    }

    /// The short text `setfacl --test` shows, each entry with `prefix`.
    fn short_text(&self, prefix: &str) -> Vec<String> {
        self.entries.iter().map(|&e| format!("{prefix}{}", entry_text(e, true, true))).collect()
    }
}

/// `rwx` with `-` for each permission it has not.
fn perm_text(perm: u8) -> String {
    let mut text = String::with_capacity(3);
    text.push(if perm & 4 != 0 { 'r' } else { '-' });
    text.push(if perm & 2 != 0 { 'w' } else { '-' });
    text.push(if perm & 1 != 0 { 'x' } else { '-' });
    text
}

/// An entry as text: `user:alice:rw-`, with the user's or group's name
/// unless `numeric`, and one letter for the tag if `short`.
fn entry_text(e: Entry, numeric: bool, short: bool) -> String {
    let tag = if short { &e.tag.word()[..1] } else { e.tag.word() };
    let who = match e.tag {
        Tag::User if numeric => e.id.to_string(),
        Tag::User => util::user_name(e.id),
        Tag::Group if numeric => e.id.to_string(),
        Tag::Group => util::group_name(e.id),
        _ => String::new(),
    };
    format!("{tag}:{who}:{}", perm_text(e.perm))
}

/// Permissions as an entry of setfacl gives them: `X` gives execute to a
/// directory, or to a file someone may execute already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Perm {
    bits: u8,
    x_if: bool,
}

impl Perm {
    fn on(self, is_dir: bool, mode: u32) -> u8 {
        if self.x_if && (is_dir || mode & 0o111 != 0) { self.bits | 1 } else { self.bits }
    }
}

/// An entry of setfacl's command line: a default list's if `default`,
/// with no number for the owner's, group's, mask's and others', and no
/// permissions in one to remove.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Spec {
    default: bool,
    tag: Tag,
    id: u32,
    perm: Option<Perm>,
}

/// Why an entry did not read: where, counted from 1 as setfacl counts
/// characters, or that it ended too soon.
#[derive(Debug, PartialEq, Eq)]
enum SpecError {
    Near(usize),
    Incomplete,
}

/// Reads entries of acl(5)'s text, separated by commas or lines, with
/// comments from `#` to the end of a line: `[d[efault]:]u[ser]:[name]`,
/// `g[roup]:[name]`, `m[ask][:]` and `o[ther][:]`, then `:` and
/// permissions (`rwxX-` or a digit) if `perms`, which entries to remove
/// have not.
fn parse_specs(text: &str, perms: bool) -> Result<Vec<Spec>, SpecError> {
    let mut specs = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let content = line.split('#').next().unwrap_or("");
        let mut at = offset;
        for raw in content.split(',') {
            let trimmed = raw.trim_matches(|c: char| c.is_whitespace());
            let lead = raw.len() - raw.trim_start().len();
            if !trimmed.is_empty() {
                specs.push(parse_spec(trimmed, at + lead, perms)?);
            }
            at += raw.len() + 1;
        }
        offset += line.len();
    }
    Ok(specs)
}

/// One entry of [`parse_specs`], which starts at byte `at` of the text.
fn parse_spec(text: &str, at: usize, perms: bool) -> Result<Spec, SpecError> {
    let fields: Vec<&str> = text.split(':').collect();
    let mut i = 0;
    let default = matches!(fields[0], "d" | "default");
    if default {
        i += 1;
    }
    let near = |field: usize| {
        let before: usize = fields[..field].iter().map(|f| f.len() + 1).sum();
        SpecError::Near(at + before + 1)
    };
    let Some(&word) = fields.get(i) else { return Err(SpecError::Incomplete) };
    let (tag_named, tag_obj, takes_name) = match word {
        "u" | "user" => (Tag::User, Tag::UserObj, true),
        "g" | "group" => (Tag::Group, Tag::GroupObj, true),
        "m" | "mask" => (Tag::Mask, Tag::Mask, false),
        "o" | "other" => (Tag::Other, Tag::Other, false),
        _ => return Err(near(i)),
    };
    i += 1;
    // The name: none for the mask and the others, which may leave out
    // its field (`m:rw` as well as `m::rw`).
    let mut id = NO_ID;
    let mut tag = tag_obj;
    let rest = fields.len() - i;
    let has_name_field = takes_name || rest == 2 || (!perms && rest == 1);
    if has_name_field && i < fields.len() {
        let name = fields[i];
        if !name.is_empty() {
            if !takes_name {
                return Err(near(i));
            }
            let found =
                if tag_named == Tag::User { util::user_id(name) } else { util::group_id(name) };
            id = found.ok_or_else(|| near(i))?;
            tag = tag_named;
        }
        i += 1;
    }
    if !perms {
        return if i >= fields.len() || (i + 1 == fields.len() && fields[i].is_empty()) {
            Ok(Spec { default, tag, id, perm: None })
        } else {
            Err(near(i))
        };
    }
    let Some(&text) = fields.get(i) else { return Err(SpecError::Incomplete) };
    if i + 1 != fields.len() {
        return Err(near(i + 1));
    }
    let perm = parse_perm(text).ok_or_else(|| {
        let bad = text.find(|c: char| !"rwxX-".contains(c)).unwrap_or(0);
        let before: usize = fields[..i].iter().map(|f| f.len() + 1).sum();
        SpecError::Near(at + before + bad + 1)
    })?;
    Ok(Spec { default, tag, id, perm: Some(perm) })
}

/// Permissions as text: letters of `rwxX-`, or one octal digit.
fn parse_perm(text: &str) -> Option<Perm> {
    if text.is_empty() {
        return None;
    }
    if text.len() == 1
        && let Some(d) = text.chars().next().and_then(|c| c.to_digit(8))
    {
        return Some(Perm { bits: d as u8, x_if: false });
    }
    let mut perm = Perm { bits: 0, x_if: false };
    for c in text.chars() {
        match c {
            'r' => perm.bits |= 4,
            'w' => perm.bits |= 2,
            'x' => perm.bits |= 1,
            'X' => perm.x_if = true,
            '-' => {}
            _ => return None,
        }
    }
    Some(perm)
}

/// A file's attribute `name` read, following a symbolic link unless
/// `physical`: none if it has none, or its filesystem keeps none.
fn read_attribute(path: &Path, name: &str, physical: bool) -> io::Result<Option<Vec<u8>>> {
    let c_path = c_string(path.as_os_str().as_bytes())?;
    let c_name = c_string(name.as_bytes())?;
    let mut buffer = vec![0u8; 256];
    loop {
        // SAFETY: the path and name are C strings, and the buffer is as
        // long as the size given.
        let n = unsafe {
            let get = if physical { lgetxattr } else { getxattr };
            get(c_path.as_ptr(), c_name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len())
        };
        if n >= 0 {
            buffer.truncate(n as usize);
            return Ok(Some(buffer));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENODATA | libc::EOPNOTSUPP) => return Ok(None),
            Some(libc::ERANGE) if buffer.len() < 65536 => buffer.resize(buffer.len() * 4, 0),
            _ => return Err(error),
        }
    }
}

fn c_string(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// A file's access list, from its attribute or its mode.
fn access_list(path: &Path, mode: u32, physical: bool) -> io::Result<Acl> {
    Ok(read_attribute(path, ACCESS, physical)?
        .and_then(|v| Acl::from_value(&v))
        .unwrap_or_else(|| Acl::from_mode(mode)))
}

/// A directory's default list, if it has one.
fn default_list(path: &Path, physical: bool) -> io::Result<Option<Acl>> {
    Ok(read_attribute(path, DEFAULT, physical)?.and_then(|v| Acl::from_value(&v)))
}

/// Sets a file's access list. One the mode says all of is set as a mode
/// where the filesystem keeps no lists, as libacl does.
fn write_access(path: &Path, acl: &Acl, mode: u32) -> io::Result<()> {
    let c_path = c_string(path.as_os_str().as_bytes())?;
    let c_name = c_string(ACCESS.as_bytes())?;
    let value = acl.to_value();
    // SAFETY: C strings, and the value's bytes with their length.
    let r = unsafe {
        setxattr(c_path.as_ptr(), c_name.as_ptr(), value.as_ptr().cast(), value.len(), 0)
    };
    if r == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EOPNOTSUPP) && acl.is_base() {
        let perm = |tag| u32::from(acl.get(tag, NO_ID).map_or(0, |e| e.perm));
        let bits = (mode & 0o7000)
            | (perm(Tag::UserObj) << 6)
            | (perm(Tag::GroupObj) << 3)
            | perm(Tag::Other);
        return std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits));
    }
    Err(error)
}

/// Sets a directory's default list, or takes it away given none.
fn write_default(path: &Path, acl: Option<&Acl>) -> io::Result<()> {
    let c_path = c_string(path.as_os_str().as_bytes())?;
    let c_name = c_string(DEFAULT.as_bytes())?;
    // SAFETY: C strings, and the value's bytes with their length.
    let r = unsafe {
        match acl {
            Some(acl) => {
                let value = acl.to_value();
                setxattr(c_path.as_ptr(), c_name.as_ptr(), value.as_ptr().cast(), value.len(), 0)
            }
            None => removexattr(c_path.as_ptr(), c_name.as_ptr()),
        }
    };
    let error = io::Error::last_os_error();
    if r == 0
        || (acl.is_none() && matches!(error.raw_os_error(), Some(libc::ENODATA | libc::EOPNOTSUPP)))
    {
        return Ok(());
    }
    Err(error)
}

/// Which symbolic links a walk follows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Walk {
    /// Those named, into directories (neither -L nor -P).
    Named,
    /// -L: every one.
    Logical,
    /// -P: none, and links are skipped.
    Physical,
}

/// Runs `apply` on `path` and, if `recursive`, on what is under it,
/// telling it whether the file was named. Returns false if any failed.
fn walk(
    tool: &str,
    path: &Path,
    named: bool,
    recursive: bool,
    how: Walk,
    apply: &mut dyn FnMut(&Path, bool) -> bool,
) -> bool {
    let link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if link && !named && how != Walk::Logical {
        return true;
    }
    let mut ok = apply(path, named);
    let into = recursive
        && (!link || how == Walk::Logical || (named && how == Walk::Named))
        && std::fs::metadata(path).is_ok_and(|m| m.is_dir());
    if into {
        match std::fs::read_dir(path) {
            Ok(entries) => {
                let mut names: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
                names.sort();
                for name in names {
                    ok &= walk(tool, &name, false, recursive, how, apply);
                }
            }
            Err(e) => {
                eprintln!("{tool}: {}: {}", path.display(), util::strip_os_error(&e));
                ok = false;
            }
        }
    }
    ok
}

/// What getfacl shows.
struct Show {
    access: bool,
    default: bool,
    header: bool,
    /// Effective rights: for every limited entry (-e), for none (-E), or
    /// where the mask takes some away.
    effective: Option<bool>,
    skip_base: bool,
    numeric: bool,
    absolute: bool,
}

/// getfacl's text of one file's lists: its header, the access list and
/// the default list, each entry an `#effective:` after it where the mask
/// takes permissions away.
fn listing(
    shown: &str,
    meta: &std::fs::Metadata,
    access: &Acl,
    default: Option<&Acl>,
    show: &Show,
) -> String {
    let mut out = String::new();
    if show.header {
        let owner = if show.numeric { meta.uid().to_string() } else { util::user_name(meta.uid()) };
        let group =
            if show.numeric { meta.gid().to_string() } else { util::group_name(meta.gid()) };
        let _ = writeln!(out, "# file: {shown}\n# owner: {owner}\n# group: {group}");
        let mode = meta.mode();
        if mode & 0o7000 != 0 {
            let flag = |bit: u32, c: char| if mode & bit != 0 { c } else { '-' };
            let _ = writeln!(
                out,
                "# flags: {}{}{}",
                flag(0o4000, 's'),
                flag(0o2000, 's'),
                flag(0o1000, 't')
            );
        }
    }
    let mut list = |acl: &Acl, prefix: &str| {
        let mask = acl.mask();
        for e in &acl.entries {
            let text = entry_text(*e, show.numeric, false);
            let _ = write!(out, "{prefix}{text}");
            if let Some(mask) = mask.filter(|_| e.tag.masked()) {
                let cut = e.perm & mask;
                if show.effective.unwrap_or(cut != e.perm) {
                    let _ = write!(out, "\t#effective:{}", perm_text(cut));
                }
            }
            out.push('\n');
        }
    };
    if show.access {
        list(access, "");
    }
    if show.default
        && let Some(default) = default
    {
        list(default, "default:");
    }
    out.push('\n');
    out
}

/// getfacl(1).
pub fn getfacl(args: &mut Args) -> Result<i32, Fail> {
    let mut show = Show {
        access: true,
        default: true,
        header: true,
        effective: None,
        skip_base: false,
        numeric: false,
        absolute: false,
    };
    let (mut only_access, mut only_default) = (false, false);
    let mut recursive = false;
    let mut how = Walk::Named;
    let mut files: Vec<OsString> = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('a') => only_access = true,
            Arg::Long(l) if l == "access" => only_access = true,
            Arg::Short('d') => only_default = true,
            Arg::Long(l) if l == "default" => only_default = true,
            Arg::Short('c') => show.header = false,
            Arg::Long(l) if l == "omit-header" => show.header = false,
            Arg::Short('e') => show.effective = Some(true),
            Arg::Long(l) if l == "all-effective" => show.effective = Some(true),
            Arg::Short('E') => show.effective = Some(false),
            Arg::Long(l) if l == "no-effective" => show.effective = Some(false),
            Arg::Short('s') => show.skip_base = true,
            Arg::Long(l) if l == "skip-base" => show.skip_base = true,
            Arg::Short('R') => recursive = true,
            Arg::Long(l) if l == "recursive" => recursive = true,
            Arg::Short('L') => how = Walk::Logical,
            Arg::Long(l) if l == "logical" => how = Walk::Logical,
            Arg::Short('P') => how = Walk::Physical,
            Arg::Long(l) if l == "physical" => how = Walk::Physical,
            Arg::Short('n') => show.numeric = true,
            Arg::Long(l) if l == "numeric" => show.numeric = true,
            Arg::Short('p') => show.absolute = true,
            Arg::Long(l) if l == "absolute-names" => show.absolute = true,
            Arg::Operand(o) => files.push(o),
            other => return Err(args.unknown(&other)),
        }
    }
    if only_access || only_default {
        show.access = only_access;
        show.default = only_default;
    }
    if files.is_empty() {
        return Err(usage("no file named".into()));
    }
    let mut told = false;
    let mut out = io::stdout().lock();
    let mut ok = true;
    for file in &files {
        // `-` reads the names from standard input, as Linux's getfacl.
        let names: Vec<OsString> = if file.as_bytes() == b"-" {
            io::stdin().lock().split(b'\n').map_while(Result::ok).map(OsString::from_vec).collect()
        } else {
            vec![file.clone()]
        };
        for name in names {
            ok &= walk("getfacl", Path::new(&name), true, recursive, how, &mut |path, _| {
                let physical = how == Walk::Physical;
                let meta = match if physical {
                    std::fs::symlink_metadata(path)
                } else {
                    std::fs::metadata(path)
                } {
                    Ok(meta) => meta,
                    Err(e) => {
                        eprintln!("getfacl: {}: {}", path.display(), util::strip_os_error(&e));
                        return false;
                    }
                };
                let lists = access_list(path, meta.mode(), physical).and_then(|access| {
                    let default = if meta.is_dir() { default_list(path, physical)? } else { None };
                    Ok((access, default))
                });
                let (access, default) = match lists {
                    Ok(lists) => lists,
                    Err(e) => {
                        eprintln!("getfacl: {}: {}", path.display(), util::strip_os_error(&e));
                        return false;
                    }
                };
                if show.skip_base && access.is_base() && default.is_none() {
                    return true;
                }
                let text = path.to_string_lossy();
                let shown = match text.strip_prefix('/') {
                    Some(rest) if !show.absolute => {
                        if !told {
                            eprintln!("getfacl: Removing leading '/' from absolute path names");
                            told = true;
                        }
                        if rest.is_empty() { "." } else { rest }
                    }
                    _ => &text,
                };
                let _ = out
                    .write_all(listing(shown, &meta, &access, default.as_ref(), &show).as_bytes());
                true
            });
        }
    }
    Ok(i32::from(!ok))
}

/// One change setfacl makes, in the order given.
enum Change {
    /// -m: set entries.
    Modify(Vec<Spec>),
    /// -x: remove entries.
    Remove(Vec<Spec>),
    /// --set: the lists anew.
    Set(Vec<Spec>),
    /// -b: every entry but the owner's, group's and others', and the
    /// default list.
    RemoveAll,
    /// -k: the default list.
    RemoveDefault,
}

/// What setfacl does to each file.
struct Plan {
    changes: Vec<Change>,
    /// -d: every entry is the default list's.
    default: bool,
    /// -n: the mask is not worked out anew.
    no_mask: bool,
    test: bool,
}

/// The new lists of a file whose lists are `access` and `default`, after
/// `plan`'s changes, with masks worked out anew: an error as setfacl
/// gives it.
fn changed(
    plan: &Plan,
    access: &Acl,
    default: Option<&Acl>,
    is_dir: bool,
    named: bool,
    mode: u32,
) -> Result<(Acl, Option<Acl>, bool, bool), String> {
    let mut access = access.clone();
    let mut default = default.cloned();
    let (mut access_changed, mut default_changed) = (false, false);
    // Masks given, which are kept as given.
    let (mut access_mask, mut default_mask) = (false, false);
    for change in &plan.changes {
        match change {
            Change::RemoveAll => {
                access = access.base();
                default = None;
                access_changed = true;
                default_changed = true;
            }
            Change::RemoveDefault => {
                default = None;
                default_changed = true;
            }
            Change::Modify(specs) | Change::Set(specs) | Change::Remove(specs) => {
                let set = matches!(change, Change::Set(_));
                let mut fresh_access = set && specs.iter().any(|s| !(s.default || plan.default));
                let mut fresh_default = set && specs.iter().any(|s| s.default || plan.default);
                for spec in specs {
                    let to_default = spec.default || plan.default;
                    if to_default && !is_dir {
                        if named {
                            return Err("Only directories can have default ACLs".into());
                        }
                        continue;
                    }
                    if to_default && spec.perm.is_none() && default.is_none() {
                        continue;
                    }
                    let list = if to_default {
                        default_changed = true;
                        if std::mem::take(&mut fresh_default) {
                            default = Some(Acl::default());
                        }
                        // A default list starts from the access list's
                        // owner, group and others, as setfacl's.
                        default.get_or_insert_with(|| access.base())
                    } else {
                        access_changed = true;
                        if std::mem::take(&mut fresh_access) {
                            access = Acl::default();
                        }
                        &mut access
                    };
                    match spec.perm {
                        Some(perm) => {
                            list.set(Entry {
                                tag: spec.tag,
                                id: spec.id,
                                perm: perm.on(is_dir, mode),
                            });
                            if spec.tag == Tag::Mask {
                                if to_default { default_mask = true } else { access_mask = true }
                            }
                        }
                        None => list.remove(spec.tag, spec.id),
                    }
                }
            }
        }
    }
    let finish = |acl: &mut Acl, given: bool, kind: &str| -> Result<(), String> {
        if acl.has_named() || acl.mask().is_some() {
            if !given && !plan.no_mask {
                let mask = acl.calc_mask();
                acl.set(Entry { tag: Tag::Mask, id: NO_ID, perm: mask });
            } else if acl.mask().is_none() {
                let group = acl.get(Tag::GroupObj, NO_ID).map_or(0, |e| e.perm);
                acl.set(Entry { tag: Tag::Mask, id: NO_ID, perm: group });
            }
        }
        if let Some(entry) = acl.missing() {
            let text: Vec<String> =
                acl.entries.iter().map(|&e| entry_text(e, false, false)).collect();
            return Err(format!(
                "Malformed {kind} ACL `{}': Missing or wrong entry at entry {entry}",
                text.join(",")
            ));
        }
        Ok(())
    };
    if access_changed {
        finish(&mut access, access_mask, "access")?;
    }
    if default_changed && let Some(list) = &mut default {
        if list.entries.is_empty() {
            default = None;
        } else {
            finish(list, default_mask, "default")?;
        }
    }
    Ok((access, default, access_changed, default_changed))
}

/// The entries of `-M`, `-X` and `--set-file`'s file, `-` for standard
/// input.
fn specs_from_file(option: &str, file: &OsString, perms: bool) -> Result<Vec<Spec>, Fail> {
    let text = if file.as_bytes() == b"-" {
        io::read_to_string(io::stdin()).map_err(|e| util::failed(&"standard input", e))?
    } else {
        std::fs::read_to_string(file).map_err(|e| util::failed(&Path::new(file).display(), e))?
    };
    specs(option, &text, perms)
}

/// The entries of option `option`'s text, or setfacl's complaint.
fn specs(option: &str, text: &str, perms: bool) -> Result<Vec<Spec>, Fail> {
    parse_specs(text, perms).map_err(|e| match e {
        SpecError::Near(at) => {
            usage(format!("Option {option}: Invalid argument near character {at}"))
        }
        SpecError::Incomplete => usage(format!("Option {option} incomplete")),
    })
}

/// setfacl(1).
pub fn setfacl(args: &mut Args) -> Result<i32, Fail> {
    let mut plan = Plan { changes: Vec::new(), default: false, no_mask: false, test: false };
    let mut recursive = false;
    let mut how = Walk::Named;
    let mut restore = None;
    let mut files: Vec<OsString> = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('m') => plan.changes.push(Change::Modify(specs("-m", &args.text()?, true)?)),
            Arg::Long(l) if l == "modify" => {
                plan.changes.push(Change::Modify(specs("-m", &args.text()?, true)?));
            }
            Arg::Short('M') => {
                plan.changes.push(Change::Modify(specs_from_file("-M", &args.value()?, true)?));
            }
            Arg::Long(l) if l == "modify-file" => {
                plan.changes.push(Change::Modify(specs_from_file("-M", &args.value()?, true)?));
            }
            Arg::Short('x') => {
                plan.changes.push(Change::Remove(specs("-x", &args.text()?, false)?));
            }
            Arg::Long(l) if l == "remove" => {
                plan.changes.push(Change::Remove(specs("-x", &args.text()?, false)?));
            }
            Arg::Short('X') => {
                plan.changes.push(Change::Remove(specs_from_file("-X", &args.value()?, false)?));
            }
            Arg::Long(l) if l == "remove-file" => {
                plan.changes.push(Change::Remove(specs_from_file("-X", &args.value()?, false)?));
            }
            Arg::Long(l) if l == "set" => {
                plan.changes.push(Change::Set(specs("--set", &args.text()?, true)?));
            }
            Arg::Long(l) if l == "set-file" => {
                plan.changes.push(Change::Set(specs_from_file(
                    "--set-file",
                    &args.value()?,
                    true,
                )?));
            }
            Arg::Short('b') => plan.changes.push(Change::RemoveAll),
            Arg::Long(l) if l == "remove-all" => plan.changes.push(Change::RemoveAll),
            Arg::Short('k') => plan.changes.push(Change::RemoveDefault),
            Arg::Long(l) if l == "remove-default" => plan.changes.push(Change::RemoveDefault),
            Arg::Short('d') => plan.default = true,
            Arg::Long(l) if l == "default" => plan.default = true,
            Arg::Short('n') => plan.no_mask = true,
            Arg::Long(l) if l == "no-mask" => plan.no_mask = true,
            Arg::Long(l) if l == "mask" => plan.no_mask = false,
            Arg::Short('R') => recursive = true,
            Arg::Long(l) if l == "recursive" => recursive = true,
            Arg::Short('L') => how = Walk::Logical,
            Arg::Long(l) if l == "logical" => how = Walk::Logical,
            Arg::Short('P') => how = Walk::Physical,
            Arg::Long(l) if l == "physical" => how = Walk::Physical,
            Arg::Long(l) if l == "test" => plan.test = true,
            Arg::Long(l) if l == "restore" => restore = Some(args.value()?),
            Arg::Operand(o) => files.push(o),
            other => return Err(args.unknown(&other)),
        }
    }
    if let Some(dump) = restore {
        return restore_from(&dump, &plan);
    }
    if plan.changes.is_empty() || files.is_empty() {
        return Err(usage("Usage: setfacl [-bkndRLP] { -m|-M|-x|-X ... } file ...".into()));
    }
    let mut ok = true;
    for file in &files {
        ok &= walk("setfacl", Path::new(file), true, recursive, how, &mut |path, named| {
            apply(&plan, path, named, None)
        });
    }
    Ok(i32::from(!ok))
}

/// Applies `plan` to the file at `path`; says why not, and returns
/// false, if it cannot. A restore gives the owner and group to set too.
fn apply(plan: &Plan, path: &Path, named: bool, owner: Option<(u32, u32)>) -> bool {
    let complain = |why: &str| {
        eprintln!("setfacl: {}: {why}", path.display());
        false
    };
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) => return complain(&util::strip_os_error(&e)),
    };
    let lists = access_list(path, meta.mode(), false).and_then(|access| {
        let default = if meta.is_dir() { default_list(path, false)? } else { None };
        Ok((access, default))
    });
    let (access, default) = match lists {
        Ok(lists) => lists,
        Err(e) => return complain(&util::strip_os_error(&e)),
    };
    let (access, default, access_changed, default_changed) =
        match changed(plan, &access, default.as_ref(), meta.is_dir(), named, meta.mode()) {
            Ok(lists) => lists,
            Err(why) => return complain(&why),
        };
    if plan.test {
        let mut parts = access.short_text("");
        parts.push("*".into());
        if let Some(default) = &default {
            parts.extend(default.short_text("d:"));
        }
        println!("{}: {}", path.display(), parts.join(","));
        return true;
    }
    if let Some((uid, gid)) = owner
        && let Err(e) = std::os::unix::fs::chown(path, Some(uid), Some(gid))
    {
        return complain(&util::strip_os_error(&e));
    }
    if access_changed && let Err(e) = write_access(path, &access, meta.mode()) {
        return complain(&util::strip_os_error(&e));
    }
    if default_changed && let Err(e) = write_default(path, default.as_ref()) {
        return complain(&util::strip_os_error(&e));
    }
    true
}

/// `setfacl --restore`: the lists, owners, groups and flags of a dump of
/// getfacl's, each file's set anew.
fn restore_from(dump: &OsString, base: &Plan) -> Result<i32, Fail> {
    let text = if dump.as_bytes() == b"-" {
        io::read_to_string(io::stdin()).map_err(|e| util::failed(&"standard input", e))?
    } else {
        std::fs::read_to_string(dump).map_err(|e| util::failed(&Path::new(dump).display(), e))?
    };
    let mut ok = true;
    let mut file: Option<String> = None;
    let (mut owner, mut group, mut flags) = (None, None, None);
    let mut entries = String::new();
    let mut finish = |file: &mut Option<String>,
                      owner: &mut Option<String>,
                      group: &mut Option<String>,
                      flags: &mut Option<String>,
                      entries: &mut String|
     -> Result<(), Fail> {
        let Some(name) = file.take() else { return Ok(()) };
        let specs = specs("--restore", entries, true)?;
        entries.clear();
        let plan = Plan {
            changes: vec![Change::Set(specs)],
            default: false,
            no_mask: true,
            test: base.test,
        };
        let path = Path::new(&name);
        let ids = match (owner.take(), group.take()) {
            (Some(o), Some(g)) => util::user_id(&o).zip(util::group_id(&g)),
            _ => None,
        };
        let mut done = apply(&plan, path, true, ids);
        if let Some(flags) = flags.take()
            && done
            && !base.test
            && let Ok(meta) = std::fs::metadata(path)
        {
            let f = flags.as_bytes();
            let bit = |i: usize, c: u8, b: u32| if f.get(i) == Some(&c) { b } else { 0 };
            let mode = (meta.mode() & 0o777)
                | bit(0, b's', 0o4000)
                | bit(1, b's', 0o2000)
                | bit(2, b't', 0o1000);
            if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
                eprintln!("setfacl: {name}: {}", util::strip_os_error(&e));
                done = false;
            }
        }
        ok &= done;
        Ok(())
    };
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("# file: ") {
            finish(&mut file, &mut owner, &mut group, &mut flags, &mut entries)?;
            file = Some(name.to_string());
        } else if let Some(name) = line.strip_prefix("# owner: ") {
            owner = Some(name.to_string());
        } else if let Some(name) = line.strip_prefix("# group: ") {
            group = Some(name.to_string());
        } else if let Some(text) = line.strip_prefix("# flags: ") {
            flags = Some(text.to_string());
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            if file.is_none() {
                return Err(fail("--restore: no file named before the entries".into()));
            }
            entries.push_str(line);
            entries.push('\n');
        }
    }
    finish(&mut file, &mut owner, &mut group, &mut flags, &mut entries)?;
    Ok(i32::from(!ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(default: bool, tag: Tag, id: u32, perm: Option<u8>) -> Spec {
        Spec { default, tag, id, perm: perm.map(|bits| Perm { bits, x_if: false }) }
    }

    #[test]
    fn entries_read_as_setfacl_reads_them() {
        let specs = parse_specs("u::rw,u:1001:r-x, g:20:7,m::rw,o:r,d:user:1002:X", true).unwrap();
        assert_eq!(specs[0], spec(false, Tag::UserObj, NO_ID, Some(6)));
        assert_eq!(specs[1], spec(false, Tag::User, 1001, Some(5)));
        assert_eq!(specs[2], spec(false, Tag::Group, 20, Some(7)));
        assert_eq!(specs[3], spec(false, Tag::Mask, NO_ID, Some(6)));
        assert_eq!(specs[4], spec(false, Tag::Other, NO_ID, Some(4)));
        assert_eq!(specs[5].perm, Some(Perm { bits: 0, x_if: true }));
        assert!(specs[5].default);
        // Lines and comments, as -M's file has them.
        let specs =
            parse_specs("# alice and bob\nuser:1001:rw  # alice\ngroup::r\n", true).unwrap();
        assert_eq!(specs.len(), 2);
        // Entries to remove have no permissions.
        let specs = parse_specs("u:1001,d:g:20,m::,default:mask", false).unwrap();
        assert_eq!(specs[0], spec(false, Tag::User, 1001, None));
        assert_eq!(specs[1], spec(true, Tag::Group, 20, None));
        assert_eq!(specs[2], spec(false, Tag::Mask, NO_ID, None));
        assert_eq!(specs[3], spec(true, Tag::Mask, NO_ID, None));
        // Mistakes, where setfacl puts them.
        assert_eq!(parse_specs("u:1001", true), Err(SpecError::Incomplete));
        assert_eq!(parse_specs("u:1001:rq", true), Err(SpecError::Near(9)));
        assert_eq!(parse_specs("q:1001:r", true), Err(SpecError::Near(1)));
        assert_eq!(parse_specs("u::r,z::r", true), Err(SpecError::Near(6)));
        assert_eq!(parse_specs("u:1001:r", false), Err(SpecError::Near(8)));
    }

    #[test]
    fn values_are_linuxs() {
        let mut acl = Acl::from_mode(0o640);
        acl.set(Entry { tag: Tag::User, id: 1001, perm: 6 });
        acl.set(Entry { tag: Tag::Mask, id: NO_ID, perm: 6 });
        let value = acl.to_value();
        assert_eq!(value.len(), 4 + 5 * 8);
        assert_eq!(&value[4..12], &[1, 0, 6, 0, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(&value[12..20], &[2, 0, 6, 0, 0xe9, 3, 0, 0]);
        assert_eq!(Acl::from_value(&value), Some(acl));
        assert_eq!(Acl::from_value(&2u32.to_le_bytes()), None);
        assert_eq!(Acl::from_value(&[1, 0, 0, 0]), None);
    }

    fn plan(changes: Vec<Change>) -> Plan {
        Plan { changes, default: false, no_mask: false, test: false }
    }

    #[test]
    fn masks_are_worked_out_as_setfacls() {
        let base = Acl::from_mode(0o640);
        // -m adds bob and a mask of all the group class has.
        let p = plan(vec![Change::Modify(parse_specs("u:1001:rwx", true).unwrap())]);
        let (access, _, access_changed, _) = changed(&p, &base, None, false, true, 0o640).unwrap();
        assert!(access_changed);
        assert_eq!(access.mask(), Some(7));
        // A mask given is kept; -n keeps the group's for a new one.
        let p = plan(vec![Change::Modify(parse_specs("u:1001:rwx,m::r", true).unwrap())]);
        assert_eq!(changed(&p, &base, None, false, true, 0o640).unwrap().0.mask(), Some(4));
        let mut p = plan(vec![Change::Modify(parse_specs("u:1001:rw", true).unwrap())]);
        p.no_mask = true;
        assert_eq!(changed(&p, &base, None, false, true, 0o640).unwrap().0.mask(), Some(4));
        // -x of the last named user keeps a mask, of the group's.
        let (with_bob, ..) = changed(
            &plan(vec![Change::Modify(parse_specs("u:1001:rwx", true).unwrap())]),
            &base,
            None,
            false,
            true,
            0o640,
        )
        .unwrap();
        let p = plan(vec![Change::Remove(parse_specs("u:1001", false).unwrap())]);
        let (access, ..) = changed(&p, &with_bob, None, false, true, 0o640).unwrap();
        assert_eq!(access.mask(), Some(4));
        assert!(access.get(Tag::User, 1001).is_none());
        // -b leaves the owner, group and others.
        let (access, default, ..) =
            changed(&plan(vec![Change::RemoveAll]), &with_bob, None, false, true, 0o640).unwrap();
        assert_eq!(access, base);
        assert!(default.is_none());
    }

    #[test]
    fn default_lists_start_from_the_access_list() {
        let base = Acl::from_mode(0o750);
        let p = plan(vec![Change::Modify(parse_specs("d:u:1001:rwX", true).unwrap())]);
        let (_, default, access_changed, default_changed) =
            changed(&p, &base, None, true, true, 0o750).unwrap();
        assert!(!access_changed && default_changed);
        let default = default.unwrap();
        assert_eq!(default.get(Tag::UserObj, NO_ID).unwrap().perm, 7);
        assert_eq!(default.get(Tag::User, 1001).unwrap().perm, 7);
        assert_eq!(default.get(Tag::Other, NO_ID).unwrap().perm, 0);
        assert_eq!(default.mask(), Some(7));
        // Not on a file named, and skipped on one under -R.
        assert_eq!(
            changed(&p, &base, None, false, true, 0o640).unwrap_err(),
            "Only directories can have default ACLs"
        );
        assert!(changed(&p, &base, None, false, false, 0o640).is_ok());
        // --set needs the owner, group and others.
        let p = plan(vec![Change::Set(parse_specs("u::rw,g::r", true).unwrap())]);
        assert_eq!(
            changed(&p, &base, None, false, true, 0o640).unwrap_err(),
            "Malformed access ACL `user::rw-,group::r--': Missing or wrong entry at entry 3"
        );
    }

    #[test]
    fn listings_are_getfacls() {
        let mut acl = Acl::from_mode(0o640);
        acl.set(Entry { tag: Tag::User, id: 1001, perm: 7 });
        acl.set(Entry { tag: Tag::Mask, id: NO_ID, perm: 4 });
        let show = Show {
            access: true,
            default: true,
            header: false,
            effective: None,
            skip_base: false,
            numeric: true,
            absolute: false,
        };
        let meta = std::fs::metadata("/").unwrap();
        assert_eq!(
            listing("f", &meta, &acl, None, &show),
            "user::rw-\nuser:1001:rwx\t#effective:r--\ngroup::r--\nmask::r--\nother::---\n\n"
        );
        let default = Acl::from_mode(0o755);
        let show = Show { access: false, ..show };
        assert_eq!(
            listing("d", &meta, &acl, Some(&default), &show),
            "default:user::rwx\ndefault:group::r-x\ndefault:other::r-x\n\n"
        );
    }
}
