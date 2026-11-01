// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Getting about folders (ADR 0262): `z`, which goes to the folder most
//! used of those that match its words, as zoxide and z.sh do, and
//! Alt+Left and Alt+Right, which go back and forward through the folders
//! this shell has been in, as a file manager's arrows do.
//!
//! Each folder an interactive shell goes into is counted in
//! `~/.local/state/nukesh/folders`, a line each: `rank TAB time TAB
//! path`. A folder's score is its rank, weighed by how lately it was
//! used; when the ranks add up to more than 10 000, each is cut by a
//! tenth and those under 1 are dropped, so the list keeps to what is used.

use crate::shell::Shell;

/// The file, under the home folder.
pub const FILE: &str = ".local/state/nukesh/folders";

/// When the ranks add up to more than this, they age.
const AGE_AT: f64 = 10_000.0;

/// A folder and how much it has been used.
#[derive(Clone, Debug, PartialEq)]
pub struct Folder {
    pub rank: f64,
    pub time: i64,
    pub path: String,
}

/// The folders file's lines.
pub fn parse(text: &str) -> Vec<Folder> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let rank = fields.next()?.parse().ok()?;
            let time = fields.next()?.parse().ok()?;
            let path = fields.next()?.to_string();
            path.starts_with('/').then_some(Folder { rank, time, path })
        })
        .collect()
}

fn format(folders: &[Folder]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for f in folders {
        let _ = writeln!(out, "{}\t{}\t{}", f.rank, f.time, f.path);
    }
    out
}

/// Counts a visit to `path` at `now`.
pub fn visit(folders: &mut Vec<Folder>, path: &str, now: i64) {
    match folders.iter_mut().find(|f| f.path == path) {
        Some(folder) => {
            folder.rank += 1.0;
            folder.time = now;
        }
        None => folders.push(Folder { rank: 1.0, time: now, path: path.to_string() }),
    }
    if folders.iter().map(|f| f.rank).sum::<f64>() > AGE_AT {
        for folder in folders.iter_mut() {
            folder.rank *= 0.9;
        }
        folders.retain(|f| f.rank >= 1.0);
    }
}

/// A folder's score at `now`: its rank, more for one used in the last
/// hour or day, less for one not used this week.
fn score(folder: &Folder, now: i64) -> f64 {
    let age = now - folder.time;
    let weight = match age {
        ..3600 => 4.0,
        3600..86_400 => 2.0,
        86_400..604_800 => 0.5,
        _ => 0.25,
    };
    folder.rank * weight
}

/// Whether `path` matches `words`: each one, ignoring case, in order,
/// and the last in its last part.
pub fn matches(path: &str, words: &[String]) -> bool {
    let lower = path.to_lowercase();
    let mut at = 0;
    for word in words {
        let word = word.to_lowercase();
        match lower[at..].find(&word) {
            Some(found) => at += found + word.len(),
            None => return false,
        }
    }
    let last = lower.rsplit('/').next().unwrap_or("");
    words.last().is_none_or(|w| last.contains(&w.to_lowercase()))
}

/// The best folder for `words`, other than `here`, that still exists.
pub fn best<'a>(
    folders: &'a [Folder],
    words: &[String],
    here: &str,
    now: i64,
) -> Option<&'a Folder> {
    let mut found: Vec<&Folder> =
        folders.iter().filter(|f| f.path != here && matches(&f.path, words)).collect();
    // A repository's top folder counts half again: `z site` goes to the
    // repository, not a folder inside it used as often (ADR 0288).
    let weighed = |f: &Folder| {
        let top = std::path::Path::new(&f.path).join(".git").exists();
        score(f, now) * if top { 1.5 } else { 1.0 }
    };
    found.sort_by(|a, b| weighed(b).total_cmp(&weighed(a)));
    found.into_iter().find(|f| std::fs::metadata(&f.path).is_ok_and(|m| m.is_dir()))
}

/// `cdr`: to the top folder of the git repository the shell is in, or
/// of the one above it if already at a top (ADR 0288).
pub fn cdr(shell: &mut Shell, args: &[String]) -> crate::shell::Res {
    if args.len() > 1 {
        shell.error("cdr: takes no arguments");
        return Ok(2);
    }
    let here = std::path::PathBuf::from(shell.get("PWD").unwrap_or_default());
    let Some(repo) = crate::git::find(&here) else {
        shell.error("cdr: not in a git repository");
        return Ok(1);
    };
    let mut top = repo.work;
    // At a submodule's top, its superproject's.
    if top == here
        && let Some(parent) = top.parent().and_then(crate::git::find)
    {
        top = parent.work;
    }
    let path = top.to_string_lossy().into_owned();
    Ok(i32::from(!crate::builtins::cd_to(shell, &path)))
}

