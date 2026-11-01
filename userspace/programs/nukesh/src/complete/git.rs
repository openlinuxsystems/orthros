// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tab for git (ADR 0288): its commands with what each does, then for
//! each the words it takes, each with a description the menu shows:
//! branches with their last commit and its age, a remote's branches you
//! do not have yet, tags and the next versions, remotes, stashes, and
//! for `add` and `restore` only the files that changed, the files in
//! conflict first. Options come from the completion file written from
//! the command's own manual page (`git-switch` for `git switch`).

use std::path::PathBuf;

use super::files::FromFile;
use super::{Candidate, tier};
use crate::git::{self, Repo};
use crate::shell::Shell;

/// Subcommands that take a subcommand of their own.
const NESTED: &[(&str, &[(&str, &str)])] = &[
    (
        "stash",
        &[
            ("push", "Put the changes away"),
            ("pop", "Bring the newest stash back and drop it"),
            ("apply", "Bring a stash back and keep it"),
            ("list", "List the stashes"),
            ("show", "Show what a stash changes"),
            ("drop", "Delete a stash"),
            ("branch", "Make a branch from a stash"),
            ("clear", "Delete every stash"),
        ],
    ),
    (
        "remote",
        &[
            ("add", "Add a remote"),
            ("remove", "Remove a remote"),
            ("rename", "Rename a remote"),
            ("set-url", "Change where a remote is"),
            ("show", "Show a remote's branches"),
            ("prune", "Forget branches the remote no longer has"),
        ],
    ),
    (
        "worktree",
        &[
            ("add", "Check a branch out in another folder"),
            ("list", "List the working trees"),
            ("remove", "Remove a working tree"),
            ("move", "Move a working tree"),
            ("prune", "Forget working trees whose folders are gone"),
        ],
    ),
    (
        "bisect",
        &[
            ("start", "Start looking for the commit that broke something"),
            ("good", "This commit works"),
            ("bad", "This commit is broken"),
            ("skip", "This commit cannot be tested"),
            ("run", "Test each commit with a command"),
            ("reset", "Stop, and go back to where you were"),
            ("log", "Show what was marked"),
        ],
    ),
    (
        "submodule",
        &[
            ("add", "Add a repository inside this one"),
            ("update", "Check the submodules out at their recorded commits"),
            ("init", "Set the submodules up"),
            ("status", "Show each submodule's commit"),
            ("sync", "Copy the submodules' addresses to the settings"),
            ("deinit", "Take a submodule's checkout away"),
        ],
    ),
];

/// What each kind of command's operands are.
enum Wants {
    Refs { tags: bool },
    Changed { staged: bool },
    Remote,
    Tag,
    Commands,
}

fn wants(sub: &str, args: &[String]) -> Option<Wants> {
    let has = |o: &str| args.iter().any(|a| a == o);
    Some(match sub {
        "switch" | "merge" | "rebase" | "cherry-pick" | "branch" | "range-diff" => {
            Wants::Refs { tags: false }
        }
        "checkout" | "log" | "show" | "reset" | "revert" | "describe" | "diff" | "rev-parse"
        | "blame" | "shortlog" | "whatchanged" => Wants::Refs { tags: true },
        "add" => Wants::Changed { staged: false },
        "restore" => Wants::Changed { staged: has("--staged") || has("-S") },
        "push" | "pull" | "fetch" => Wants::Remote,
        "tag" => Wants::Tag,
        "help" => Wants::Commands,
        _ => return None,
    })
}

fn candidate(text: String, description: String, space: bool) -> Candidate {
    Candidate {
        display: text.clone(),
        text,
        space,
        description: (!description.is_empty()).then_some(description),
    }
}

/// The candidates of `list` that `word` starts, or else that it matches
/// loosely, as the shell's other words do.
fn matching(word: &str, list: Vec<Candidate>) -> Vec<Candidate> {
    let exact: Vec<Candidate> = list.iter().filter(|c| c.text.starts_with(word)).cloned().collect();
    if !exact.is_empty() || word.is_empty() {
        return exact;
    }
    let best = list.iter().filter_map(|c| tier(word, &c.text)).min();
    list.into_iter().filter(|c| best.is_some() && tier(word, &c.text) == best).collect()
}

