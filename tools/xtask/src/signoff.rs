// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob signoff`: every commit carries a Developer Certificate of Origin
//! sign-off from a person (ADR 0025, CONTRIBUTING.md).

use std::process::Command;

use crate::process::{self, step};
use crate::{Ctx, Error, Result};

/// The branch a pull request merges into, used when no base is given.
const DEFAULT_BASE: &str = "origin/main";

/// Identities of tools that write commits but cannot certify the DCO.
const TOOL_EMAILS: &[&str] = &["noreply@anthropic.com"];

/// Separates the fields of one commit in `git log` output.
const FIELD: char = '\u{1f}';
/// Separates commits in `git log` output.
const RECORD: char = '\u{1e}';
/// Separates the Signed-off-by values of one commit.
const VALUE: char = '\u{1d}';

/// One commit as `ob signoff` sees it.
#[derive(Debug, PartialEq)]
struct Commit {
    hash: String,
    subject: String,
    author: Identity,
    signoffs: Vec<Identity>,
}

/// A `Name <email>` pair.
#[derive(Debug, PartialEq)]
struct Identity {
    name: String,
    email: String,
}

impl Identity {
    /// Parses `Name <email>`, as in a Signed-off-by trailer.
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (name, rest) = text.split_once('<')?;
        let email = rest.strip_suffix('>')?.trim();
        let name = name.trim();
        if name.is_empty() || !email.contains('@') {
            return None;
        }
        Some(Self { name: name.to_string(), email: email.to_string() })
    }

    /// Whether this is a tool rather than a person: an AI assistant or a
    /// GitHub App.
    fn is_tool(&self) -> bool {
        TOOL_EMAILS.iter().any(|tool| self.email.eq_ignore_ascii_case(tool))
            || self.name.ends_with("[bot]")
    }

    fn same_email(&self, other: &Identity) -> bool {
        self.email.eq_ignore_ascii_case(&other.email)
    }
}

/// `ob signoff [<base>]`.
pub fn check(ctx: &Ctx, base: Option<&str>) -> Result<()> {
    let base = match base {
        Some(base) => base.to_string(),
        None => merge_base(ctx)?,
    };
    step(&format!("Checking sign-offs on the commits after {}", short(&base)));
    let mut cmd = git(ctx);
    cmd.args(["log", "--no-merges"]).arg(format!(
        "--format=%H{FIELD}%s{FIELD}%an{FIELD}%ae{FIELD}\
         %(trailers:key=Signed-off-by,valueonly,separator=%x1d){RECORD}"
    ));
    cmd.arg(format!("{base}..HEAD"));
    let output = process::output(ctx, &mut cmd)?;
    let commits = parse_log(&String::from_utf8_lossy(&output.stdout));

    let problems: Vec<String> = commits
        .iter()
        .filter_map(|commit| {
            problem(commit).map(|why| format!("{} {}: {why}", short(&commit.hash), commit.subject))
        })
        .collect();
    if problems.is_empty() {
        println!("All {} commits are signed off.", commits.len());
        return Ok(());
    }
    for line in &problems {
        eprintln!("  {line}");
    }
    Err(Error::new(format!(
        "{} of {} commits lack a Developer Certificate of Origin sign-off",
        problems.len(),
        commits.len()
    ))
    .hint(format!(
        "add one with `git rebase --signoff {}`, then force-push; CONTRIBUTING.md explains it",
        short(&base)
    )))
}

/// The commit where HEAD left the main branch.
fn merge_base(ctx: &Ctx) -> Result<String> {
    let mut cmd = git(ctx);
    cmd.args(["merge-base", DEFAULT_BASE, "HEAD"]);
    let output = process::output(ctx, &mut cmd).map_err(|err| {
        err.hint(format!("fetch {DEFAULT_BASE} first, or name the base: `./ob signoff <commit>`"))
    })?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git(ctx: &Ctx) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(&ctx.root);
    cmd
}

fn short(hash: &str) -> &str {
    if hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()) { &hash[..12] } else { hash }
}

