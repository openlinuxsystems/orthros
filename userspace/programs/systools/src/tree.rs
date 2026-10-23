// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! tree(1): what directories hold, drawn as a tree, as Steve Baker's tree
//! draws it: entries sorted by name (or version, size or time), hidden
//! ones left out unless `-a`, filtered by wildcards (`-P`, `-I`), as deep
//! as asked (`-L`), with what ls -l would say of each (`-p`, `-s`, `-D`,
//! ...), then a count of the directories and files. `-J` and `-X` write
//! the tree as JSON or XML instead.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::PathBuf;

use crate::args::{Arg, Args};
use crate::modes::mode_text;
use crate::util::{self, Fail, usage};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Sort {
    #[default]
    Name,
    Version,
    Size,
    Modified,
    Changed,
    Unsorted,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Format {
    #[default]
    Text,
    Json,
    Xml,
}

/// How characters that are not printable are shown in names.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Unprintable {
    /// As octal escapes, `\001`, as by default.
    #[default]
    Escaped,
    /// As `?` (`-q`).
    Question,
    /// As they are (`-N`).
    Raw,
}

/// What the options ask for.
#[derive(Default)]
struct Options {
    all: bool,
    dirs_only: bool,
    follow: bool,
    full_path: bool,
    one_fs: bool,
    level: Option<usize>,
    pattern: Option<String>,
    ignore: Vec<String>,
    ignore_case: bool,
    match_dirs: bool,
    prune: bool,
    no_report: bool,
    ascii: bool,
    file_limit: Option<usize>,
    no_indent: bool,
    meta_first: bool,
    quote: bool,
    names: Unprintable,
    perms: bool,
    user: bool,
    group: bool,
    size: bool,
    human: bool,
    si: bool,
    du: bool,
    date: bool,
    time_format: Option<String>,
    classify: bool,
    inodes: bool,
    device: bool,
    sort: Sort,
    changed: bool,
    reverse: bool,
    dirs_first: bool,
    files_first: bool,
    format: Format,
}

/// An entry, and what it holds if it is a directory that was opened.
struct Node {
    /// What is shown: a root's path as given, or an entry's name.
    name: OsString,
    path: PathBuf,
    /// The entry itself (lstat).
    meta: Option<Metadata>,
    /// For a symbolic link: where it points, and what is there.
    target: Option<OsString>,
    target_meta: Option<Metadata>,
    /// Why it was not opened, or what went wrong.
    note: Option<String>,
    children: Option<Vec<Node>>,
    /// Its size with what it holds (`--du`).
    du: u64,
}

impl Node {
    fn at(path: PathBuf, name: OsString) -> Node {
        let meta = fs::symlink_metadata(&path).ok();
        let link = meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
        let target = link.then(|| fs::read_link(&path).ok().map(PathBuf::into_os_string)).flatten();
        let target_meta = if link { fs::metadata(&path).ok() } else { None };
        let du = meta.as_ref().map_or(0, Metadata::len);
        Node { name, path, meta, target, target_meta, note: None, children: None, du }
    }

    fn is_link(&self) -> bool {
        self.meta.as_ref().is_some_and(|m| m.file_type().is_symlink())
    }

    /// A directory itself, not a link to one.
    fn is_real_dir(&self) -> bool {
        self.meta.as_ref().is_some_and(Metadata::is_dir)
    }

    /// A directory, or a link to one: what the count calls a directory.
    fn is_dir(&self) -> bool {
        self.is_real_dir()
            || self.is_link() && self.target_meta.as_ref().is_some_and(Metadata::is_dir)
    }

    /// The type JSON and XML give it.
    fn kind(&self) -> &'static str {
        let Some(meta) = &self.meta else { return "unknown" };
        let t = meta.file_type();
        if t.is_symlink() {
            "link"
        } else if t.is_dir() {
            "directory"
        } else if t.is_fifo() {
            "fifo"
        } else if t.is_socket() {
            "socket"
        } else if t.is_char_device() {
            "char"
        } else if t.is_block_device() {
            "block"
        } else {
            "file"
        }
    }
}

