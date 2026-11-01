// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the line editor offers as you type (ADRs 0259 and 0262): the
//! faded hint after the cursor, the Ctrl+R picker, and the right prompt.
//!
//! The hint is what an option does while you type one, from the
//! program's completion file (`-l  use a long listing format`), and
//! otherwise the newest command in the record that starts with what you
//! typed, one run in this folder first. → or End takes it; an option's
//! description is never taken, only the rest of its name.
//!
//! Ctrl+R opens the picker: the commands of the record, each once,
//! matched loosely (`gst` finds `git status`) and ranked by how well they
//! match, then by whether they ran in this folder, then by how lately;
//! each with how it ended (`✗ 2`), how long it took, when, and where.

use std::cell::RefCell;
use std::rc::Rc;

use reedline::{Completer, CompletionResult, Hinter, History, Span, Suggestion};

use crate::shell::Shell;

/// The shell, as reedline's parts hold it.
#[derive(Clone)]
pub struct Handle(pub Rc<RefCell<Shell>>);

// SAFETY: as for the editor's own handle: reedline calls the hinter and
// the picker's completer only from `read_line`, on the shell's thread.
unsafe impl Send for Handle {}

/// The faded text after the cursor.
pub struct ShellHinter {
    shell: Handle,
    style: nu_ansi_term::Style,
    history: bool,
    options: bool,
    /// What is shown, and what → takes of it.
    shown: String,
    taken: String,
    /// The empty line's hint, worked out once per prompt: for which last
    /// command, status and folder, and what it is.
    empty: Option<((String, i32, String), String, String)>,
}

impl ShellHinter {
    pub fn new(shell: Handle, style: nu_ansi_term::Style, history: bool, options: bool) -> Self {
        ShellHinter {
            shell,
            style,
            history,
            options,
            shown: String::new(),
            taken: String::new(),
            empty: None,
        }
    }
}

impl Hinter for ShellHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        history: &dyn History,
        use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        self.shown.clear();
        self.taken.clear();
        if pos != line.len() || (line.trim().is_empty() && !line.is_empty()) {
            return String::new();
        }
        let Ok(mut shell) = self.shell.0.try_borrow_mut() else { return String::new() };
        if line.is_empty() {
            let key =
                (shell.last_command.clone(), shell.status, shell.get("PWD").unwrap_or_default());
            if self.empty.as_ref().is_none_or(|(k, _, _)| *k != key) {
                let (shown, taken) = empty_line_hint(&shell, &key.0, key.1, &key.2);
                self.empty = Some((key, shown, taken));
            }
            if let Some((_, shown, taken)) = &self.empty {
                self.shown.clone_from(shown);
                self.taken.clone_from(taken);
            }
            if self.shown.is_empty() || !use_ansi_coloring {
                self.taken.clear();
                self.shown.clear();
                return String::new();
            }
            return self.style.paint(&self.shown).to_string();
        }
        if let Some((token, message)) = crate::editor::mistake(line, shell.parse_options()) {
            // Syntax mistakes, before Enter (ADR 0274); → takes nothing.
            self.shown = match token {
                Some(token) => format!("  ← `{token}` cannot go there"),
                None => format!(
                    "  ← {}",
                    message.trim_start_matches("syntax error").trim_start_matches(": ").trim()
                ),
            };
        } else if self.options
            && let Some((rest, about)) = option_help(&mut shell, line)
        {
            self.taken = rest.clone();
            self.shown = if about.is_empty() { rest } else { format!("{rest}  {about}") };
        } else if self.history {
            let folder = shell.get("PWD").unwrap_or_default();
            let found = match &shell.records {
                Some(records) => records.hint(line, &folder).map(str::to_string),
                None => history
                    .search(reedline::SearchQuery::last_with_prefix(line.to_string(), None))
                    .ok()
                    .and_then(|found| found.first().map(|item| item.command_line.clone())),
            };
            if let Some(command) = found.filter(|c| c.len() > line.len() && c.starts_with(line)) {
                self.taken = command[line.len()..].to_string();
                self.shown.clone_from(&self.taken);
            }
        }
        // A hint shows only in its colour: without colours it would look
        // typed.
        if self.shown.is_empty() || !use_ansi_coloring {
            self.taken.clear();
            self.shown.clear();
            return String::new();
        }
        self.style.paint(self.shown.replace('\n', " ")).to_string()
    }

    fn complete_hint(&self) -> String {
        self.taken.clone()
    }

    fn next_hint_token(&self) -> String {
        let rest = &self.taken;
        let start = rest.len() - rest.trim_start().len();
        let end = rest[start..].find(char::is_whitespace).map_or(rest.len(), |i| start + i);
        rest[..end].to_string()
    }
}