impl Shell {
    /// Tab for the git command `words`, at `index`; nothing for the
    /// shell's own completion to do instead.
    pub fn complete_git(&mut self, words: &[String], index: usize) -> Option<Vec<Candidate>> {
        let word = words[index].clone();
        let here = PathBuf::from(self.get("PWD").unwrap_or_default());
        let repo = git::find(&here);
        let man = self.get("NUKESH_MAN_DIR").unwrap_or_else(|| "/system/resources/man".into());
        // Where the subcommand is: past git's own options.
        let mut probe: Vec<String> = words[..index].to_vec();
        probe.push("x".into());
        let (at, _) = git::subcommand_words(&probe);
        if at == index {
            if word.starts_with('-') {
                return None;
            }
            let list = git::commands(repo.as_ref(), &man)
                .into_iter()
                .map(|(name, about)| candidate(name, about, true))
                .collect();
            return Some(matching(&word, list));
        }
        let sub = words.get(at)?.clone();
        let args: Vec<String> = words[at + 1..index].to_vec();
        if word.starts_with('-') {
            let file = self.completion_file(&format!("git-{sub}"))?;
            let mut shifted = vec![format!("git-{sub}")];
            shifted.extend_from_slice(&words[at + 1..]);
            return match self.complete_from_file(&file, &shifted, index - at) {
                FromFile::Options(found) => Some(found),
                FromFile::Words { .. } => None,
            };
        }
        // A command of its own: `git stash pop`.
        let operands: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
        if let Some((_, nested)) = NESTED.iter().find(|(name, _)| *name == sub) {
            if operands.is_empty() {
                let list = nested
                    .iter()
                    .map(|(name, about)| candidate((*name).to_string(), (*about).to_string(), true))
                    .collect();
                return Some(matching(&word, list));
            }
            let repo = repo?;
            return match (sub.as_str(), operands[0].as_str()) {
                ("stash", "pop" | "apply" | "drop" | "show" | "branch") => {
                    Some(matching(&word, stashes(&repo)))
                }
                ("remote", "remove" | "rename" | "set-url" | "show" | "prune") => {
                    Some(matching(&word, remotes(&repo)))
                }
                ("bisect", "good" | "bad" | "skip" | "start") => {
                    Some(matching(&word, refs(&repo, true, false)))
                }
                _ => None,
            };
        }
        let repo = repo?;
        let found = match wants(&sub, &args)? {
            Wants::Refs { tags } => {
                let local_only = sub == "branch";
                let mut list = refs(&repo, tags, matches!(sub.as_str(), "switch" | "checkout"));
                if local_only {
                    list.retain(|c| !c.text.contains('/') || repo_has_branch(&repo, &c.text));
                }
                if sub == "diff" || sub == "checkout" {
                    list.extend(changed(&repo, false));
                }
                list
            }
            Wants::Changed { staged } => changed(&repo, staged),
            Wants::Remote => {
                if operands.is_empty() {
                    remotes(&repo)
                } else {
                    refs(&repo, sub != "push", false)
                }
            }
            Wants::Tag => {
                let deleting = args.iter().any(|a| a == "-d" || a == "--delete");
                let tags = git::refs(&repo, &["refs/tags"]);
                if deleting || args.iter().any(|a| a == "-v" || a == "--verify") {
                    tags.into_iter().map(|t| candidate(t.name, t.subject, true)).collect()
                } else if operands.is_empty() {
                    let names: Vec<String> = tags.into_iter().map(|t| t.name).collect();
                    let annotated = args.iter().any(|a| a == "-a" || a == "-s");
                    git::next_versions(&names)
                        .into_iter()
                        .map(|(version, about)| {
                            // An annotated tag is the default (ADR 0288).
                            let text = if annotated {
                                version.clone()
                            } else {
                                format!(
                                    "-a {version} -m \"Version {}\"",
                                    version.trim_start_matches('v')
                                )
                            };
                            Candidate {
                                display: version,
                                text,
                                space: true,
                                description: Some(about.to_string()),
                            }
                        })
                        .collect()
                } else {
                    refs(&repo, false, false)
                }
            }
            Wants::Commands => git::commands(Some(&repo), &man)
                .into_iter()
                .map(|(name, about)| candidate(name, about, true))
                .collect(),
        };
        // Display names match on what they show: the version for a tag.
        let found = found
            .into_iter()
            .filter(|c| c.display.starts_with(&word) || c.text.starts_with(&word))
            .collect::<Vec<_>>();
        Some(found)
    }
}

