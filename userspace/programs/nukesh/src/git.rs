// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! git in the shell (ADR 0288): what the right prompt shows of a
//! repository, Tab's words for git, the corrections, warnings and undos
//! of git commands, what the empty line suggests after one, and the
//! plain-words account of a repository's state that Alt+E gives.
//!
//! The branch, a detached head's tag, an operation in progress
//! (rebase, merge, cherry-pick, revert, bisect) and the stashes are read
//! from the repository's files, without running git. The counts of
//! files staged, changed, untracked and in conflict, and how far the
//! branch is ahead of and behind its upstream, come from `git status`,
//! run on a thread of its own: the prompt waits for it a moment at most
//! and otherwise shows what it found the last time, so that a big
//! repository never holds the prompt up.
//!
//! Everything here is a hint that shows the real git command: nothing
//! runs until Enter, and nothing hides or replaces git.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A repository: its work tree, its git folder (a worktree's own), and
/// the folder its branches and objects are in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub work: PathBuf,
    pub dir: PathBuf,
    pub common: PathBuf,
}

/// The repository `start` is in: `.git` in it or the nearest folder
/// above, a folder or, in a worktree or a submodule, a file naming it.
pub fn find(start: &Path) -> Option<Repo> {
    let mut work = start.to_path_buf();
    loop {
        let git = work.join(".git");
        if let Ok(meta) = std::fs::metadata(&git) {
            let dir = if meta.is_dir() {
                git
            } else {
                let text = std::fs::read_to_string(&git).ok()?;
                let target = text.trim().strip_prefix("gitdir:")?.trim();
                work.join(target)
            };
            let common = match std::fs::read_to_string(dir.join("commondir")) {
                Ok(text) => dir.join(text.trim()),
                Err(_) => dir.clone(),
            };
            return Some(Repo { work, dir, common });
        }
        if !work.pop() {
            return None;
        }
    }
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Where HEAD is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    /// On a branch, which may have no commit yet.
    Branch(String),
    /// On a commit: its short name, and a tag that names it.
    Detached { short: String, tag: Option<String> },
}

impl Head {
    pub fn branch(&self) -> Option<&str> {
        match self {
            Head::Branch(name) => Some(name),
            Head::Detached { .. } => None,
        }
    }
}

pub fn head(repo: &Repo) -> Option<Head> {
    let text = read(&repo.dir.join("HEAD"))?;
    let text = text.trim();
    if let Some(name) = text.strip_prefix("ref:") {
        let name = name.trim();
        return Some(Head::Branch(name.strip_prefix("refs/heads/").unwrap_or(name).to_string()));
    }
    let hash = text.to_string();
    let short = hash.chars().take(7).collect();
    Some(Head::Detached { short, tag: tag_at(repo, &hash) })
}

/// A tag that names the commit `hash`: a lightweight tag's file, or a
/// line of packed-refs, where an annotated tag's commit follows it after
/// `^`.
fn tag_at(repo: &Repo, hash: &str) -> Option<String> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(repo.common.join("refs/tags")) {
        for entry in entries.flatten() {
            if read(&entry.path()).is_some_and(|t| t.trim() == hash) {
                found.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    if let Some(packed) = read(&repo.common.join("packed-refs")) {
        let mut last: Option<&str> = None;
        for line in packed.lines() {
            if let Some(peeled) = line.strip_prefix('^') {
                if peeled.trim() == hash
                    && let Some(name) = last
                {
                    found.push(name.to_string());
                }
                continue;
            }
            last = None;
            let Some((value, name)) = line.split_once(' ') else { continue };
            if let Some(tag) = name.strip_prefix("refs/tags/") {
                if value == hash {
                    found.push(tag.to_string());
                }
                last = Some(tag);
            }
        }
    }
    found.sort();
    found.pop()
}

/// An operation git has stopped in the middle of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    Rebase {
        step: usize,
        total: usize,
    },
    Merge,
    CherryPick,
    Revert,
    Bisect,
    /// `git am`, applying mailed patches.
    Apply,
}

impl Operation {
    /// The command it is, as `git <command> --continue` takes it.
    pub fn command(&self) -> &'static str {
        match self {
            Operation::Rebase { .. } => "rebase",
            Operation::Merge => "merge",
            Operation::CherryPick => "cherry-pick",
            Operation::Revert => "revert",
            Operation::Bisect => "bisect",
            Operation::Apply => "am",
        }
    }

    /// What the prompt says.
    pub fn label(&self) -> String {
        match self {
            Operation::Rebase { step, total } if *total > 0 => format!("REBASING {step}/{total}"),
            Operation::Rebase { .. } => "REBASING".into(),
            Operation::Merge => "MERGING".into(),
            Operation::CherryPick => "CHERRY-PICKING".into(),
            Operation::Revert => "REVERTING".into(),
            Operation::Bisect => "BISECTING".into(),
            Operation::Apply => "APPLYING".into(),
        }
    }
}