/// tree(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    // The lines are drawn in UTF-8 when the locale's characters are.
    let mut o = Options { ascii: !util::utf8_locale(), ..Options::default() };
    let mut roots: Vec<OsString> = Vec::new();
    let mut output: Option<OsString> = None;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('a') => o.all = true,
            Arg::Short('d') => o.dirs_only = true,
            Arg::Short('l') => o.follow = true,
            Arg::Short('f') => o.full_path = true,
            Arg::Short('x') => o.one_fs = true,
            Arg::Short('L') => {
                let level: usize = args.number()?;
                if level == 0 {
                    return Err(usage("Invalid level, must be greater than 0.".into()));
                }
                o.level = Some(level);
            }
            Arg::Short('P') => o.pattern = Some(args.text()?),
            Arg::Short('I') => o.ignore.push(args.text()?),
            Arg::Long(l) if l == "ignore-case" => o.ignore_case = true,
            Arg::Long(l) if l == "matchdirs" => o.match_dirs = true,
            Arg::Long(l) if l == "metafirst" => o.meta_first = true,
            Arg::Long(l) if l == "prune" => o.prune = true,
            Arg::Long(l) if l == "noreport" => o.no_report = true,
            Arg::Long(l) if l == "charset" => {
                o.ascii = !matches!(args.text()?.to_ascii_lowercase().as_str(), "utf-8" | "utf8")
            }
            Arg::Long(l) if l == "filelimit" => o.file_limit = Some(args.number()?),
            Arg::Short('o') => output = Some(args.value()?),
            Arg::Short('i') => o.no_indent = true,
            Arg::Short('q') => o.names = Unprintable::Question,
            Arg::Short('N') => o.names = Unprintable::Raw,
            Arg::Short('Q') => o.quote = true,
            Arg::Short('p') => o.perms = true,
            Arg::Short('u') => o.user = true,
            Arg::Short('g') => o.group = true,
            Arg::Short('s') => o.size = true,
            Arg::Short('h') => (o.size, o.human) = (true, true),
            Arg::Long(l) if l == "si" => (o.size, o.si) = (true, true),
            Arg::Long(l) if l == "du" => (o.size, o.du) = (true, true),
            Arg::Short('D') => o.date = true,
            Arg::Long(l) if l == "timefmt" => (o.date, o.time_format) = (true, Some(args.text()?)),
            Arg::Short('F') => o.classify = true,
            Arg::Long(l) if l == "inodes" => o.inodes = true,
            Arg::Long(l) if l == "device" => o.device = true,
            Arg::Short('v') => o.sort = Sort::Version,
            Arg::Short('t') => o.sort = Sort::Modified,
            Arg::Short('c') => (o.sort, o.changed) = (Sort::Changed, true),
            Arg::Short('U') => o.sort = Sort::Unsorted,
            Arg::Short('r') => o.reverse = true,
            Arg::Long(l) if l == "dirsfirst" => o.dirs_first = true,
            Arg::Long(l) if l == "filesfirst" => o.files_first = true,
            Arg::Long(l) if l == "sort" => {
                o.sort = match args.text()?.as_str() {
                    "name" => Sort::Name,
                    "version" => Sort::Version,
                    "size" => Sort::Size,
                    "mtime" => Sort::Modified,
                    "ctime" => (Sort::Changed, o.changed = true).0,
                    "none" => Sort::Unsorted,
                    other => {
                        return Err(usage(format!(
                            "sort type '{other}' not valid, should be one of: name, version, size, mtime, ctime, none"
                        )));
                    }
                }
            }
            Arg::Short('J') => o.format = Format::Json,
            Arg::Short('X') => o.format = Format::Xml,
            // Colors, other line sets and --info files: the plain output
            // is the same without them.
            Arg::Short('n' | 'C' | 'A' | 'S' | 'R') => {}
            Arg::Long(l) if l == "info" => {}
            Arg::Operand(root) => roots.push(root),
            other => return Err(args.unknown(&other)),
        }
    }
    if roots.is_empty() {
        roots.push(".".into());
    }
    let mut code = 0;
    let mut nodes = Vec::new();
    for root in roots {
        let mut node = Node::at(PathBuf::from(&root), root);
        if node.meta.is_none() {
            code = 2;
        }
        if node.is_dir() {
            let device =
                node.target_meta.as_ref().or(node.meta.as_ref()).map_or(0, MetadataExt::dev);
            let mut seen = HashSet::new();
            read(&o, &mut node, 0, &mut seen, device, false);
        } else {
            node.note = Some("error opening dir".into());
        }
        nodes.push(node);
    }
    let (mut dirs, mut files) = (0, 0);
    for node in &nodes {
        if node.meta.is_some() {
            count(node, true, &mut dirs, &mut files);
        }
    }
    let mut out: Box<dyn Write> = match output {
        Some(path) => Box::new(BufWriter::new(
            fs::File::create(&path)
                .map_err(|e| util::failed(&PathBuf::from(&path).display(), e))?,
        )),
        None => Box::new(BufWriter::new(io::stdout().lock())),
    };
    match o.format {
        Format::Text => {
            for node in &nodes {
                text(&mut out, &o, node, "", None)?;
            }
            if !o.no_report {
                let total: u64 = nodes.iter().map(|n| n.du).sum();
                writeln!(out, "\n{}", report(&o, dirs, files, total))?;
            }
        }
        Format::Json => {
            writeln!(out, "[")?;
            for (i, node) in nodes.iter().enumerate() {
                if i > 0 {
                    writeln!(out, ",")?;
                }
                json(&mut out, &o, node, 1)?;
            }
            if o.no_report {
                writeln!(out, "\n\n]")?;
            } else if o.dirs_only {
                writeln!(out, "\n,\n  {{\"type\":\"report\",\"directories\":{dirs}}}\n]")?;
            } else {
                writeln!(
                    out,
                    "\n,\n  {{\"type\":\"report\",\"directories\":{dirs},\"files\":{files}}}\n]"
                )?;
            }
        }
        Format::Xml => {
            writeln!(out, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tree>")?;
            for node in &nodes {
                xml(&mut out, &o, node, 1)?;
            }
            if !o.no_report {
                writeln!(out, "  <report>\n    <directories>{dirs}</directories>")?;
                if !o.dirs_only {
                    writeln!(out, "    <files>{files}</files>")?;
                }
                writeln!(out, "  </report>")?;
            }
            writeln!(out, "</tree>")?;
        }
    }
    out.flush()?;
    Ok(code)
}