fn repo_has_branch(repo: &Repo, name: &str) -> bool {
    repo.common.join("refs/heads").join(name).exists()
}

/// Branches with their last commit and its age, the newest first; with
/// `tags`, tags after them; with `remote_names`, a remote's branches you
/// have no branch of by their own name, which git switch makes one of.
fn refs(repo: &Repo, tags: bool, remote_names: bool) -> Vec<Candidate> {
    let mut kinds = vec!["refs/heads", "refs/remotes"];
    if tags {
        kinds.push("refs/tags");
    }
    let found = git::refs(repo, &kinds);
    let remotes = git::remotes(repo);
    let locals: Vec<String> =
        git::refs(repo, &["refs/heads"]).into_iter().map(|r| r.name).collect();
    let mut list = Vec::new();
    for r in found {
        let about = match (r.subject.is_empty(), r.age.is_empty()) {
            (false, false) => format!("{} · {}", r.subject, r.age),
            _ => r.subject.clone(),
        };
        let remote =
            r.name.split_once('/').filter(|(remote, _)| remotes.iter().any(|x| x == remote));
        match remote {
            Some((remote, name)) if remote_names && !locals.iter().any(|l| l == name) => {
                list.push(candidate(name.to_string(), format!("{about} (from {remote})"), true));
            }
            _ => list.push(candidate(r.name, about, true)),
        }
    }
    let mut seen = std::collections::HashSet::new();
    list.retain(|c| seen.insert(c.text.clone()));
    list
}

fn remotes(repo: &Repo) -> Vec<Candidate> {
    git::remotes(repo).into_iter().map(|r| candidate(r, "a remote".into(), true)).collect()
}

/// The stashes, `stash@{n}`, with their messages.
fn stashes(repo: &Repo) -> Vec<Candidate> {
    git::output(&repo.work, &["stash", "list", "--format=%gd%x09%gs%x09%cr"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.to_string();
            let message = fields.next().unwrap_or_default();
            let age = fields.next().unwrap_or_default();
            Some(candidate(name, format!("{message} · {age}"), true))
        })
        .collect()
}

/// The files that changed, with how: those staged with `staged`, else
/// those not staged and the untracked; while files are in conflict,
/// only those.
fn changed(repo: &Repo, staged: bool) -> Vec<Candidate> {
    let Some(status) = git::status_now(repo) else { return Vec::new() };
    let in_conflict = status.conflicts > 0 && !staged;
    let base = std::env::current_dir().unwrap_or_default();
    let relative = |path: &str| {
        let full = repo.work.join(path);
        full.strip_prefix(&base)
            .map_or_else(|_| full.display().to_string(), |p| p.display().to_string())
    };
    status
        .files
        .iter()
        .filter_map(|(path, xy)| {
            let mut letters = xy.chars();
            let (x, y) = (letters.next().unwrap_or(' '), letters.next().unwrap_or(' '));
            let conflict = matches!(xy.as_str(), "UU" | "AA" | "DD" | "AU" | "UA" | "DU" | "UD");
            if in_conflict && !conflict {
                return None;
            }
            let state = if conflict {
                "in conflict"
            } else if xy == "??" {
                if staged {
                    return None;
                }
                "new file"
            } else {
                let letter = if staged { x } else { y };
                match letter {
                    'M' => "changed",
                    'A' => "added",
                    'D' => "deleted",
                    'R' => "renamed",
                    'C' => "copied",
                    'T' => "type changed",
                    _ => return None,
                }
            };
            Some(candidate(relative(path), state.to_string(), true))
        })
        .collect()
}
