// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The working directory: `cd`, `pwd`, and the directory stack of
//! `pushd`, `popd` and `dirs`.
//!
//! The shell keeps a logical working directory in `PWD`: `cd` through a
//! symbolic link remembers the link's name, and `cd ..` goes back up
//! through it (XCU cd, "-L"). With `-P` or `set -P`, links are resolved.

use super::{flags, out, usage};
use crate::shell::{Res, Shell};
use crate::text;
use crate::vars::attr;

/// `path` with `.` and `..` taken out by its text alone: `a/b/../c` is
/// `a/c`.
pub fn canonical(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    // POSIX keeps a leading `//`.
    if path.starts_with("//") && !path.starts_with("///") {
        out.insert(0, '/');
    }
    out
}

fn is_dir(path: &str) -> bool {
    std::fs::metadata(text::to_os(path)).is_ok_and(|m| m.is_dir())
}

/// The working directory as the system has it.
fn physical() -> Option<String> {
    std::env::current_dir().ok().map(|p| text::from_os(p.as_os_str()))
}

impl Shell {
    /// The logical working directory: `PWD` if it names where we are.
    pub fn logical_pwd(&self) -> Option<String> {
        self.get("PWD").filter(|pwd| pwd.starts_with('/') && same_file(pwd, "."))
    }

    /// Goes to `dir`; with `physical`, through symbolic links. Sets `PWD`
    /// and `OLDPWD`, which is exported, as bash does: the new directory,
    /// and bash's errors for those of the two that are read-only (the
    /// change stays made). An error message on failure.
    pub fn change_dir(
        &mut self,
        dir: &str,
        physical_mode: bool,
    ) -> Result<(String, Vec<String>), String> {
        let old = self.logical_pwd().or_else(physical).unwrap_or_default();
        let mut target = match dir.starts_with('/') {
            true => dir.to_string(),
            // Not `//dir` from `/`, which POSIX lets mean something else.
            false if old.ends_with('/') => format!("{old}{dir}"),
            false => format!("{old}/{dir}"),
        };
        if !physical_mode {
            target = canonical(&target);
        }
        let changed = std::env::set_current_dir(text::to_os(&target));
        if let Err(error) = changed {
            // A logical path through `..` that does not exist physically:
            // try the path as given.
            if !physical_mode && std::env::set_current_dir(text::to_os(dir)).is_ok() {
                target = physical().unwrap_or(target);
            } else {
                return Err(format!(
                    "{}: {}",
                    crate::expand::printable(dir),
                    crate::sys::describe(&std::io::Error::from_raw_os_error(
                        error.raw_os_error().unwrap_or(libc::ENOENT)
                    ))
                ));
            }
        }
        let new = if physical_mode { physical().unwrap_or(target) } else { target };
        let mut refused = Vec::new();
        for (name, value) in [("OLDPWD", &old), ("PWD", &new)] {
            if let Err(message) = self.writable(name) {
                refused.push(message);
                continue;
            }
            self.set_var(name, value);
            if name == "OLDPWD" {
                self.vars.entry(name).attrs |= attr::EXPORT;
            }
        }
        Ok((new, refused))
    }

    /// The shell's own file, as bash finds it for BASH: `arg0` made
    /// absolute, or found on `PATH`; else the program's path.
    pub fn own_path(&self, arg0: &str) -> String {
        let arg0 = arg0.trim_start_matches('-');
        if arg0.starts_with('/') {
            return arg0.to_string();
        }
        if arg0.contains('/') {
            if let Some(cwd) = self.logical_pwd().or_else(physical) {
                return canonical(&format!("{cwd}/{arg0}"));
            }
        } else if let Some(found) = self.search_path(arg0, false) {
            return found;
        }
        std::env::current_exe().map(|path| text::from_os(path.as_os_str())).unwrap_or_default()
    }

    /// [`Shell::change_dir`] for `pushd` and `popd`: the new directory, or
    /// None with the errors given.
    fn go(&mut self, builtin: &str, dir: &str) -> Option<String> {
        match self.change_dir(dir, self.opts.physical) {
            Ok((new, refused)) if refused.is_empty() => Some(new),
            Ok((_, refused)) => {
                for message in refused {
                    self.error(&message);
                }
                None
            }
            Err(message) => {
                self.error(&format!("{builtin}: {message}"));
                None
            }
        }
    }

    /// `~`-abbreviated `path`, as `dirs` shows it.
    fn tilde_short(&self, path: &str) -> String {
        match self.get("HOME").filter(|h| !h.is_empty() && h != "/") {
            Some(home) if path == home => "~".into(),
            Some(home) if path.starts_with(&format!("{home}/")) => {
                format!("~{}", &path[home.len()..])
            }
            _ => path.to_string(),
        }
    }
}

fn same_file(a: &str, b: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(text::to_os(a)), std::fs::metadata(text::to_os(b))) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