/// Whether `name` matches one of the `|`-separated wildcards in `pattern`.
fn matches(o: &Options, pattern: &str, name: &OsStr) -> bool {
    pattern.split('|').any(|p| {
        if o.ignore_case {
            util::fnmatch(&p.to_lowercase(), OsStr::new(&name.to_string_lossy().to_lowercase()))
        } else {
            util::fnmatch(p, name)
        }
    })
}

/// Reads the directory `node` names, and those under it as deep as
/// asked; `free` means what it holds need not match `-P`.
fn read(
    o: &Options,
    node: &mut Node,
    depth: usize,
    seen: &mut HashSet<(u64, u64)>,
    device: u64,
    free: bool,
) {
    let Ok(entries) = fs::read_dir(&node.path) else {
        node.note = Some("error opening dir".into());
        return;
    };
    let names: Vec<OsString> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| o.all || !n.as_bytes().starts_with(b"."))
        .collect();
    if let Some(limit) = o.file_limit
        && names.len() > limit
    {
        node.note = Some(format!("{} entries exceeds filelimit, not opening dir", names.len()));
        return;
    }
    seen.extend(node.target_meta.as_ref().or(node.meta.as_ref()).map(|m| (m.dev(), m.ino())));
    // Each child, whether it is a directory to open, and whether it
    // matched -P itself (--matchdirs).
    let mut children: Vec<(Node, bool, bool)> = Vec::new();
    for name in names {
        let child = Node::at(node.path.join(&name), name);
        if o.ignore.iter().any(|p| matches(o, p, &child.name)) {
            continue;
        }
        let dir = child.is_dir();
        if o.dirs_only && !dir {
            continue;
        }
        // A link to a directory is a file to -P, unless links are followed.
        let opened_dir = child.is_real_dir() || o.follow && dir;
        let mut matched = false;
        if let Some(pattern) = &o.pattern {
            matched = o.match_dirs && opened_dir && matches(o, pattern, &child.name);
            if !opened_dir && !free && !matches(o, pattern, &child.name) {
                continue;
            }
        }
        children.push((child, opened_dir, matched));
    }
    children.sort_by(|a, b| order(o, &a.0, &b.0));
    // Opened in the order shown, as tree opens them: a link to a
    // directory shown before it is not followed.
    let mut kept = Vec::new();
    for (mut child, opened_dir, matched) in children {
        if opened_dir && o.level.is_none_or(|level| depth + 1 < level) {
            let id = child.target_meta.as_ref().or(child.meta.as_ref()).map(|m| (m.dev(), m.ino()));
            if child.is_link() && id.is_some_and(|id| seen.contains(&id)) {
                child.note = Some("recursive, not followed".into());
            } else if !(o.one_fs && id.is_some_and(|(dev, _)| dev != device)) {
                read(o, &mut child, depth + 1, seen, device, matched);
            }
        }
        if o.prune && child.is_dir() && child.children.as_ref().is_none_or(Vec::is_empty) {
            continue;
        }
        kept.push(child);
    }
    node.du += kept.iter().map(|c| c.du).sum::<u64>();
    node.children = Some(kept);
}