pub fn operation(repo: &Repo) -> Option<Operation> {
    let number = |path: PathBuf| read(&path).and_then(|t| t.trim().parse::<usize>().ok());
    let merge = repo.dir.join("rebase-merge");
    if merge.is_dir() {
        return Some(Operation::Rebase {
            step: number(merge.join("msgnum")).unwrap_or(0),
            total: number(merge.join("end")).unwrap_or(0),
        });
    }
    let apply = repo.dir.join("rebase-apply");
    if apply.is_dir() {
        if apply.join("applying").exists() {
            return Some(Operation::Apply);
        }
        return Some(Operation::Rebase {
            step: number(apply.join("next")).unwrap_or(0),
            total: number(apply.join("last")).unwrap_or(0),
        });
    }
    if repo.dir.join("MERGE_HEAD").exists() {
        return Some(Operation::Merge);
    }
    if repo.dir.join("CHERRY_PICK_HEAD").exists() {
        return Some(Operation::CherryPick);
    }
    if repo.dir.join("REVERT_HEAD").exists() {
        return Some(Operation::Revert);
    }
    if repo.dir.join("BISECT_LOG").exists() {
        return Some(Operation::Bisect);
    }
    None
}

/// The stashes: the lines of the stash's reflog.
pub fn stashes(repo: &Repo) -> usize {
    read(&repo.common.join("logs/refs/stash")).map_or(0, |t| t.lines().count())
}

/// When the oldest stash was made, in seconds since 1970.
pub fn oldest_stash(repo: &Repo) -> Option<i64> {
    let text = read(&repo.common.join("logs/refs/stash"))?;
    let line = text.lines().next()?;
    // `old new Name <email> time zone\tmessage`
    let before_tab = line.split('\t').next()?;
    let mut words = before_tab.rsplit(' ');
    let _zone = words.next()?;
    words.next()?.parse().ok()
}

/// What `git status` says of a work tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub staged: usize,
    pub changed: usize,
    pub untracked: usize,
    pub conflicts: usize,
    /// Each file and its two-letter state: index, then work tree, as
    /// `git status --short` has it (`??` untracked, `UU` in conflict).
    pub files: Vec<(String, String)>,
}

impl Status {
    pub fn clean(&self) -> bool {
        self.staged + self.changed + self.untracked + self.conflicts == 0
    }
}

/// Reads `git status --porcelain=v2 --branch -z`.
pub fn parse_status(text: &str) -> Status {
    let mut status = Status::default();
    let mut fields = text.split('\0');
    while let Some(entry) = fields.next() {
        if let Some(header) = entry.strip_prefix("# ") {
            if let Some(name) = header.strip_prefix("branch.upstream ") {
                status.upstream = Some(name.to_string());
            } else if let Some(ab) = header.strip_prefix("branch.ab ") {
                for word in ab.split(' ') {
                    if let Some(n) = word.strip_prefix('+') {
                        status.ahead = n.parse().unwrap_or(0);
                    } else if let Some(n) = word.strip_prefix('-') {
                        status.behind = n.parse().unwrap_or(0);
                    }
                }
            }
            continue;
        }
        let kind = entry.chars().next();
        match kind {
            Some('1' | '2') => {
                // `1 XY sub mH mI mW hH hI path`; a `2` has a score before
                // the path and the old path in the next field.
                let parts = if kind == Some('1') { 9 } else { 10 };
                let mut words = entry.splitn(parts, ' ');
                let xy = words.nth(1).unwrap_or("..");
                let path = words.last().unwrap_or_default().to_string();
                if kind == Some('2') {
                    fields.next();
                }
                let (x, y) = (xy.chars().next().unwrap_or('.'), xy.chars().nth(1).unwrap_or('.'));
                if x != '.' {
                    status.staged += 1;
                }
                if y != '.' {
                    status.changed += 1;
                }
                status.files.push((path, xy.replace('.', " ")));
            }
            Some('u') => {
                let path = entry.splitn(11, ' ').last().unwrap_or_default().to_string();
                let xy = entry.split(' ').nth(1).unwrap_or("UU").to_string();
                status.conflicts += 1;
                status.files.push((path, xy));
            }
            Some('?') => {
                status.untracked += 1;
                status.files.push((entry[2..].to_string(), "??".into()));
            }
            _ => {}
        }
    }
    status
}

/// The last status found for each work tree, and when.
type Cache = Mutex<HashMap<PathBuf, (Status, Instant)>>;

fn cache() -> &'static Arc<Cache> {
    static CACHE: OnceLock<Arc<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

/// Runs `git status` in `work` and returns what it says.
pub fn run_status(work: &Path) -> Option<Status> {
    let out = Command::new("git")
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--ignore-submodules=dirty",
        ])
        .current_dir(work)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| parse_status(&String::from_utf8_lossy(&out.stdout)))
}

/// The status of `repo`'s work tree, waiting for `git status` at most
/// `wait`; after that, what was found the last time, while the run goes
/// on and leaves its answer for the next prompt.
pub fn status(repo: &Repo, wait: Duration) -> Option<Status> {
    let work = repo.work.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let shared = Arc::clone(cache());
    std::thread::spawn(move || {
        let found = run_status(&work);
        if let Some(found) = &found
            && let Ok(mut cache) = shared.lock()
        {
            cache.insert(work, (found.clone(), Instant::now()));
        }
        let _ = sender.send(found);
    });
    match receiver.recv_timeout(wait) {
        Ok(found) => found,
        Err(_) => cache().lock().ok()?.get(&repo.work).map(|(s, _)| s.clone()),
    }
}

/// The status, at once if it is fresh enough for Tab's words.
pub fn status_now(repo: &Repo) -> Option<Status> {
    if let Ok(cache) = cache().lock()
        && let Some((found, at)) = cache.get(&repo.work)
        && at.elapsed() < Duration::from_secs(2)
    {
        return Some(found.clone());
    }
    let found = run_status(&repo.work)?;
    if let Ok(mut cache) = cache().lock() {
        cache.insert(repo.work.clone(), (found.clone(), Instant::now()));
    }
    Some(found)
}