/// What an empty line shows (ADRs 0268 and 0270), and what → takes of
/// it: after a failure, the offer of Alt+E; otherwise the command most
/// often run after the last one, from the record, or Nebix's better
/// guess when it is running.
fn empty_line_hint(shell: &Shell, last: &str, status: i32, folder: &str) -> (String, String) {
    let settings = &shell.settings;
    if last.is_empty() || settings.screen_reader {
        return (String::new(), String::new());
    }
    if status != 0 && status != 130 {
        // The command fixed (ADR 0275), and the offer of Alt+E.
        let offer = if settings.explain_offer { "Alt+E explains what went wrong" } else { "" };
        return match crate::fix::fix(shell, last, status) {
            Some(fixed) if offer.is_empty() => (fixed.clone(), fixed),
            Some(fixed) => (format!("{fixed}   · {offer}"), fixed),
            None => (offer.to_string(), String::new()),
        };
    }
    if !settings.suggest_next {
        return (String::new(), String::new());
    }
    let guess = shell.records.as_ref().and_then(|r| r.next_after(last, folder)).map(str::to_string);
    let better = crate::nebix::next(&crate::explain::mask(last), status, folder, guess.as_deref());
    match better.or(guess) {
        Some(command) => (command.clone(), command),
        None => (String::new(), String::new()),
    }
}

/// The command and the word being typed at the end of `line`: the
/// program named first in the last simple command, past assignments and
/// `sudo` or `elevate`.
pub fn command_and_word(line: &str) -> Option<(&str, &str)> {
    let last = line.rsplit(['|', ';', '&', '(', '\n', '`']).next()?;
    if last.ends_with(char::is_whitespace) {
        return None;
    }
    let mut words = last.split_whitespace();
    let word = last.split_whitespace().last()?;
    let command = words.find(|w| {
        !matches!(*w, "sudo" | "elevate" | "doas" | "time" | "command" | "exec" | "!")
            && !w.split_once('=').is_some_and(|(name, _)| crate::parse::is_name(name))
    })?;
    // The word must come after the command.
    if std::ptr::eq(command, word) {
        return None;
    }
    Some((command.rsplit('/').next().unwrap_or(command), word))
}

/// What the option being typed at the end of `line` does: the rest of
/// its name, and its description, when the completion file of its
/// program has exactly one option that starts with it, or one that is it.
fn option_help(shell: &mut Shell, line: &str) -> Option<(String, String)> {
    let (command, word) = command_and_word(line)?;
    if !word.starts_with('-') || word == "-" || word == "--" || word.contains('=') {
        return None;
    }
    let file = shell.completion_file(command)?;
    let mut exact = None;
    let mut starting = Vec::new();
    for option in &file.options {
        for form in &option.forms {
            if form == word {
                exact = Some(option);
            } else if form.starts_with(word) {
                starting.push((form, option));
            }
        }
    }
    if let Some(option) = exact {
        return Some((String::new(), option.description.clone())).filter(|(_, d)| !d.is_empty());
    }
    // Several forms of one option (`--colour`, `--color`) are one answer.
    let first = starting.first()?.1;
    if starting.iter().any(|(_, o)| !std::ptr::eq(*o, first)) {
        return None;
    }
    let form = starting.iter().map(|(f, _)| *f).min_by_key(|f| f.len())?;
    Some((form[word.len()..].to_string(), first.description.clone()))
}

/// Ctrl+R's picker: the command record, as suggestions.
pub struct Picker(pub Handle);

impl Completer for Picker {
    fn complete(&mut self, line: &str, _pos: usize) -> CompletionResult {
        let Ok(mut shell) = self.0.0.try_borrow_mut() else {
            return CompletionResult::fresh(Vec::new());
        };
        let folder = shell.get("PWD").unwrap_or_default();
        let now = crate::sys::now().0;
        let Some(records) = shell.records.as_mut() else {
            return CompletionResult::fresh(Vec::new());
        };
        records.refresh();
        let found: Vec<Suggestion> = records
            .search(line, &folder, now, 200)
            .into_iter()
            .map(|record| Suggestion {
                value: record.command.clone(),
                display_override: Some(record.command.replace('\n', " ⏎ ")),
                description: Some(crate::record::describe(record, now)),
                span: Span::new(0, line.len()),
                append_whitespace: false,
                ..Suggestion::default()
            })
            .collect();
        CompletionResult::fresh(found)
    }
}