/// The order of two entries of a directory.
fn order(o: &Options, a: &Node, b: &Node) -> Ordering {
    let time =
        |n: &Node| n.meta.as_ref().map_or(0, |m| if o.changed { m.ctime() } else { m.mtime() });
    let size = |n: &Node| n.meta.as_ref().map_or(0, Metadata::len);
    if o.dirs_first || o.files_first {
        let group = b.is_dir().cmp(&a.is_dir());
        let group = if o.files_first { group.reverse() } else { group };
        if group != Ordering::Equal {
            return group;
        }
    }
    let names = || a.name.as_bytes().cmp(b.name.as_bytes());
    let order = match o.sort {
        Sort::Name => names(),
        Sort::Version => version_cmp(a.name.as_bytes(), b.name.as_bytes()),
        Sort::Size => size(b).cmp(&size(a)).then_with(names),
        Sort::Modified | Sort::Changed => time(a).cmp(&time(b)).then_with(names),
        Sort::Unsorted => Ordering::Equal,
    };
    if o.reverse { order.reverse() } else { order }
}

/// Compares names as versions: runs of digits by their value.
fn version_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let start = (i, j);
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let x = &a[start.0..i];
            let y = &b[start.1..j];
            let (tx, ty) = (trim_zeros(x), trim_zeros(y));
            let order = tx
                .len()
                .cmp(&ty.len())
                .then_with(|| tx.cmp(ty))
                .then_with(|| y.len().cmp(&x.len()));
            if order != Ordering::Equal {
                return order;
            }
        } else {
            if a[i] != b[j] {
                return a[i].cmp(&b[j]);
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

/// Digits without their leading zeros.
fn trim_zeros(digits: &[u8]) -> &[u8] {
    let start = digits.iter().position(|&c| c != b'0').unwrap_or(digits.len());
    &digits[start..]
}

/// Counts the directories and files from `node` down. tree counts a root
/// directory only if it holds something shown.
fn count(node: &Node, root: bool, dirs: &mut u64, files: &mut u64) {
    if root && node.children.as_ref().is_some_and(Vec::is_empty) {
        // An empty root: not counted.
    } else if node.is_dir() {
        *dirs += 1;
    } else {
        *files += 1;
    }
    for child in node.children.iter().flatten() {
        count(child, false, dirs, files);
    }
}

/// A size as tree prints it: in a field of 11, or of 4 with a unit.
fn size_text(o: &Options, size: u64) -> String {
    if !(o.human || o.si) {
        return format!("{size:11}");
    }
    let (base, units) = if o.si { (1000u64, *b"BkMGTPEZY") } else { (1024, *b"BKMGTPEZY") };
    let mut value = size;
    let mut unit = usize::from(size >= base);
    while value >= base * base {
        value /= base;
        unit += 1;
    }
    if unit == 0 {
        return format!("{size:4}");
    }
    let shown = value as f64 / base as f64;
    let digits = if value / base >= 10 { 0 } else { 1 };
    format!("{shown:3.digits$}{}", units[unit] as char)
}

/// What `-p`, `-s`, `-D` and the like say of a node, in brackets.
fn info(o: &Options, node: &Node) -> String {
    let Some(meta) = &node.meta else { return String::new() };
    let mut fields = Vec::new();
    if o.inodes {
        fields.push(format!("{:7}", meta.ino()));
    }
    if o.device {
        fields.push(format!("{:3}", meta.dev()));
    }
    if o.perms {
        fields.push(perms(meta));
    }
    if o.user {
        fields.push(format!("{:<8}", util::user_name(meta.uid())));
    }
    if o.group {
        fields.push(format!("{:<8}", util::group_name(meta.gid())));
    }
    if o.size {
        fields.push(size_text(o, if o.du { node.du } else { meta.len() }));
    }
    if o.date {
        fields.push(date(o, if o.changed { meta.ctime() } else { meta.mtime() }));
    }
    if fields.is_empty() { String::new() } else { format!("[{}]  ", fields.join(" ")) }
}

fn perms(meta: &Metadata) -> String {
    let t = meta.file_type();
    let kind = if t.is_dir() {
        'd'
    } else if t.is_symlink() {
        'l'
    } else if t.is_fifo() {
        'p'
    } else if t.is_socket() {
        's'
    } else if t.is_char_device() {
        'c'
    } else if t.is_block_device() {
        'b'
    } else {
        '-'
    };
    format!("{kind}{}", mode_text(meta.mode()))
}

fn date(o: &Options, at: i64) -> String {
    if let Some(format) = &o.time_format {
        return util::local_time(at, format);
    }
    let now = (util::now_ns() / 1_000_000_000) as i64;
    let six_months = 6 * 30 * 24 * 60 * 60;
    if at > now || at + six_months < now {
        util::local_time(at, "%b %e  %Y")
    } else {
        util::local_time(at, "%b %e %H:%M")
    }
}

/// A name as shown: quoted with `-Q`, and with control characters as
/// octal escapes, as `?` with `-q`, or as they are with `-N`.
fn shown(o: &Options, name: &OsStr) -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::with_capacity(name.len());
    for &b in name.as_bytes() {
        match o.names {
            _ if b >= 0x20 && b != 0x7f => bytes.push(b),
            Unprintable::Escaped => bytes.extend(format!("\\{b:03o}").bytes()),
            Unprintable::Question => bytes.push(b'?'),
            Unprintable::Raw => bytes.push(b),
        }
    }
    if o.quote {
        bytes.insert(0, b'"');
        bytes.push(b'"');
    }
    bytes
}

/// `-F`'s mark for what `meta` describes.
fn mark(meta: Option<&Metadata>) -> &'static str {
    let Some(meta) = meta else { return "" };
    let t = meta.file_type();
    if t.is_dir() {
        "/"
    } else if t.is_symlink() {
        "@"
    } else if t.is_fifo() {
        "|"
    } else if t.is_socket() {
        "="
    } else if t.is_file() && meta.mode() & 0o111 != 0 {
        "*"
    } else {
        ""
    }
}

