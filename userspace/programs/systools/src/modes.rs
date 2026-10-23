// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! chmod(1), chown(1) and chgrp(1), with POSIX's modes and GNU's options.
//!
//! Until users come (Phase 13) Radix's files have no owners or
//! permission bits to change: the C library's chmod and chown check that
//! the file is there and change nothing (radix-posix(7)). These tools
//! still read their arguments as Linux's do and fail as they would, so
//! that scripts that use them run.

use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use crate::args::{Arg, Args};
use crate::util::{self, Fail, usage};

/// What to say about each file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verbose {
    Quiet,
    Changes,
    All,
}

/// Options the three share.
struct Common {
    /// chmod, chown or chgrp, which starts its messages.
    tool: &'static str,
    recursive: bool,
    verbose: Verbose,
    silent: bool,
    /// chown and chgrp: change a link itself, not what it points to.
    no_dereference: bool,
    reference: Option<OsString>,
    operands: Vec<OsString>,
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
        no_dereference: false,
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
            Arg::Short('h') => c.no_dereference = true,
            Arg::Long(l) if l == "no-dereference" => c.no_dereference = true,
            Arg::Long(l) if l == "dereference" => c.no_dereference = false,
            Arg::Short('H' | 'L' | 'P') => {}
            Arg::Long(l) if l == "preserve-root" || l == "no-preserve-root" => {}
            Arg::Long(l) if l == "reference" => c.reference = Some(args.value()?),
            Arg::Operand(o) => c.operands.push(o),
            other => return Err(args.unknown(&other)),
        }
    }
    Ok(c)
}

/// Runs `apply` on each file, and on what is under each directory with
/// `-R`. Returns the exit status.
fn each(c: &Common, files: &[OsString], apply: &mut dyn FnMut(&Path) -> io::Result<()>) -> i32 {
    let mut code = 0;
    for file in files {
        walk(c, Path::new(file), apply, &mut code);
    }
    code
}

fn walk(c: &Common, path: &Path, apply: &mut dyn FnMut(&Path) -> io::Result<()>, code: &mut i32) {
    if let Err(e) = apply(path) {
        if !c.silent {
            eprintln!(
                "{}: cannot access '{}': {}",
                c.tool,
                path.display(),
                util::strip_os_error(&e)
            );
        }
        *code = 1;
        return;
    }
    if !c.recursive {
        return;
    }
    // Links are not followed into.
    let Ok(meta) = std::fs::symlink_metadata(path) else { return };
    if !meta.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
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
            return;
        }
    };
    let mut names: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    names.sort();
    for name in names {
        walk(c, &name, apply, code);
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
                        b'X' if is_dir || old & 0o111 != 0 => 0o111,
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
            let meta = std::fs::metadata(reference)
                .map_err(|e| util::failed(&Path::new(reference).display(), e))?;
            format!("{:o}", meta.permissions().mode() & 0o7777)
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
    let code = each(&c, &operands, &mut |path| {
        let meta = std::fs::metadata(path)?;
        let old = meta.permissions().mode() & 0o7777;
        let new = new_mode(&mode, old, meta.is_dir(), mask)
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let cpath = c_path(path)?;
        // SAFETY: `cpath` is a C string.
        if unsafe { libc::chmod(cpath.as_ptr(), new as libc::mode_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let changed = new != old;
        if c.verbose == Verbose::All || (c.verbose == Verbose::Changes && changed) {
            if changed {
                println!(
                    "mode of '{}' changed from {old:04o} ({}) to {new:04o} ({})",
                    path.display(),
                    mode_text(old),
                    mode_text(new)
                );
            } else {
                println!("mode of '{}' retained as {old:04o} ({})", path.display(), mode_text(old));
            }
        }
        Ok(())
    });
    Ok(code)
}

/// Changes the owner and group of the files the arguments name.
fn change_owner(args: &mut Args, group_only: bool) -> Result<i32, Fail> {
    let c = common(args, |_, _| Ok(false))?;
    let mut operands = c.operands.clone();
    let (uid, gid): (Option<u32>, Option<u32>) = if let Some(reference) = &c.reference {
        let meta = std::fs::metadata(reference)
            .map_err(|e| util::failed(&Path::new(reference).display(), e))?;
        (if group_only { None } else { Some(meta.uid()) }, Some(meta.gid()))
    } else {
        if operands.is_empty() {
            return Err(usage("missing operand".into()));
        }
        let spec = operands.remove(0).to_string_lossy().into_owned();
        if group_only {
            let gid =
                util::group_id(&spec).ok_or_else(|| usage(format!("invalid group: '{spec}'")))?;
            (None, Some(gid))
        } else {
            let (owner, group) = match spec.find([':', '.']) {
                Some(at) => (&spec[..at], Some(&spec[at + 1..])),
                None => (spec.as_str(), None),
            };
            let uid = if owner.is_empty() {
                None
            } else {
                Some(util::user_id(owner).ok_or_else(|| usage(format!("invalid user: '{spec}'")))?)
            };
            let gid = match group {
                // `owner:` is the owner's login group: root's is 0.
                Some("") if uid.is_some() => Some(0),
                Some("") | None => None,
                Some(group) => Some(
                    util::group_id(group)
                        .ok_or_else(|| usage(format!("invalid group: '{spec}'")))?,
                ),
            };
            (uid, gid)
        }
    };
    if operands.is_empty() {
        return Err(usage("missing operand".into()));
    }
    let to_id = |id: Option<u32>| id.unwrap_or(u32::MAX);
    let code = each(&c, &operands, &mut |path| {
        let meta = if c.no_dereference {
            std::fs::symlink_metadata(path)?
        } else {
            std::fs::metadata(path)?
        };
        let cpath = c_path(path)?;
        // SAFETY: `cpath` is a C string; -1 leaves an id as it is.
        let result = unsafe {
            if c.no_dereference {
                libc::lchown(cpath.as_ptr(), to_id(uid), to_id(gid))
            } else {
                libc::chown(cpath.as_ptr(), to_id(uid), to_id(gid))
            }
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let changed = uid.is_some_and(|u| u != meta.uid()) || gid.is_some_and(|g| g != meta.gid());
        if c.verbose == Verbose::All || (c.verbose == Verbose::Changes && changed) {
            let what = if group_only { "group" } else { "ownership" };
            let verb = if changed { "changed" } else { "retained" };
            println!("{what} of '{}' {verb}", path.display());
        }
        Ok(())
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
    fn modes_as_ls_shows_them() {
        assert_eq!(mode_text(0o755), "rwxr-xr-x");
        assert_eq!(mode_text(0o4755), "rwsr-xr-x");
        assert_eq!(mode_text(0o1644), "rw-r--r-T");
    }
}
