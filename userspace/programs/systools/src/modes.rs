// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! chmod(1), chown(1) and chgrp(1), with POSIX's modes and GNU's options.
//!
//! tmpfs, Btrfs and ext4 keep owners, groups and permission bits (ADR
//! 0213); FAT and exFAT keep none, and as on Linux take a change that
//! changes nothing and refuse any other with EPERM.

use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use crate::args::{Arg, Args};
use crate::util::{self, Fail, fail, usage};

/// What to say about each file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verbose {
    Quiet,
    Changes,
    All,
}

/// Which symbolic links -R follows into directories.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Follow {
    /// -P: none.
    Never,
    /// -H: those named on the command line, chmod's way without it.
    Named,
    /// -L: every one.
    All,
}

/// Options the three share.
struct Common {
    /// chmod, chown or chgrp, which starts its messages.
    tool: &'static str,
    recursive: bool,
    verbose: Verbose,
    silent: bool,
    /// chown and chgrp: -h (false) or --dereference (true), if given.
    dereference: Option<bool>,
    /// -H, -L or -P, if given.
    follow: Option<Follow>,
    reference: Option<OsString>,
    operands: Vec<OsString>,
}

impl Common {
    /// Which links -R follows into directories: chmod follows those on
    /// the command line, and chown and chgrp none, unless told.
    fn follow(&self) -> Follow {
        self.follow.unwrap_or(if self.tool == "chmod" { Follow::Named } else { Follow::Never })
    }
}

fn common(
    args: &mut Args,
    mut extra: impl FnMut(&Arg, &mut Args) -> Result<bool, Fail>,
) -> Result<Common, Fail> {
    let mut c = Common {
        tool: args.tool,
        recursive: false,
        verbose: Verbose::Quiet,
        silent: false,
        dereference: None,
        follow: None,
        reference: None,
        operands: Vec::new(),
    };
    while let Some(arg) = args.next()? {
        if extra(&arg, args)? {
            continue;
        }
        match arg {
            Arg::Short('R') => c.recursive = true,
            Arg::Long(l) if l == "recursive" => c.recursive = true,
            Arg::Short('v') => c.verbose = Verbose::All,
            Arg::Long(l) if l == "verbose" => c.verbose = Verbose::All,
            Arg::Short('c') => c.verbose = Verbose::Changes,
            Arg::Long(l) if l == "changes" => c.verbose = Verbose::Changes,
            Arg::Short('f') => c.silent = true,
            Arg::Long(l) if l == "silent" || l == "quiet" => c.silent = true,
            Arg::Short('h') => c.dereference = Some(false),
            Arg::Long(l) if l == "no-dereference" => c.dereference = Some(false),
            Arg::Long(l) if l == "dereference" => c.dereference = Some(true),
            Arg::Short('H') => c.follow = Some(Follow::Named),
            Arg::Short('L') => c.follow = Some(Follow::All),
            Arg::Short('P') => c.follow = Some(Follow::Never),
            Arg::Long(l) if l == "preserve-root" || l == "no-preserve-root" => {}
            Arg::Long(l) if l == "reference" => c.reference = Some(args.value()?),
            Arg::Operand(o) => c.operands.push(o),
            other => return Err(args.unknown(&other)),
        }
    }
    Ok(c)
}

/// Runs `apply` on each file, and on what is under each directory with
/// `-R`, telling it whether the file was named on the command line; it
/// says what it did and whether it worked. Returns the exit status.
fn each(c: &Common, files: &[OsString], apply: &mut dyn FnMut(&Path, bool) -> bool) -> i32 {
    let mut code = 0;
    for file in files {
        walk(c, Path::new(file), true, apply, &mut code);
    }
    code
}