/// Writes a node and what it holds; `last` says whether it is the last
/// in its directory (none for a root), and `lines` is what comes before
/// the branches of its children.
fn text(
    out: &mut dyn Write,
    o: &Options,
    node: &Node,
    lines: &str,
    last: Option<bool>,
) -> io::Result<()> {
    // tree's line down is followed by no-break spaces, so that it keeps
    // its place when the tree is copied.
    let (branch, more, end) = if o.ascii {
        ("|-- ", "|   ", "`-- ")
    } else {
        ("├── ", "│\u{a0}\u{a0} ", "└── ")
    };
    let prefix = match last {
        _ if o.no_indent => String::new(),
        None => String::new(),
        Some(true) => format!("{lines}{end}"),
        Some(false) => format!("{lines}{branch}"),
    };
    let info = info(o, node);
    if o.meta_first {
        write!(out, "{info}{prefix}")?;
    } else {
        write!(out, "{prefix}{info}")?;
    }
    let name =
        if o.full_path && last.is_some() { node.path.as_os_str() } else { node.name.as_os_str() };
    out.write_all(&shown(o, name))?;
    // A root is shown as it was named, even if it is a link.
    match node.target.as_ref().filter(|_| last.is_some()) {
        Some(target) => {
            out.write_all(b" -> ")?;
            out.write_all(&shown(o, target))?;
            if o.classify {
                out.write_all(mark(node.target_meta.as_ref()).as_bytes())?;
            }
        }
        None if o.classify => out.write_all(mark(node.meta.as_ref()).as_bytes())?,
        None => {}
    }
    if let Some(note) = &node.note {
        write!(out, "  [{note}]")?;
    }
    writeln!(out)?;
    let below = match last {
        None => String::new(),
        Some(true) => format!("{lines}    "),
        Some(false) => format!("{lines}{more}"),
    };
    let children = node.children.as_deref().unwrap_or_default();
    for (i, child) in children.iter().enumerate() {
        text(out, o, child, &below, Some(i + 1 == children.len()))?;
    }
    Ok(())
}