fn parse_log(text: &str) -> Vec<Commit> {
    text.split(RECORD)
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').split(FIELD);
            let hash = fields.next()?.trim();
            if hash.is_empty() {
                return None;
            }
            let subject = fields.next()?.to_string();
            let author = Identity { name: fields.next()?.into(), email: fields.next()?.into() };
            let signoffs = fields
                .next()
                .unwrap_or_default()
                .split(VALUE)
                .filter_map(Identity::parse)
                .collect();
            Some(Commit { hash: hash.to_string(), subject, author, signoffs })
        })
        .collect()
}

/// Why `commit` is not properly signed off, if it is not.
///
/// A person signs off their own commits. A tool cannot certify anything, so
/// its sign-offs do not count, and a commit a tool wrote needs the sign-off
/// of the person who submits it.
fn problem(commit: &Commit) -> Option<&'static str> {
    let people: Vec<&Identity> = commit.signoffs.iter().filter(|s| !s.is_tool()).collect();
    if commit.author.is_tool() {
        if people.is_empty() {
            return Some(
                "written by a tool, so it needs a Signed-off-by line from the person submitting it",
            );
        }
    } else if !people.iter().any(|s| s.same_email(&commit.author)) {
        return Some(if commit.signoffs.is_empty() {
            "no Signed-off-by line"
        } else {
            "no Signed-off-by line with the author's email address"
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(text: &str) -> Identity {
        Identity::parse(text).unwrap()
    }

    fn commit(author: &str, signoffs: &[&str]) -> Commit {
        Commit {
            hash: "0".repeat(40),
            subject: "Do a thing".into(),
            author: identity(author),
            signoffs: signoffs.iter().map(|s| identity(s)).collect(),
        }
    }

    #[test]
    fn parses_identities() {
        let id = identity(" Random J Developer <random@developer.example.org> ");
        assert_eq!(id.name, "Random J Developer");
        assert_eq!(id.email, "random@developer.example.org");
        assert!(Identity::parse("no email").is_none());
        assert!(Identity::parse("<only@email>").is_none());
        assert!(Identity::parse("Name <not-an-email>").is_none());
    }

    #[test]
    fn parses_git_log_output() {
        let log = format!(
            "aaaa{FIELD}First{FIELD}Ann{FIELD}ann@example.org{FIELD}\
             Ann <ann@example.org>{VALUE}Bob <bob@example.org>{RECORD}\n\
             bbbb{FIELD}Second{FIELD}Bob{FIELD}bob@example.org{FIELD}{RECORD}\n"
        );
        let commits = parse_log(&log);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "First");
        assert_eq!(commits[0].signoffs.len(), 2);
        assert_eq!(commits[1].hash, "bbbb");
        assert!(commits[1].signoffs.is_empty());
    }

    #[test]
    fn people_sign_off_their_own_commits() {
        let ann = "Ann <ann@example.org>";
        assert_eq!(problem(&commit(ann, &[ann])), None);
        assert_eq!(problem(&commit(ann, &["ANN <Ann@Example.org>"])), None);
        assert!(problem(&commit(ann, &[])).is_some());
        assert!(problem(&commit(ann, &["Bob <bob@example.org>"])).is_some());
    }

    #[test]
    fn tools_need_a_person_to_sign_off() {
        let claude = "Claude <noreply@anthropic.com>";
        let bot = "dependabot[bot] <49699333+dependabot[bot]@users.noreply.github.com>";
        assert_eq!(problem(&commit(claude, &["Ann <ann@example.org>"])), None);
        assert!(problem(&commit(claude, &[])).is_some());
        assert!(problem(&commit(claude, &[claude])).is_some());
        assert!(problem(&commit(bot, &[bot])).is_some());
        assert!(problem(&commit("Ann <ann@example.org>", &[claude])).is_some());
    }

    #[test]
    fn shortens_only_full_hashes() {
        assert_eq!(short(&"a".repeat(40)), "aaaaaaaaaaaa");
        assert_eq!(short("origin/main"), "origin/main");
    }
}