/// [`each`] for one file. A link leads into its directory with -L, or
/// with -H when it was named. chmod changes a directory before what is
/// in it, and chown and chgrp after, as GNU's do.
fn walk(
    c: &Common,
    path: &Path,
    named: bool,
    apply: &mut dyn FnMut(&Path, bool) -> bool,
    code: &mut i32,
) {
    let after = c.tool != "chmod";
    let link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    let follow = c.follow();
    let into = c.recursive
        && (!link || follow == Follow::All || (named && follow == Follow::Named))
        && std::fs::metadata(path).is_ok_and(|m| m.is_dir());
    if !after && !apply(path, named) {
        *code = 1;
    }
    if into {
        match std::fs::read_dir(path) {
            Ok(entries) => {
                let mut names: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
                names.sort();
                for name in names {
                    walk(c, &name, false, apply, code);
                }
            }
            Err(e) => {
                if !c.silent {
                    eprintln!(
                        "{}: cannot read directory '{}': {}",
                        c.tool,
                        path.display(),
                        util::strip_os_error(&e)
                    );
                }
                *code = 1;
            }
        }
    }
    if after && !apply(path, named) {
        *code = 1;
    }
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// One clause of a symbolic mode: who, and its actions.
fn apply_symbolic(mode: &str, old: u32, is_dir: bool, umask: u32) -> Result<u32, String> {
    let mut new = old;
    for clause in mode.split(',') {
        let bytes = clause.as_bytes();
        let mut i = 0;
        let mut who = 0u32;
        while i < bytes.len() && b"ugoa".contains(&bytes[i]) {
            who |= match bytes[i] {
                b'u' => 0o4700,
                b'g' => 0o2070,
                b'o' => 0o1007,
                _ => 0o7777,
            };
            i += 1;
        }
        let masked = who == 0;
        if masked {
            who = 0o7777;
        }
        if i >= bytes.len() {
            return Err(format!("invalid mode: '{mode}'"));
        }
        while i < bytes.len() {
            let op = bytes[i];
            if !b"+-=".contains(&op) {
                return Err(format!("invalid mode: '{mode}'"));
            }
            i += 1;
            let mut perm = 0u32;
            // A copy of another class's bits (`g=u`).
            if i < bytes.len() && b"ugo".contains(&bytes[i]) {
                let bits = match bytes[i] {
                    b'u' => (new >> 6) & 7,
                    b'g' => (new >> 3) & 7,
                    _ => new & 7,
                };
                perm = bits << 6 | bits << 3 | bits;
                i += 1;
            } else {
                while i < bytes.len() && b"rwxXst".contains(&bytes[i]) {
                    perm |= match bytes[i] {
                        b'r' => 0o444,
                        b'w' => 0o222,
                        b'x' => 0o111,
                        // Judged on the mode as the clauses so far left it.
                        b'X' if is_dir || new & 0o111 != 0 => 0o111,
                        b'X' => 0,
                        b's' => 0o6000,
                        _ => 0o1000,
                    };
                    i += 1;
                }
            }
            let mut bits = perm & who;
            if masked {
                bits &= !umask;
            }
            match op {
                b'+' => new |= bits,
                b'-' => new &= !bits,
                _ => {
                    let clear = if masked { 0o7777 } else { who };
                    // `=` keeps a directory's set-ID bits unless named.
                    let keep = if is_dir { new & 0o6000 & !(perm & 0o6000) } else { 0 };
                    new = (new & !clear) | bits | (keep & clear);
                }
            }
        }
    }
    Ok(new)
}

/// The mode `mode` makes of `old`: octal, or symbolic.
fn new_mode(mode: &str, old: u32, is_dir: bool, umask: u32) -> Result<u32, String> {
    if !mode.is_empty() && mode.bytes().all(|b| b.is_ascii_digit()) {
        let n = u32::from_str_radix(mode, 8).map_err(|_| format!("invalid mode: '{mode}'"))?;
        if n > 0o7777 {
            return Err(format!("invalid mode: '{mode}'"));
        }
        // Fewer than five digits keep a directory's set-ID bits.
        return Ok(if is_dir && mode.len() < 5 { n | (old & 0o6000) } else { n });
    }
    apply_symbolic(mode, old, is_dir, umask)
}

/// The process's umask.
fn umask() -> u32 {
    // SAFETY: umask only sets and returns the mask; it is put back.
    unsafe {
        let mask = libc::umask(0o022);
        libc::umask(mask);
        mask as u32
    }
}

/// A mode as ls shows it, `rwxr-xr-x`.
pub fn mode_text(mode: u32) -> String {
    let mut s = String::new();
    for (shift, special, set) in [(6, 0o4000, 's'), (3, 0o2000, 's'), (0, 0o1000, 't')] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(match (bits & 1 != 0, mode & special != 0) {
            (true, true) => set,
            (false, true) => set.to_ascii_uppercase(),
            (true, false) => 'x',
            (false, false) => '-',
        });
    }
    s
}