fn path_of(shell: &Shell) -> Option<String> {
    let home = shell.get("HOME").filter(|h| h.starts_with('/'))?;
    Some(format!("{}/{FILE}", home.trim_end_matches('/')))
}

fn load(shell: &Shell) -> Vec<Folder> {
    path_of(shell)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map_or_else(Vec::new, |t| parse(&t))
}

fn save(shell: &Shell, folders: &[Folder]) {
    let Some(path) = path_of(shell) else { return };
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let temporary = format!("{path}.{}", std::process::id());
    if std::fs::write(&temporary, format(folders)).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
}

/// Before each prompt: a folder changed since the last is counted, and
/// the one left is the way back.
pub fn before_prompt(shell: &mut Shell) {
    let here = shell.get("PWD").unwrap_or_default();
    if here.is_empty() || here == shell.last_dir {
        return;
    }
    let left = std::mem::replace(&mut shell.last_dir, here.clone());
    if !left.is_empty() {
        shell.dirs_back.push(left);
        if shell.dirs_back.len() > 100 {
            shell.dirs_back.remove(0);
        }
        shell.dirs_forward.clear();
    }
    if shell.private {
        return;
    }
    let mut folders = load(shell);
    visit(&mut folders, &here, crate::sys::now().0);
    save(shell, &folders);
    if shell.settings.resume {
        remember_where(shell, &here);
    }
}

/// Where the last shell on each console was, a line each: `console TAB
/// folder` (ADR 0286, `[folders] resume`).
pub const WHERE: &str = ".local/state/nukesh/where";

fn where_path(shell: &Shell) -> Option<String> {
    let home = shell.get("HOME").filter(|h| h.starts_with('/'))?;
    Some(format!("{}/{WHERE}", home.trim_end_matches('/')))
}

/// `text` with `console` now in `folder`.
pub fn set_where(text: &str, console: &str, folder: &str) -> String {
    let mut out = String::new();
    for line in text.lines().filter(|line| line.split('\t').next() != Some(console)) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("{console}\t{folder}\n"));
    out
}

/// Where the last shell on `console` was, in `text`.
pub fn get_where<'a>(text: &'a str, console: &str) -> Option<&'a str> {
    text.lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(c, _)| *c == console)
        .map(|(_, f)| f)
}

/// The console the shell is on, from `XDG_VTNR`, which consoled sets;
/// none in a terminal window or over ssh, where nothing is resumed.
fn console(shell: &Shell) -> Option<String> {
    shell
        .get("XDG_VTNR")
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .map(|n| format!("tty{n}"))
}

fn remember_where(shell: &Shell, folder: &str) {
    let (Some(path), Some(console)) = (where_path(shell), console(shell)) else { return };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let temporary = format!("{path}.{}", std::process::id());
    if std::fs::write(&temporary, set_where(&text, &console, folder)).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
}

/// At the start of a console's first shell, in the home folder: back to
/// the folder the last shell on that console was in.
pub fn resume(shell: &mut Shell) {
    let top = shell.get("SHLVL").is_some_and(|l| l == "1");
    let home = shell.get("HOME").unwrap_or_default();
    let at_home =
        shell.get("PWD").is_some_and(|p| p.trim_end_matches('/') == home.trim_end_matches('/'));
    if !shell.settings.resume || shell.private || !top || !at_home || home.is_empty() {
        return;
    }
    let (Some(path), Some(console)) = (where_path(shell), console(shell)) else { return };
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let Some(folder) = get_where(&text, &console).map(str::to_string) else { return };
    if folder != home && std::fs::metadata(&folder).is_ok_and(|m| m.is_dir()) {
        crate::builtins::cd_to(shell, &folder);
    }
}