/// The right prompt's account of a repository, as (colour key, text)
/// pairs: the operation in progress, the branch or tag or commit, how
/// far ahead and behind, the files staged, changed, untracked and in
/// conflict, and the stashes. `plain` keeps to the letters every console
/// font has.
pub fn prompt_parts(
    repo: &Repo,
    status: Option<&Status>,
    plain: bool,
) -> Vec<(&'static str, String)> {
    let mut parts = Vec::new();
    if let Some(operation) = operation(repo) {
        parts.push(("git-state", operation.label()));
    }
    let (mark, tag_mark) = if plain { ("git ", "tag ") } else { ("⎇ ", "➦ ") };
    match head(repo) {
        Some(Head::Branch(name)) => parts.push(("branch", format!("{mark}{name}"))),
        Some(Head::Detached { tag: Some(tag), .. }) => {
            parts.push(("git-state", format!("{tag_mark}{tag}")));
        }
        Some(Head::Detached { short, .. }) => {
            parts.push(("git-state", format!("{tag_mark}{short} (detached)")));
        }
        None => {}
    }
    if let Some(status) = status {
        let (up, down) = if plain { ("^", "v") } else { ("↑", "↓") };
        let mut sync = Vec::new();
        if status.ahead > 0 {
            sync.push(format!("{up}{}", status.ahead));
        }
        if status.behind > 0 {
            sync.push(format!("{down}{}", status.behind));
        }
        if !sync.is_empty() {
            parts.push(("git-sync", sync.join(" ")));
        }
        if status.conflicts > 0 {
            let word = if status.conflicts == 1 { "conflict" } else { "conflicts" };
            parts.push(("git-conflict", format!("!{} {word}", status.conflicts)));
        }
        for (count, sign, key) in [
            (status.staged, '+', "git-staged"),
            (status.changed, '~', "git-changed"),
            (status.untracked, '?', "git-untracked"),
        ] {
            if count > 0 {
                parts.push((key, format!("{sign}{count}")));
            }
        }
    }
    let stashes = stashes(repo);
    if stashes > 0 {
        parts.push((
            "git-stash",
            if plain { format!("stash {stashes}") } else { format!("≡{stashes}") },
        ));
    }
    if status.is_some_and(Status::clean) && parts.len() == 1 {
        parts.push(("git-clean", if plain { "clean".into() } else { "✓".into() }));
    }
    parts
}

// What git is asked for: branches, tags, remotes and its commands.

/// Runs git in `dir` and returns its output, or nothing if it fails.
pub fn output(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A branch, tag or remote branch: its name, the subject of its last
/// commit and how long ago that was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub subject: String,
    pub age: String,
}

/// The refs under `kinds` (`refs/heads`, `refs/remotes`, `refs/tags`),
/// the newest commit first.
pub fn refs(repo: &Repo, kinds: &[&str]) -> Vec<Ref> {
    let mut args = vec![
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(refname:short)%09%(contents:subject)%09%(committerdate:relative)",
    ];
    args.extend_from_slice(kinds);
    output(&repo.work, &args)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.to_string();
            if name.ends_with("/HEAD") || name.is_empty() {
                return None;
            }
            Some(Ref {
                name,
                subject: fields.next().unwrap_or_default().to_string(),
                age: fields.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

pub fn remotes(repo: &Repo) -> Vec<String> {
    output(&repo.work, &["remote"]).unwrap_or_default().lines().map(str::to_string).collect()
}

/// The branch a new repository and most remotes call their main one:
/// the upstream's HEAD, or else `main` or `master`, whichever exists.
pub fn main_branch(repo: &Repo) -> String {
    for remote in ["origin", "upstream"] {
        if let Some(text) = read(&repo.common.join(format!("refs/remotes/{remote}/HEAD")))
            && let Some(target) = text.trim().strip_prefix("ref: refs/remotes/")
            && let Some((_, name)) = target.split_once('/')
        {
            return name.to_string();
        }
    }
    let exists = |name: &str| {
        repo.common.join("refs/heads").join(name).exists()
            || read(&repo.common.join("packed-refs"))
                .is_some_and(|p| p.lines().any(|l| l.ends_with(&format!(" refs/heads/{name}"))))
    };
    if !exists("main") && exists("master") { "master".into() } else { "main".into() }
}

/// The local branches merged into `into`, but not `into` itself or the
/// one checked out.
pub fn merged_branches(repo: &Repo, into: &str) -> Vec<String> {
    let current = head(repo).and_then(|h| h.branch().map(str::to_string));
    output(&repo.work, &["branch", "--format=%(refname:short)", "--merged", into])
        .unwrap_or_default()
        .lines()
        .filter(|b| *b != into && Some(*b) != current.as_deref())
        .filter(|b| !matches!(*b, "main" | "master" | "develop" | "trunk"))
        .map(str::to_string)
        .collect()
}

/// git's commands and what each does: those with a manual page of their
/// own (`git-<name>.1`), and the aliases in the settings.
pub fn commands(repo: Option<&Repo>, man_dir: &str) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(format!("{man_dir}/man1")) {
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            let Some(name) = file.strip_prefix("git-").and_then(|n| n.strip_suffix(".1")) else {
                continue;
            };
            if name.contains("--") || name.starts_with("sh-") || HIDDEN.contains(&name) {
                continue;
            }
            let summary = crate::explain::manual_summary(man_dir, &format!("git-{name}"))
                .map_or_default(|s| {
                    s.trim_start_matches(&format!("git-{name}"))
                        .trim_start_matches([' ', '-'])
                        .to_string()
                });
            found.push((name.to_string(), summary));
        }
    }
    let dir = repo.map_or_else(|| PathBuf::from("."), |r| r.work.clone());
    for line in output(&dir, &["config", "--get-regexp", r"^alias\."]).unwrap_or_default().lines() {
        if let Some((key, value)) = line.split_once(' ')
            && let Some(name) = key.strip_prefix("alias.")
        {
            found.push((name.to_string(), format!("alias for {value}")));
        }
    }
    found.sort();
    found.dedup_by(|a, b| a.0 == b.0);
    found
}

/// Commands with a page that are not run by name.
const HIDDEN: &[&str] = &["credential-cache--daemon", "mergetool--lib", "web--browse"];

/// The next versions after the newest tag that looks like one: the
/// patch, minor and major steps, keeping its `v` if it has one.
pub fn next_versions(tags: &[String]) -> Vec<(String, &'static str)> {
    let parse = |tag: &str| {
        let (prefix, rest) = match tag.strip_prefix('v') {
            Some(rest) => ("v", rest),
            None => ("", tag),
        };
        let numbers: Vec<u64> = rest.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
        (numbers.len() == 3).then(|| (prefix, numbers[0], numbers[1], numbers[2]))
    };
    let newest = tags.iter().filter_map(|t| parse(t)).max_by_key(|&(_, a, b, c)| (a, b, c));
    match newest {
        Some((p, a, b, c)) => vec![
            (format!("{p}{a}.{b}.{}", c + 1), "the next patch: fixes only"),
            (format!("{p}{a}.{}.0", b + 1), "the next minor version: new features"),
            (format!("{p}{}.0.0", a + 1), "the next major version: changes that break"),
        ],
        None => {
            vec![("v0.1.0".into(), "a first version"), ("v1.0.0".into(), "a first stable version")]
        }
    }
}

/// Words of a git command line past `git` and its own options (`-C`,
/// `-c` and the rest): the subcommand first.
pub fn subcommand_words(words: &[String]) -> (usize, Vec<String>) {
    let mut at = 1;
    while let Some(word) = words.get(at) {
        match word.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => at += 2,
            w if w.starts_with('-') => at += 1,
            _ => break,
        }
    }
    (at, words.get(at..).map_or_default(<[String]>::to_vec))
}