/// The right prompt (ADR 0262): how long the last command took, when it
/// took long enough to say; the jobs; the folder's git branch.
pub fn right_prompt(shell: &Shell, paint: bool) -> String {
    let palette = shell.colors.clone();
    let style = |key: &str, text: &str| {
        if paint { palette.style(&format!("prompt.{key}")).paint(text) } else { text.to_string() }
    };
    let mut parts = Vec::new();
    let after = palette.number("prompt.took-after").saturating_mul(1000);
    if shell.last_millis >= after.max(1) {
        parts.push(style("took", &format!("took {}", crate::record::duration(shell.last_millis))));
    }
    let jobs = shell.jobs.list.iter().filter(|j| !j.done()).count();
    if jobs > 0 {
        let word = if jobs == 1 { "job" } else { "jobs" };
        parts.push(style("jobs", &format!("{jobs} {word}")));
    }
    let here = shell.get("PWD").unwrap_or_default();
    if let Some(branch) = git_branch(&here) {
        let dirty = shell.settings.git_dirty
            && shell.search_path("git", false).is_some()
            && git_dirty(&here);
        let mark = if dirty { "*" } else { "" };
        parts.push(style("branch", &format!("git {branch}{mark}")));
    }
    parts.join("  ")
}

/// The git branch of the folder `dir` is in: `.git/HEAD` in it or the
/// nearest folder above; a detached head is its commit, shortened.
pub fn git_branch(dir: &str) -> Option<String> {
    let mut dir = std::path::PathBuf::from(dir);
    loop {
        let git = dir.join(".git");
        if let Ok(meta) = std::fs::metadata(&git) {
            let head = if meta.is_dir() {
                git.join("HEAD")
            } else {
                // A worktree's .git is a file: `gitdir: path`.
                let text = std::fs::read_to_string(&git).ok()?;
                let target = text.trim().strip_prefix("gitdir:")?.trim();
                dir.join(target).join("HEAD")
            };
            let text = std::fs::read_to_string(head).ok()?;
            return branch_of(&text);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Whether the work tree at `dir` has changes not committed, by `git
/// status` with untracked files left out; no answer within 100 ms is
/// taken as clean, so that a big repository never holds the prompt.
fn git_dirty(dir: &str) -> bool {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let child = Command::new("git")
        .args(["--no-optional-locks", "status", "--porcelain", "-uno", "--ignore-submodules"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return false,
            Ok(None) if started.elapsed() > std::time::Duration::from_millis(100) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(1)),
        }
    }
    let mut out = [0u8; 1];
    child.stdout.take().is_some_and(|mut o| o.read(&mut out).is_ok_and(|n| n > 0))
}

fn branch_of(head: &str) -> Option<String> {
    let head = head.trim();
    match head.strip_prefix("ref:") {
        Some(name) => {
            let name = name.trim();
            Some(name.strip_prefix("refs/heads/").unwrap_or(name).to_string())
        }
        None if head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()) => {
            Some(head[..7].to_string())
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_and_its_last_word() {
        assert_eq!(command_and_word("ls -l"), Some(("ls", "-l")));
        assert_eq!(command_and_word("cat x | sort -r"), Some(("sort", "-r")));
        assert_eq!(command_and_word("LC_ALL=C sudo /usr/bin/grep -i"), Some(("grep", "-i")));
        assert_eq!(command_and_word("ls -l "), None);
        assert_eq!(command_and_word("ls"), None);
    }

    #[test]
    fn options_are_explained_from_completion_files() {
        let dir = std::env::temp_dir().join(format!("nukesh-assist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tool"),
            "option\t-l --long\t\tuse a long listing format\noption\t--color --colour\tvalue\tcolour the output\noption\t--count\t\tcount\n",
        )
        .unwrap();
        let mut shell = Shell::new("test", false);
        shell.set_var("NUKESH_COMPLETION_PATH", dir.to_str().unwrap());
        let help = |shell: &mut Shell, line: &str| option_help(shell, line);
        assert_eq!(
            help(&mut shell, "tool -l"),
            Some((String::new(), "use a long listing format".into()))
        );
        assert_eq!(
            help(&mut shell, "tool --lo"),
            Some(("ng".into(), "use a long listing format".into()))
        );
        assert_eq!(help(&mut shell, "tool --colo"), Some(("r".into(), "colour the output".into())));
        // --co could be --color or --count.
        assert_eq!(help(&mut shell, "tool --co"), None);
        assert_eq!(help(&mut shell, "tool --x"), None);
        assert_eq!(help(&mut shell, "other -l"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn heads_name_their_branch() {
        assert_eq!(branch_of("ref: refs/heads/main\n").as_deref(), Some("main"));
        assert_eq!(branch_of("ref: refs/heads/feature/x").as_deref(), Some("feature/x"));
        assert_eq!(
            branch_of("8f5747610fc618b79e7901b5f1b36a1f95968c44").as_deref(),
            Some("8f57476")
        );
        assert_eq!(branch_of("junk"), None);
    }
}