/// Alt+Left (`back`) or Alt+Right: the folder before or after this one.
/// Whether it moved.
pub fn step(shell: &mut Shell, back: bool) -> bool {
    let here = shell.get("PWD").unwrap_or_default();
    let target = if back { shell.dirs_back.pop() } else { shell.dirs_forward.pop() };
    let Some(target) = target else { return false };
    if !crate::builtins::cd_to(shell, &target) {
        return false;
    }
    if back {
        shell.dirs_forward.push(here);
    } else {
        shell.dirs_back.push(here);
    }
    shell.last_dir = shell.get("PWD").unwrap_or_default();
    true
}

/// `z [-l] [word ...]`: goes to the most used folder that matches the
/// words; `z -l` lists the matches, best first; `z` alone goes home.
pub fn z(shell: &mut Shell, args: &[String]) -> crate::shell::Res {
    let args = &args[1..];
    let (list, words): (bool, Vec<String>) = match args.first().map(String::as_str) {
        Some("-l") => (true, args[1..].to_vec()),
        Some("-h" | "--help") => {
            println!("usage: z [-l] [word ...]");
            return Ok(0);
        }
        Some("--") => (false, args[1..].to_vec()),
        _ => (false, args.to_vec()),
    };
    let now = crate::sys::now().0;
    let folders = load(shell);
    if list {
        let mut found: Vec<&Folder> = folders.iter().filter(|f| matches(&f.path, &words)).collect();
        found.sort_by(|a, b| score(b, now).total_cmp(&score(a, now)));
        for folder in found {
            println!("{:>8.1}  {}", score(folder, now), folder.path);
        }
        return Ok(0);
    }
    if words.is_empty() {
        let home = shell.get("HOME").unwrap_or_else(|| "/".into());
        return Ok(i32::from(!crate::builtins::cd_to(shell, &home)));
    }
    // A folder named as such goes there, as cd does.
    if words.len() == 1 && std::fs::metadata(&words[0]).is_ok_and(|m| m.is_dir()) {
        return Ok(i32::from(!crate::builtins::cd_to(shell, &words[0])));
    }
    let here = shell.get("PWD").unwrap_or_default();
    match best(&folders, &words, &here, now) {
        Some(folder) => {
            let path = folder.path.clone();
            Ok(i32::from(!crate::builtins::cd_to(shell, &path)))
        }
        None => {
            shell.error(&format!("z: no folder used before matches '{}'", words.join(" ")));
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn the_file_reads_back() {
        let folders = vec![Folder { rank: 3.5, time: 10, path: "/users/a/src".into() }];
        assert_eq!(parse(&format(&folders)), folders);
        assert!(parse("junk\n1\t2\trelative\n").is_empty());
    }

    #[test]
    fn words_match_in_order_and_the_last_in_the_last_part() {
        assert!(matches("/users/a/projects/site", &words("proj site")));
        assert!(matches("/users/a/Projects", &words("proj")));
        assert!(!matches("/users/a/projects/site", &words("site proj")));
        assert!(!matches("/users/a/projects/site", &words("proj")));
        assert!(matches("/anything", &[]));
    }

    #[test]
    fn the_most_used_lately_wins() {
        let now = 1_000_000;
        let mut folders = Vec::new();
        for _ in 0..3 {
            visit(&mut folders, "/tmp", now - 700_000);
        }
        visit(&mut folders, "/", now);
        assert_eq!(folders.len(), 2);
        // /tmp has 3 visits a week ago (0.75); / one just now (4).
        let found = best(&folders, &[], "/nowhere", now).unwrap();
        assert_eq!(found.path, "/");
        // Not the folder you are in.
        let found = best(&folders, &[], "/", now).unwrap();
        assert_eq!(found.path, "/tmp");
    }

    #[test]
    fn each_console_has_its_place() {
        let text = set_where("", "tty1", "/a");
        let text = set_where(&text, "tty2", "/b");
        let text = set_where(&text, "tty1", "/c");
        assert_eq!(get_where(&text, "tty1"), Some("/c"));
        assert_eq!(get_where(&text, "tty2"), Some("/b"));
        assert_eq!(get_where(&text, "tty3"), None);
    }

    #[test]
    fn ranks_age() {
        let mut folders = vec![
            Folder { rank: 9999.5, time: 0, path: "/a".into() },
            Folder { rank: 1.0, time: 0, path: "/b".into() },
        ];
        visit(&mut folders, "/a", 1);
        assert_eq!(folders.len(), 1);
        assert!(folders[0].rank < 9100.0);
    }
}