/// chmod(1).
pub fn chmod(args: &mut Args) -> Result<i32, Fail> {
    // `-w`, `-x` and the like are modes, not options: they come first.
    let mut items = args.rest();
    let mut mode_first = None;
    if let Some(i) = items.iter().position(|a| {
        let a = a.as_bytes();
        a.len() > 1 && a[0] == b'-' && a[1..].iter().all(|b| b"rwxXst".contains(b))
    }) {
        mode_first = Some(items.remove(i).to_string_lossy().into_owned());
    }
    *args = Args::new(args.tool, items);
    let c = common(args, |_, _| Ok(false))?;
    let mut operands = c.operands.clone();
    let mode = match (&c.reference, mode_first) {
        (Some(reference), _) => {
            format!("{:o}", reference_of(reference)?.permissions().mode() & 0o7777)
        }
        (None, Some(mode)) => mode,
        (None, None) => {
            if operands.is_empty() {
                return Err(usage("missing operand".into()));
            }
            operands.remove(0).to_string_lossy().into_owned()
        }
    };
    if operands.is_empty() {
        return Err(usage(format!("missing operand after '{mode}'")));
    }
    let mask = umask();
    // Check the mode before touching anything.
    new_mode(&mode, 0o644, false, mask).map_err(usage)?;
    let follow = c.follow();
    let verbose = c.verbose == Verbose::All;
    let code = each(&c, &operands, &mut |path, named| {
        let shown = path.display();
        let unreachable = |message: String| {
            if !c.silent {
                eprintln!("chmod: {message}");
            }
            if verbose {
                println!("'{shown}' could not be accessed");
            }
            false
        };
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) => {
                return unreachable(format!(
                    "cannot access '{shown}': {}",
                    util::strip_os_error(&e)
                ));
            }
        };
        // A link is changed through only when it is named or -L is given;
        // a mode of its own is not Linux's to change.
        let meta = if meta.file_type().is_symlink() {
            if !named && follow != Follow::All {
                if verbose {
                    println!("neither symbolic link '{shown}' nor referent has been changed");
                }
                return true;
            }
            match std::fs::metadata(path) {
                Ok(meta) => meta,
                Err(_) => {
                    return unreachable(format!("cannot operate on dangling symlink '{shown}'"));
                }
            }
        } else {
            meta
        };
        let old = meta.permissions().mode() & 0o7777;
        let (Ok(new), Ok(naive)) =
            (new_mode(&mode, old, meta.is_dir(), mask), new_mode(&mode, old, meta.is_dir(), 0))
        else {
            return false;
        };
        let change =
            format!("from {old:04o} ({}) to {new:04o} ({})", mode_text(old), mode_text(new));
        let done = c_path(path).and_then(|cpath| {
            // SAFETY: `cpath` is a C string.
            match unsafe { libc::chmod(cpath.as_ptr(), new as libc::mode_t) } {
                0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            }
        });
        if let Err(e) = done {
            if !c.silent {
                eprintln!("chmod: changing permissions of '{shown}': {}", util::strip_os_error(&e));
            }
            if verbose {
                println!("failed to change mode of '{shown}' {change}");
            }
            return false;
        }
        if new != old && c.verbose != Verbose::Quiet {
            println!("mode of '{shown}' changed {change}");
        } else if verbose {
            println!("mode of '{shown}' retained as {old:04o} ({})", mode_text(old));
        }
        // The umask kept bits the mode took away (`-w` with 022): said
        // even with -f, as GNU does, and a failure.
        if new & !naive != 0 {
            eprintln!(
                "chmod: {shown}: new permissions are {}, not {}",
                mode_text(new),
                mode_text(naive)
            );
            return false;
        }
        true
    });
    Ok(code)
}

/// What a reference file's metadata says, or GNU's error for it.
fn reference_of(reference: &OsString) -> Result<std::fs::Metadata, Fail> {
    std::fs::metadata(reference).map_err(|e| {
        let shown = Path::new(reference).display();
        fail(format!("failed to get attributes of '{shown}': {}", util::strip_os_error(&e)))
    })
}