fn report(o: &Options, dirs: u64, files: u64, total: u64) -> String {
    let mut text = String::new();
    if o.du {
        text = format!(
            " {}{} used in ",
            size_text(o, total),
            if o.human || o.si { "" } else { " bytes" }
        );
    }
    text += &format!("{dirs} {}", if dirs == 1 { "directory" } else { "directories" });
    if !o.dirs_only {
        text += &format!(", {files} {}", if files == 1 { "file" } else { "files" });
    }
    text
}

/// The `-p`, `-s` (and so on) fields of a node, as JSON or XML values.
fn fields(o: &Options, node: &Node) -> Vec<(&'static str, String, bool)> {
    let Some(meta) = &node.meta else { return Vec::new() };
    let mut fields = Vec::new();
    if o.inodes {
        fields.push(("inode", meta.ino().to_string(), false));
    }
    if o.device {
        fields.push(("dev", meta.dev().to_string(), false));
    }
    if o.perms {
        fields.push(("mode", format!("{:04o}", meta.mode() & 0o7777), true));
        fields.push(("prot", perms(meta), true));
    }
    if o.user {
        fields.push(("user", util::user_name(meta.uid()), true));
    }
    if o.group {
        fields.push(("group", util::group_name(meta.gid()), true));
    }
    if o.size {
        fields.push(("size", (if o.du { node.du } else { meta.len() }).to_string(), false));
    }
    if o.date {
        fields.push(("time", date(o, if o.changed { meta.ctime() } else { meta.mtime() }), true));
    }
    fields
}