// Fixes, warnings, undos and the next command.

/// A git command that failed, corrected: a mistyped command, a first
/// push of a branch without an upstream, a branch name one typo away,
/// and a pull that cannot merge on its own.
pub fn fix(dir: &Path, words: &[&str], status: i32, man_dir: &str) -> Option<String> {
    if words.first().map(|w| w.rsplit('/').next().unwrap_or(w)) != Some("git") || words.len() < 2 {
        return None;
    }
    let repo = find(dir);
    let sub = words[1];
    let rest = words[2..].join(" ");
    let with = |name: &str| {
        if rest.is_empty() { format!("git {name}") } else { format!("git {name} {rest}") }
    };
    let known: Vec<(String, String)> = commands(repo.as_ref(), man_dir);
    if status == 1 && !sub.starts_with('-') && !known.iter().any(|(n, _)| n == sub) {
        let best = known
            .iter()
            .map(|(name, _)| (crate::suggest::distance(sub, name), name))
            .filter(|(d, name)| *d <= 2 && *d < name.len())
            .min_by_key(|(d, name)| (*d, name.len()))?;
        return Some(with(best.1));
    }
    let repo = repo?;
    match (sub, status) {
        ("push", 128) if words.len() == 2 => {
            let branch = head(&repo)?.branch()?.to_string();
            let remote = remotes(&repo).into_iter().next()?;
            Some(format!("git push -u {remote} {branch}"))
        }
        ("pull", 128) if words.len() == 2 => Some("git pull --rebase".into()),
        ("switch" | "checkout", 128 | 1) if words.len() == 3 => {
            let typed = words[2];
            let names: Vec<String> = refs(&repo, &["refs/heads", "refs/remotes"])
                .into_iter()
                .map(|r| match r.name.split_once('/') {
                    Some((remote, name)) if remotes(&repo).iter().any(|x| x == remote) => {
                        name.to_string()
                    }
                    _ => r.name,
                })
                .collect();
            let best = names
                .iter()
                .filter(|n| *n != typed)
                .map(|n| (crate::suggest::distance(&typed.to_lowercase(), &n.to_lowercase()), n))
                .filter(|(d, _)| *d <= 2)
                .min_by_key(|(d, n)| (*d, n.len()))?;
            Some(format!("git {sub} {}", best.1))
        }
        ("commit", 1) if !words.iter().any(|w| *w == "-a" || *w == "--all") => {
            let status = run_status(&repo.work)?;
            (status.staged == 0 && status.changed > 0).then(|| {
                let mut fixed =
                    words.iter().map(std::string::ToString::to_string).collect::<Vec<_>>();
                fixed.insert(2, "-a".into());
                fixed.join(" ")
            })
        }
        _ => None,
    }
}