/// The owner and group a chown spec names, and how messages write them:
/// as they were typed, with the login group by name for `user:`.
fn owner_spec(spec: &str) -> Result<(Option<u32>, Option<u32>, String, String), Fail> {
    let invalid_user = || usage(format!("invalid user: '{spec}'"));
    // `user.group` is an old spelling, taken when no user has that name.
    let (owner, group, dotted) = match spec.find(':') {
        Some(at) => (&spec[..at], Some(&spec[at + 1..]), false),
        None => match spec.find('.') {
            Some(at) if util::user_id(spec).is_none() => (&spec[..at], Some(&spec[at + 1..]), true),
            _ => (spec, None, false),
        },
    };
    let uid = match owner {
        "" => None,
        owner => Some(util::user_id(owner).ok_or_else(invalid_user)?),
    };
    let (gid, group_text) = match (group, uid) {
        (None, _) | (Some(""), None) => (None, String::new()),
        (Some(""), Some(uid)) => {
            let gid =
                util::login_group(uid).ok_or_else(|| usage(format!("invalid spec: '{spec}'")))?;
            (Some(gid), util::group_name(gid))
        }
        (Some(group), _) => match util::group_id(group) {
            Some(gid) => (Some(gid), group.to_string()),
            None if dotted => return Err(invalid_user()),
            None => return Err(usage(format!("invalid group: '{spec}'"))),
        },
    };
    if dotted {
        eprintln!("chown: warning: '.' should be ':': '{spec}'");
    }
    Ok((uid, gid, owner.to_string(), group_text))
}

/// Changes the owner and group of the files the arguments name.
fn change_owner(args: &mut Args, group_only: bool) -> Result<i32, Fail> {
    let c = common(args, |_, _| Ok(false))?;
    let mut operands = c.operands.clone();
    // The ids to give, and how messages write them.
    let (uid, gid, user, group) = if let Some(reference) = &c.reference {
        let meta = reference_of(reference)?;
        let uid = (!group_only).then(|| meta.uid());
        (uid, Some(meta.gid()), util::user_name(meta.uid()), util::group_name(meta.gid()))
    } else {
        if operands.is_empty() {
            return Err(usage("missing operand".into()));
        }
        let spec = operands.remove(0).to_string_lossy().into_owned();
        if group_only {
            let gid =
                util::group_id(&spec).ok_or_else(|| usage(format!("invalid group: '{spec}'")))?;
            (None, Some(gid), String::new(), spec)
        } else {
            owner_spec(&spec)?
        }
    };
    if operands.is_empty() {
        return Err(usage("missing operand".into()));
    }
    // With -R and -P, the default there, no link is followed, not even
    // one named.
    let physical = c.recursive && c.follow() == Follow::Never;
    if physical && c.dereference == Some(true) {
        return Err(usage("-R --dereference requires either -H or -L".into()));
    }
    let dereference = c.dereference != Some(false) && !physical;
    let what = if group_only { "group" } else { "ownership" };
    let new = match (group_only, uid, gid) {
        (true, ..) => group.clone(),
        (false, Some(_), Some(_)) => format!("{user}:{group}"),
        (false, Some(_), None) => user.clone(),
        (false, None, Some(_)) => format!(":{group}"),
        (false, None, None) => String::new(),
    };
    let verbose = c.verbose == Verbose::All;
    let to_id = |id: Option<u32>| id.unwrap_or(u32::MAX);
    let code = each(&c, &operands, &mut |path, _| {
        let shown = path.display();
        let found =
            if dereference { std::fs::metadata(path) } else { std::fs::symlink_metadata(path) };
        let meta = match found {
            Ok(meta) => meta,
            Err(e) => {
                if !c.silent {
                    let dangling = dereference && std::fs::symlink_metadata(path).is_ok();
                    let verb = if dangling { "dereference" } else { "access" };
                    eprintln!("{}: cannot {verb} '{shown}': {}", c.tool, util::strip_os_error(&e));
                }
                if verbose {
                    let to = if new.is_empty() { String::new() } else { format!(" to {new}") };
                    println!("failed to change {what} of '{shown}'{to}");
                }
                return false;
            }
        };
        let old = match (group_only, uid, gid) {
            (true, ..) => util::group_name(meta.gid()),
            (false, None, None) => String::new(),
            (false, _, None) => util::user_name(meta.uid()),
            (false, _, Some(_)) => {
                format!("{}:{}", util::user_name(meta.uid()), util::group_name(meta.gid()))
            }
        };
        let done = c_path(path).and_then(|cpath| {
            // SAFETY: `cpath` is a C string; -1 leaves an id as it is.
            let result = unsafe {
                if dereference {
                    libc::chown(cpath.as_ptr(), to_id(uid), to_id(gid))
                } else {
                    libc::lchown(cpath.as_ptr(), to_id(uid), to_id(gid))
                }
            };
            if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
        });
        if let Err(e) = done {
            if !c.silent {
                eprintln!("{}: changing {what} of '{shown}': {}", c.tool, util::strip_os_error(&e));
            }
            if verbose {
                println!("failed to change {what} of '{shown}' from {old} to {new}");
            }
            return false;
        }
        let changed = uid.is_some_and(|u| u != meta.uid()) || gid.is_some_and(|g| g != meta.gid());
        if changed && c.verbose != Verbose::Quiet {
            println!("changed {what} of '{shown}' from {old} to {new}");
        } else if verbose && old.is_empty() {
            println!("{what} of '{shown}' retained");
        } else if verbose {
            println!("{what} of '{shown}' retained as {old}");
        }
        true
    });
    Ok(code)
}