fn json_string(text: &OsStr) -> String {
    let mut s = String::from("\"");
    for c in text.to_string_lossy().chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04x}", c as u32)),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

fn json(out: &mut dyn Write, o: &Options, node: &Node, depth: usize) -> io::Result<()> {
    let indent = "  ".repeat(depth);
    write!(out, "{indent}{{\"type\":\"{}\",\"name\":{}", node.kind(), json_string(&node.name))?;
    if let Some(target) = node.target.as_ref().filter(|_| depth > 1) {
        write!(out, ",\"target\":{}", json_string(target))?;
    }
    for (key, value, string) in fields(o, node) {
        if string {
            write!(out, ",\"{key}\":{}", json_string(OsStr::new(&value)))?;
        } else {
            write!(out, ",\"{key}\":{value}")?;
        }
    }
    if let Some(note) = &node.note {
        write!(out, ",\"error\":{}", json_string(OsStr::new(note)))?;
    }
    match node.children.as_deref() {
        Some(children) if !children.is_empty() => {
            writeln!(out, ",\"contents\":[")?;
            for (i, child) in children.iter().enumerate() {
                if i > 0 {
                    writeln!(out, ",")?;
                }
                json(out, o, child, depth + 1)?;
            }
            write!(out, "\n{indent}]}}")
        }
        _ => write!(out, "}}"),
    }
}

fn xml_string(text: &OsStr) -> String {
    let mut s = String::new();
    for c in text.to_string_lossy().chars() {
        match c {
            '&' => s.push_str("&amp;"),
            '<' => s.push_str("&lt;"),
            '>' => s.push_str("&gt;"),
            '"' => s.push_str("&quot;"),
            '\'' => s.push_str("&apos;"),
            c => s.push(c),
        }
    }
    s
}

fn xml(out: &mut dyn Write, o: &Options, node: &Node, depth: usize) -> io::Result<()> {
    let indent = "  ".repeat(depth);
    let kind = node.kind();
    write!(out, "{indent}<{kind} name=\"{}\"", xml_string(&node.name))?;
    if let Some(target) = node.target.as_ref().filter(|_| depth > 1) {
        write!(out, " target=\"{}\"", xml_string(target))?;
    }
    for (key, value, _) in fields(o, node) {
        write!(out, " {key}=\"{}\"", xml_string(OsStr::new(&value)))?;
    }
    write!(out, ">")?;
    if let Some(note) = &node.note {
        write!(out, "<error>{}</error>", xml_string(OsStr::new(note)))?;
    }
    match node.children.as_deref() {
        Some(children) if !children.is_empty() => {
            writeln!(out)?;
            for child in children {
                xml(out, o, child, depth + 1)?;
            }
            writeln!(out, "{indent}</{kind}>")
        }
        _ => writeln!(out, "</{kind}>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_sort_by_number() {
        assert_eq!(version_cmp(b"x2", b"x10"), Ordering::Less);
        assert_eq!(version_cmp(b"a", b"b"), Ordering::Less);
        assert_eq!(version_cmp(b"file1.10", b"file1.9"), Ordering::Greater);
        assert_eq!(version_cmp(b"x", b"x1"), Ordering::Less);
    }

    #[test]
    fn sizes_as_tree_prints_them() {
        let mut o = test_options();
        assert_eq!(size_text(&o, 4096), "       4096");
        o.human = true;
        assert_eq!(size_text(&o, 7), "   7");
        assert_eq!(size_text(&o, 4096), "4.0K");
        assert_eq!(size_text(&o, 12791), " 12K");
        o.human = false;
        o.si = true;
        assert_eq!(size_text(&o, 4096), "4.1k");
    }

    fn test_options() -> Options {
        Options::default()
    }
}