/// What the empty line suggests after a git command that worked, when
/// the record has nothing better: a commit after `git add`, a push after
/// a commit, `--follow-tags` after a tag, the `--continue` once the last
/// conflict is resolved, and the branches a pull or switch left merged.
pub fn next_after(dir: &Path, last: &str) -> Option<String> {
    // In a list (`echo both >f; git add f`), the git command it ends with.
    let last = if last.trim_start().starts_with("git ") {
        last
    } else {
        last.rsplit([';', '&', '|']).next().unwrap_or(last)
    };
    let words: Vec<&str> = last.split_whitespace().collect();
    if words.first() != Some(&"git") || words.len() < 2 {
        return None;
    }
    let repo = find(dir)?;
    if let Some(operation) = operation(&repo)
        && operation != Operation::Bisect
        && words[1] == "add"
    {
        let status = run_status(&repo.work)?;
        return (status.conflicts == 0).then(|| format!("git {} --continue", operation.command()));
    }
    match words[1] {
        "add" => Some("git commit -m \"\"".into()),
        "commit" if !words.contains(&"--amend") => {
            let status = run_status(&repo.work)?;
            if status.upstream.is_some() || !remotes(&repo).is_empty() {
                Some("git push".into())
            } else {
                None
            }
        }
        "tag" if words.len() > 2 && !words.contains(&"-d") && !words.contains(&"-l") => {
            (!remotes(&repo).is_empty()).then(|| "git push --follow-tags".into())
        }
        "pull" | "switch" | "checkout" | "merge" | "fetch" => {
            let main = main_branch(&repo);
            if head(&repo)?.branch()? != main {
                return None;
            }
            let merged = merged_branches(&repo, &main);
            (!merged.is_empty()).then(|| format!("git branch -d {}", merged.join(" ")))
        }
        _ => None,
    }
}

/// Before a git command runs, what it should know: a commit on a
/// detached head belongs to no branch, and a commit straight to the main
/// branch of a repository shared with others is noted.
pub fn guardrail(dir: &Path, line: &str) -> Option<String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.first() != Some(&"git")
        || words.get(1) != Some(&"commit")
        || words.contains(&"--amend")
    {
        return None;
    }
    let repo = find(dir)?;
    if operation(&repo).is_some() {
        return None;
    }
    match head(&repo)? {
        Head::Detached { short, .. } => Some(format!(
            "note: HEAD is detached at {short}, so this commit will be on no branch. \
             To keep it, make one first: git switch -c <name>"
        )),
        Head::Branch(name) if name == main_branch(&repo) && !remotes(&repo).is_empty() => {
            Some(format!("note: committing straight to {name}"))
        }
        Head::Branch(_) => None,
    }
}

/// Warnings with this repository's numbers, for what cannot be undone:
/// the files a hard reset or a restore throws away and the commits it
/// moves past, what a forced push replaces, the files a clean deletes,
/// the commits a branch deleted with -D still has, and the stashes a
/// clear or drop loses.
pub fn warnings(dir: &Path, words: &[String]) -> Vec<String> {
    let Some(repo) = find(dir) else { return Vec::new() };
    let (_, words) = subcommand_words(words);
    let Some(sub) = words.first().map(String::as_str) else { return Vec::new() };
    let args = &words[1..];
    let has = |o: &str| args.iter().any(|a| a == o);
    let changed = || {
        run_status(&repo.work).map(|s| {
            s.files.iter().filter(|(_, xy)| xy != "??").map(|(p, _)| p.clone()).collect::<Vec<_>>()
        })
    };
    let list = |files: &[String]| {
        let shown: Vec<&str> = files.iter().take(4).map(String::as_str).collect();
        let more = files.len().saturating_sub(4);
        if more > 0 { format!("{}, and {more} more", shown.join(", ")) } else { shown.join(", ") }
    };
    let mut out = Vec::new();
    match sub {
        "reset" if has("--hard") => {
            if let Some(files) = changed()
                && !files.is_empty()
            {
                let word = if files.len() == 1 { "file" } else { "files" };
                out.push(format!(
                    "Throws away the changes in {} {word} for good: {}",
                    files.len(),
                    list(&files)
                ));
            }
            let target = args.iter().find(|a| !a.starts_with('-')).map_or("HEAD", String::as_str);
            if let Some(n) =
                output(&repo.work, &["rev-list", "--count", &format!("{target}..HEAD")])
                    .and_then(|t| t.trim().parse::<usize>().ok())
                    .filter(|n| *n > 0)
            {
                let branch = head(&repo).and_then(|h| h.branch().map(str::to_string));
                let what = branch.unwrap_or("HEAD".to_string());
                let word = if n == 1 { "commit" } else { "commits" };
                out.push(format!(
                    "Moves {what} back {n} {word}; they stay findable for 90 days with git reflog"
                ));
            }
        }
        "restore" | "checkout"
            if args.iter().any(|a| a == "." || a == "--") && !has("--staged") =>
        {
            if let Some(files) = changed()
                && !files.is_empty()
            {
                out.push(format!("Throws away the changes not committed in {}", list(&files)));
            }
        }
        "push" if has("-f") || has("--force") || args.iter().any(|a| a.starts_with('+')) => {
            let branch = head(&repo).and_then(|h| h.branch().map(str::to_string));
            if let Some(branch) = branch {
                let remote = args
                    .iter()
                    .find(|a| !a.starts_with('-'))
                    .cloned()
                    .unwrap_or_else(|| "origin".into());
                let theirs = format!("{remote}/{branch}");
                if let Some(n) =
                    output(&repo.work, &["rev-list", "--count", &format!("HEAD..{theirs}")])
                        .and_then(|t| t.trim().parse::<usize>().ok())
                {
                    let lost = if n == 0 {
                        "nothing there is missing here now".to_string()
                    } else {
                        let word = if n == 1 { "commit" } else { "commits" };
                        format!("{n} {word} there that you don't have will be lost")
                    };
                    out.push(format!("Replaces {theirs} with your {branch}: {lost}"));
                }
            }
            out.push(
                "Safer: git push --force-with-lease, which stops if someone else pushed".into(),
            );
        }
        "clean"
            if args
                .iter()
                .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
                || has("--force") =>
        {
            let mut dry = vec!["clean", "-n"];
            for a in args {
                if a.starts_with('-') && !a.starts_with("--") {
                    for c in a[1..].chars().filter(|c| matches!(c, 'd' | 'x' | 'X')) {
                        dry.push(match c {
                            'd' => "-d",
                            'x' => "-x",
                            _ => "-X",
                        });
                    }
                }
            }
            if let Some(text) = output(&repo.work, &dry) {
                let files: Vec<String> = text
                    .lines()
                    .filter_map(|l| l.strip_prefix("Would remove "))
                    .map(str::to_string)
                    .collect();
                if !files.is_empty() {
                    out.push(format!(
                        "Deletes {} files git does not track, for good: {}",
                        files.len(),
                        list(&files)
                    ));
                }
            }
        }
        "branch" if has("-D") || (has("-d") && has("--force")) => {
            for name in args.iter().filter(|a| !a.starts_with('-')) {
                let main = main_branch(&repo);
                if let Some(n) =
                    output(&repo.work, &["rev-list", "--count", &format!("{main}..{name}")])
                        .and_then(|t| t.trim().parse::<usize>().ok())
                        .filter(|n| *n > 0)
                {
                    let word = if n == 1 { "commit" } else { "commits" };
                    out.push(format!(
                        "Deletes {name}, which has {n} {word} not in {main}; git reflog finds them for a while"
                    ));
                }
            }
        }
        "stash" if args.first().is_some_and(|a| a == "clear") => {
            let n = stashes(&repo);
            if n > 0 {
                out.push(format!("Deletes all {n} stashes"));
            }
        }
        "stash" if args.first().is_some_and(|a| a == "drop") => {
            out.push(
                "Deletes the stash; git fsck --unreachable can still find it for a while".into(),
            );
        }
        _ => {}
    }
    out
}