/// chown(1).
pub fn chown(args: &mut Args) -> Result<i32, Fail> {
    change_owner(args, false)
}

/// chgrp(1).
pub fn chgrp(args: &mut Args) -> Result<i32, Fail> {
    change_owner(args, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octal_modes() {
        assert_eq!(new_mode("755", 0o644, false, 0o022), Ok(0o755));
        assert_eq!(new_mode("0644", 0o4755, false, 0o022), Ok(0o644));
        assert!(new_mode("8", 0, false, 0).is_err());
        assert!(new_mode("77777", 0, false, 0).is_err());
    }

    #[test]
    fn symbolic_modes() {
        assert_eq!(new_mode("+x", 0o644, false, 0o022), Ok(0o755));
        assert_eq!(new_mode("u+x", 0o644, false, 0o022), Ok(0o744));
        assert_eq!(new_mode("go-w", 0o666, false, 0o022), Ok(0o644));
        assert_eq!(new_mode("a=r", 0o777, false, 0o022), Ok(0o444));
        assert_eq!(new_mode("u=rwx,g=rx,o=", 0o000, false, 0o022), Ok(0o750));
        assert_eq!(new_mode("g=u", 0o700, false, 0o022), Ok(0o770));
        assert_eq!(new_mode("a+X", 0o644, true, 0o022), Ok(0o755));
        assert_eq!(new_mode("a+X", 0o644, false, 0o022), Ok(0o644));
        assert_eq!(new_mode("u+s", 0o755, false, 0o022), Ok(0o4755));
        assert_eq!(new_mode("+t", 0o755, true, 0o022), Ok(0o1755));
        assert!(new_mode("u*x", 0o644, false, 0o022).is_err());
        assert!(new_mode("u", 0o644, false, 0o022).is_err());
    }

    #[test]
    fn x_follows_the_clauses_before_it() {
        // As the host's chmod (coreutils 9.4) gives them.
        assert_eq!(new_mode("u+x,g+X", 0o644, false, 0o022), Ok(0o754));
        assert_eq!(new_mode("g+X,u+x", 0o644, false, 0o022), Ok(0o744));
        assert_eq!(new_mode("g=u-w", 0o700, false, 0o022), Ok(0o750));
        // The umask keeps what a mode without who takes away.
        assert_eq!(new_mode("-w", 0o666, false, 0o022), Ok(0o466));
        assert_eq!(new_mode("-w", 0o666, false, 0), Ok(0o444));
    }

    #[test]
    fn owner_specs() {
        let spec = |text: &str| owner_spec(text).map_err(|fail| fail.message);
        assert_eq!(spec(""), Ok((None, None, String::new(), String::new())));
        assert_eq!(spec(":"), Ok((None, None, String::new(), String::new())));
        assert_eq!(spec("0:0"), Ok((Some(0), Some(0), "0".into(), "0".into())));
        assert_eq!(spec("7"), Ok((Some(7), None, "7".into(), String::new())));
        assert_eq!(spec(":5"), Ok((None, Some(5), String::new(), "5".into())));
        assert_eq!(spec("0.0"), Ok((Some(0), Some(0), "0".into(), "0".into())));
        assert_eq!(spec("0:0:"), Err("invalid group: '0:0:'".to_string()));
    }

    #[test]
    fn modes_as_ls_shows_them() {
        assert_eq!(mode_text(0o755), "rwxr-xr-x");
        assert_eq!(mode_text(0o4755), "rwsr-xr-x");
        assert_eq!(mode_text(0o1644), "rw-r--r-T");
    }
}