pub fn cd(shell: &mut Shell, args: &[String]) -> Res {
    if shell.opts.restricted {
        shell.error("cd: restricted");
        return Ok(1);
    }
    let flags = match flags(shell, "cd", &args[1..], "LPe@", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    // The last of -L and -P counts.
    let physical_mode = flags
        .on
        .iter()
        .rev()
        .find(|(c, _)| *c == 'L' || *c == 'P')
        .map_or(shell.opts.physical, |(c, _)| *c == 'P');
    if flags.rest.len() > 1 {
        shell.error("cd: too many arguments");
        return Ok(1);
    }
    let mut print = false;
    let dir = match flags.rest.first().map(String::as_str) {
        None => match shell.get("HOME") {
            Some(home) if !home.is_empty() => home,
            Some(_) => return Ok(0),
            None => {
                shell.error("cd: HOME not set");
                return Ok(1);
            }
        },
        Some("-") => match shell.get("OLDPWD") {
            Some(old) if !old.is_empty() => {
                print = true;
                old
            }
            _ => {
                shell.error("cd: OLDPWD not set");
                return Ok(1);
            }
        },
        Some("") => return Ok(0),
        Some(dir) => dir.to_string(),
    };
    // CDPATH, for a name that does not start with `/`, `.` or `..`.
    let mut chosen = None;
    let relative = !dir.starts_with('/')
        && dir != "."
        && dir != ".."
        && !dir.starts_with("./")
        && !dir.starts_with("../");
    if relative && let Some(cdpath) = shell.get("CDPATH") {
        for entry in cdpath.split(':') {
            let candidate = if entry.is_empty() {
                dir.clone()
            } else {
                format!("{}/{dir}", entry.trim_end_matches('/'))
            };
            let full = if candidate.starts_with('/') {
                candidate.clone()
            } else {
                format!("{}/{candidate}", shell.logical_pwd().or_else(physical).unwrap_or_default())
            };
            if is_dir(&full) {
                print = print || !entry.is_empty();
                chosen = Some(candidate);
                break;
            }
        }
    }
    let target = chosen.unwrap_or_else(|| dir.clone());
    let target = if !is_dir_target(shell, &target)
        && shell.shopt("cdable_vars")
        && crate::parse::is_name(&target)
    {
        match shell.get(&target) {
            Some(value) => {
                print = true;
                value
            }
            None => target,
        }
    } else {
        target
    };
    let (new, refused) = match shell.change_dir(&target, physical_mode) {
        Ok(changed) => changed,
        Err(message) => {
            shell.error(&format!("cd: {message}"));
            return Ok(1);
        }
    };
    if flags.has('e') && physical_mode && physical().is_none() {
        return Ok(1);
    }
    if print && !out(shell, "cd", &format!("{new}\n")) {
        return Ok(1);
    }
    for message in &refused {
        shell.error(message);
    }
    Ok(i32::from(!refused.is_empty()))
}

fn is_dir_target(shell: &Shell, dir: &str) -> bool {
    if dir.starts_with('/') {
        is_dir(dir)
    } else {
        is_dir(&format!("{}/{dir}", shell.logical_pwd().or_else(physical).unwrap_or_default()))
    }
}

pub fn pwd(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "pwd", &args[1..], "LP", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    let physical_mode = flags
        .on
        .iter()
        .rev()
        .find(|(c, _)| *c == 'L' || *c == 'P')
        .map_or(shell.opts.physical, |(c, _)| *c == 'P');
    let dir = if physical_mode { physical() } else { shell.logical_pwd().or_else(physical) };
    match dir {
        Some(dir) => Ok(i32::from(!out(shell, "pwd", &format!("{dir}\n")))),
        None => {
            shell.error(
                "pwd: error retrieving current directory: getcwd: cannot access parent directories",
            );
            Ok(1)
        }
    }
}

/// The stack as `dirs` numbers it: the working directory, then the
/// saved ones, most recent first.
fn stack(shell: &Shell) -> Vec<String> {
    let mut all = vec![shell.logical_pwd().or_else(physical).unwrap_or_default()];
    all.extend(shell.dirstack.iter().rev().cloned());
    all
}

/// Puts the stack back from `dirs`'s numbering (the first entry is the
/// working directory, which is not stored).
fn set_stack(shell: &mut Shell, all: &[String]) {
    shell.dirstack = all[1..].iter().rev().cloned().collect();
}

/// `+N` or `-N`: the entry it names, counting from the left or right.
fn stack_index(arg: &str, len: usize) -> Option<Option<usize>> {
    let (from_left, digits) = match arg.as_bytes().first() {
        Some(b'+') => (true, &arg[1..]),
        Some(b'-') => (false, &arg[1..]),
        _ => return None,
    };
    let n: usize = digits.parse().ok()?;
    Some(if from_left { (n < len).then_some(n) } else { (n < len).then(|| len - 1 - n) })
}

fn print_stack(shell: &Shell, long: bool, lines: bool, numbered: bool) -> bool {
    let all = stack(shell);
    let shown: Vec<String> =
        all.iter().map(|d| if long { d.clone() } else { shell.tilde_short(d) }).collect();
    let text = if numbered {
        shown.iter().enumerate().map(|(i, d)| format!("{i:2}  {d}\n")).collect()
    } else if lines {
        shown.iter().map(|d| format!("{d}\n")).collect()
    } else {
        format!("{}\n", shown.join(" "))
    };
    out(shell, "dirs", &text)
}

pub fn dirs(shell: &mut Shell, args: &[String]) -> Res {
    let mut long = false;
    let mut lines = false;
    let mut numbered = false;
    let mut which = None;
    for arg in &args[1..] {
        let len = stack(shell).len();
        if let Some(index) = stack_index(arg, len)
            && arg[1..].chars().all(|c| c.is_ascii_digit())
        {
            match index {
                Some(i) => which = Some(i),
                None => {
                    shell
                        .error(&format!("dirs: {}: directory stack index out of range", &arg[1..]));
                    return Ok(1);
                }
            }
            continue;
        }
        match arg.as_str() {
            "-c" => {
                shell.dirstack.clear();
                return Ok(0);
            }
            "-l" => long = true,
            "-p" => lines = true,
            "-v" => {
                lines = true;
                numbered = true;
            }
            _ => return usage(shell, "dirs", &format!("{arg}: invalid option")),
        }
    }
    if let Some(i) = which {
        let all = stack(shell);
        let dir = if long { all[i].clone() } else { shell.tilde_short(&all[i]) };
        return Ok(i32::from(!out(shell, "dirs", &format!("{dir}\n"))));
    }
    Ok(i32::from(!print_stack(shell, long, lines, numbered)))
}

pub fn pushd(shell: &mut Shell, args: &[String]) -> Res {
    let mut no_cd = false;
    let mut operand = None;
    for arg in &args[1..] {
        if arg == "-n" {
            no_cd = true;
        } else if operand.is_none() {
            operand = Some(arg.clone());
        } else {
            shell.error("pushd: too many arguments");
            return Ok(1);
        }
    }
    let mut all = stack(shell);
    match operand {
        None => {
            // Swap the top two.
            if all.len() < 2 {
                shell.error("pushd: no other directory");
                return Ok(1);
            }
            all.swap(0, 1);
        }
        Some(arg)
            if stack_index(&arg, all.len()).is_some()
                && arg[1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            let Some(Some(n)) = stack_index(&arg, all.len()) else {
                shell.error(&format!("pushd: {}: directory stack index out of range", &arg[1..]));
                return Ok(1);
            };
            all.rotate_left(n);
        }
        Some(dir) => {
            if no_cd {
                all.insert(1, dir);
                set_stack(shell, &all);
                print_stack(shell, false, false, false);
                return Ok(0);
            }
            let current = all[0].clone();
            let Some(new) = shell.go("pushd", &dir) else { return Ok(1) };
            all[0] = new;
            all.insert(1, current);
            set_stack(shell, &all);
            print_stack(shell, false, false, false);
            return Ok(0);
        }
    }
    if !no_cd {
        let dir = all[0].clone();
        if shell.go("pushd", &dir).is_none() {
            return Ok(1);
        }
    }
    set_stack(shell, &all);
    print_stack(shell, false, false, false);
    Ok(0)
}

pub fn popd(shell: &mut Shell, args: &[String]) -> Res {
    let mut no_cd = false;
    let mut which = None;
    let mut all = stack(shell);
    for arg in &args[1..] {
        if arg == "-n" {
            no_cd = true;
        } else if let Some(index) = stack_index(arg, all.len())
            && arg[1..].chars().all(|c| c.is_ascii_digit())
        {
            match index {
                Some(i) => which = Some(i),
                None => {
                    shell
                        .error(&format!("popd: {}: directory stack index out of range", &arg[1..]));
                    return Ok(1);
                }
            }
        } else {
            return usage(shell, "popd", &format!("{arg}: invalid argument"));
        }
    }
    if all.len() < 2 {
        shell.error("popd: directory stack empty");
        return Ok(1);
    }
    let i = which.unwrap_or(0);
    all.remove(i);
    if i == 0 && !no_cd {
        let dir = all[0].clone();
        if shell.go("popd", &dir).is_none() {
            return Ok(1);
        }
    }
    set_stack(shell, &all);
    print_stack(shell, false, false, false);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::canonical;

    #[test]
    fn dot_dot_goes_up_by_name() {
        assert_eq!(canonical("/a/b/../c/./d/"), "/a/c/d");
        assert_eq!(canonical("/.."), "/");
        assert_eq!(canonical("//net/x"), "//net/x");
    }
}