/// How to undo what a git command does, for Alt+E.
pub fn undo(dir: &Path, words: &[String]) -> Option<String> {
    let (_, words) = subcommand_words(words);
    let sub = words.first()?.as_str();
    let args = &words[1..];
    let has = |o: &str| args.iter().any(|a| a == o);
    let branch = find(dir).and_then(|r| head(&r)).and_then(|h| h.branch().map(str::to_string));
    Some(match sub {
        "add" => "git restore --staged <files> unstages them; the changes stay".into(),
        "commit" if has("--amend") => {
            "git reset --soft HEAD@{1} goes back to the commit before the amend".into()
        }
        "commit" => "git reset --soft HEAD~1 undoes the commit and keeps its changes staged".into(),
        "reset" if has("--hard") => {
            "git reset --hard HEAD@{1} goes back, but changes not committed are gone".into()
        }
        "reset" => "git reset HEAD@{1} goes back to where HEAD was".into(),
        "merge" | "rebase" | "pull" if has("--abort") || has("--continue") => return None,
        "merge" | "pull" => {
            "git reset --hard ORIG_HEAD, or git merge --abort while it stops on a conflict".into()
        }
        "rebase" => "git rebase --abort while it runs; after, git reset --hard ORIG_HEAD".into(),
        "cherry-pick" => {
            "git cherry-pick --abort while it runs; after, git reset --hard HEAD~1".into()
        }
        "switch" | "checkout" => "git switch - goes back to where you were".into(),
        "stash" if args.first().is_none_or(|a| a == "push") => {
            "git stash pop brings the changes back".into()
        }
        "stash" if args.first().is_some_and(|a| a == "pop") => {
            "git stash push puts them away again".into()
        }
        "branch" if has("-d") || has("-D") => {
            "git reflog shows where it was: git branch <name> <commit> brings it back".into()
        }
        "branch" => "git branch -d <name> deletes it".into(),
        "tag" if has("-d") => {
            "git tag <name> <commit> makes it again; git reflog does not keep tags".into()
        }
        "tag" => {
            "git tag -d <name> deletes it; if pushed, git push origin :refs/tags/<name> too".into()
        }
        "push" => match branch {
            Some(b) => format!(
                "A push can't be taken back for others; git revert <commit> and push again is the safe undo on {b}"
            ),
            None => {
                "A push can't be taken back for others; git revert <commit> and push again".into()
            }
        },
        "restore" | "clean" => "There is no undo: what is thrown away was never committed".into(),
        "rm" => "git restore --staged --worktree <files> brings them back".into(),
        "mv" => "git mv <new> <old> moves it back".into(),
        _ => return None,
    })
}

/// A repository's state in plain words, and the next step, for Alt+E
/// on an empty line.
pub fn describe(dir: &Path) -> Option<String> {
    let repo = find(dir)?;
    let head = head(&repo)?;
    let status = run_status(&repo.work);
    let mut lines = Vec::new();
    let mut next: Option<String> = None;
    match &head {
        Head::Branch(name) => {
            let upstream = status.as_ref().and_then(|s| s.upstream.clone());
            match (upstream, &status) {
                (Some(up), Some(s)) if s.ahead > 0 && s.behind > 0 => {
                    lines.push(format!(
                        "You are on {name}, which has {} commits {up} doesn't, and {up} has {} you don't.",
                        s.ahead, s.behind
                    ));
                    next.get_or_insert("git pull --rebase, then git push".into());
                }
                (Some(up), Some(s)) if s.ahead > 0 => {
                    lines.push(format!("You are on {name}, {} commits ahead of {up}.", s.ahead));
                    next.get_or_insert("git push".into());
                }
                (Some(up), Some(s)) if s.behind > 0 => {
                    lines.push(format!("You are on {name}, {} commits behind {up}.", s.behind));
                    next.get_or_insert("git pull".into());
                }
                (Some(up), _) => {
                    lines.push(format!("You are on {name}, up to date with {up} as last fetched."));
                }
                (None, _) => {
                    lines.push(format!("You are on {name}, which is not pushed anywhere yet."));
                }
            }
        }
        Head::Detached { short, tag } => {
            let at =
                tag.as_ref().map_or_else(|| format!("commit {short}"), |t| format!("the tag {t}"));
            lines.push(format!(
                "HEAD is detached at {at}: you are looking at a commit, not on a branch. \
                 Commits made here belong to no branch."
            ));
            next.get_or_insert(
                "git switch - to go back, or git switch -c <name> to start a branch here".into(),
            );
        }
    }
    if let Some(operation) = operation(&repo) {
        let conflicts = status.as_ref().map_or(0, |s| s.conflicts);
        let what = match &operation {
            Operation::Rebase { step, total } if *total > 0 => {
                format!("A rebase stopped at commit {step} of {total}")
            }
            Operation::Rebase { .. } => "A rebase is under way".into(),
            Operation::Merge => "A merge is under way".into(),
            Operation::CherryPick => "A cherry-pick is under way".into(),
            Operation::Revert => "A revert is under way".into(),
            Operation::Bisect => "A bisect is under way: git is helping you find a commit".into(),
            Operation::Apply => "git am is applying patches".into(),
        };
        let command = operation.command();
        if operation == Operation::Bisect {
            lines.push(format!("{what}."));
            next =
                Some("git bisect good or git bisect bad, then git bisect reset when done".into());
        } else if conflicts > 0 {
            let word = if conflicts == 1 { "file is" } else { "files are" };
            lines.push(format!("{what}: {conflicts} {word} in conflict."));
            next = Some(format!(
                "fix the files, git add them, then git {command} --continue (or git {command} --abort)"
            ));
        } else {
            lines.push(format!("{what}, with no conflicts left."));
            next = Some(format!("git {command} --continue"));
        }
    }
    if let Some(s) = &status {
        let mut counts = Vec::new();
        for (n, one, many) in [
            (s.staged, "file staged for the next commit", "files staged for the next commit"),
            (s.changed, "file changed and not staged", "files changed and not staged"),
            (s.untracked, "new file git does not track yet", "new files git does not track yet"),
        ] {
            if n > 0 {
                counts.push(format!("{n} {}", if n == 1 { one } else { many }));
            }
        }
        if counts.is_empty() && s.conflicts == 0 {
            lines.push("Nothing has changed since the last commit.".into());
        } else if !counts.is_empty() {
            lines.push(format!("{}.", capitalise(&counts.join(", "))));
            if s.staged > 0 {
                next.get_or_insert("git commit (Alt+C opens the commit sheet)".into());
            } else if s.changed + s.untracked > 0 {
                next.get_or_insert("git add <files>, then git commit".into());
            }
        }
    }
    let n = stashes(&repo);
    if n > 0 {
        let word = if n == 1 { "stash" } else { "stashes" };
        lines.push(format!("{n} {word} put away (git stash list)."));
    }
    if let Some(next) = next {
        lines.push(format!("Next: {next}"));
    }
    Some(lines.join("\n"))
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A reminder of stashes a month old, once for each repository a
/// session enters.
pub fn reminder(
    dir: &Path,
    now: i64,
    told: &mut std::collections::HashSet<PathBuf>,
) -> Option<String> {
    let repo = find(dir)?;
    if !told.insert(repo.work.clone()) {
        return None;
    }
    let oldest = oldest_stash(&repo)?;
    let days = (now - oldest) / 86_400;
    (days >= 30).then(|| {
        let n = stashes(&repo);
        let word = if n == 1 { "stash" } else { "stashes" };
        format!("note: {n} {word} here, the oldest from {days} days ago (git stash list)")
    })
}

/// The oh-my-zsh git plugin's most used abbreviations (its MIT names),
/// which `[git] abbreviations = true` turns on.
pub const ABBREVIATIONS: &[(&str, &str)] = &[
    ("g", "git"),
    ("ga", "git add"),
    ("gaa", "git add --all"),
    ("gb", "git branch"),
    ("gc", "git commit"),
    ("gcm", "git commit -m"),
    ("gco", "git checkout"),
    ("gcb", "git checkout -b"),
    ("gsw", "git switch"),
    ("gswc", "git switch -c"),
    ("gd", "git diff"),
    ("gds", "git diff --staged"),
    ("gf", "git fetch"),
    ("gl", "git pull"),
    ("gp", "git push"),
    ("glo", "git log --oneline --decorate"),
    ("glog", "git log --oneline --decorate --graph"),
    ("gst", "git status"),
    ("gss", "git status --short"),
    ("gsta", "git stash push"),
    ("gstp", "git stash pop"),
    ("grb", "git rebase"),
    ("gm", "git merge"),
    ("gcl", "git clone"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nukesh-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn porcelain_v2_is_counted() {
        let text = "# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0\
                    # branch.ab +2 -1\0\
                    1 M. N... 100644 100644 100644 a b src/a.rs\0\
                    1 .M N... 100644 100644 100644 a b src/b.rs\0\
                    2 R. N... 100644 100644 100644 a b R100 new.rs\0old.rs\0\
                    u UU N... 100644 100644 100644 100644 a b c clash.rs\0\
                    ? notes.txt\0";
        let s = parse_status(text);
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert_eq!((s.staged, s.changed, s.untracked, s.conflicts), (2, 1, 1, 1));
        assert_eq!(s.files[2], ("new.rs".to_string(), "R ".to_string()));
        assert_eq!(s.files[3], ("clash.rs".to_string(), "UU".to_string()));
        assert_eq!(s.files[4], ("notes.txt".to_string(), "??".to_string()));
    }

    #[test]
    fn versions_follow_the_newest_tag() {
        let tags = vec!["v1.2.3".to_string(), "v1.10.0".to_string(), "nightly".to_string()];
        let next: Vec<String> = next_versions(&tags).into_iter().map(|v| v.0).collect();
        assert_eq!(next, ["v1.10.1", "v1.11.0", "v2.0.0"]);
        assert_eq!(next_versions(&[])[0].0, "v0.1.0");
    }

    #[test]
    fn a_repository_is_read_without_git() {
        let dir = scratch("read");
        git(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("f"), "one\n").unwrap();
        git(&dir, &["add", "f"]);
        git(&dir, &["commit", "-q", "-m", "First"]);
        std::fs::create_dir(dir.join("sub")).unwrap();
        let repo = find(&dir.join("sub")).unwrap();
        assert_eq!(repo.work, dir);
        assert_eq!(head(&repo), Some(Head::Branch("main".into())));
        git(&dir, &["tag", "-a", "-m", "One", "v1"]);
        git(&dir, &["pack-refs", "--all"]);
        git(&dir, &["checkout", "-q", "--detach"]);
        assert!(matches!(head(&repo), Some(Head::Detached { tag: Some(t), .. }) if t == "v1"));
        git(&dir, &["switch", "-q", "main"]);
        std::fs::write(dir.join("f"), "two\n").unwrap();
        git(&dir, &["stash", "-q"]);
        assert_eq!(stashes(&repo), 1);
        assert!(oldest_stash(&repo).is_some());
        // A conflict stops a merge.
        git(&dir, &["switch", "-q", "-c", "side"]);
        std::fs::write(dir.join("f"), "side\n").unwrap();
        git(&dir, &["commit", "-q", "-am", "Side"]);
        git(&dir, &["switch", "-q", "main"]);
        std::fs::write(dir.join("f"), "main\n").unwrap();
        git(&dir, &["commit", "-q", "-am", "Main"]);
        let _ = Command::new("git").args(["merge", "-q", "side"]).current_dir(&dir).output();
        assert_eq!(operation(&repo), Some(Operation::Merge));
        let status = run_status(&dir).unwrap();
        assert_eq!(status.conflicts, 1);
        let parts = prompt_parts(&repo, Some(&status), true);
        assert_eq!(parts[0], ("git-state", "MERGING".to_string()));
        assert!(parts.contains(&("git-conflict", "!1 conflict".to_string())));
        assert!(describe(&dir).unwrap().contains("1 file is in conflict"));
        git(&dir, &["merge", "--abort"]);
        assert_eq!(merged_branches(&repo, "main"), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn warnings_carry_the_numbers() {
        let dir = scratch("warn");
        git(&dir, &["init", "-q", "-b", "main"]);
        for n in 1..=3 {
            std::fs::write(dir.join("f"), format!("{n}\n")).unwrap();
            git(&dir, &["add", "f"]);
            git(&dir, &["commit", "-q", "-m", &format!("C{n}")]);
        }
        std::fs::write(dir.join("f"), "changed\n").unwrap();
        std::fs::write(dir.join("junk"), "x\n").unwrap();
        let words = |line: &str| line.split(' ').map(str::to_string).collect::<Vec<_>>();
        let w = warnings(&dir, &words("git reset --hard HEAD~2"));
        assert_eq!(w[0], "Throws away the changes in 1 file for good: f");
        assert_eq!(
            w[1],
            "Moves main back 2 commits; they stay findable for 90 days with git reflog"
        );
        let w = warnings(&dir, &words("git clean -fd"));
        assert_eq!(w, ["Deletes 1 files git does not track, for good: junk"]);
        assert!(undo(&dir, &words("git commit -m x")).unwrap().contains("reset --soft HEAD~1"));
        assert_eq!(guardrail(&dir, "git commit -m x"), None);
        git(&dir, &["checkout", "-q", "--detach"]);
        assert!(guardrail(&dir, "git commit -m x").unwrap().contains("detached"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subcommands_come_after_gits_options() {
        let words: Vec<String> =
            ["git", "-C", "x", "--no-pager", "log", "-1"].map(String::from).to_vec();
        assert_eq!(subcommand_words(&words), (4, vec!["log".to_string(), "-1".to_string()]));
    }
}
