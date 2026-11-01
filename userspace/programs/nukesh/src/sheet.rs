// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The commit sheet (ADR 0288): Alt+C in a git repository shows, under
//! the line, the files staged for the next commit, a choice of prefix
//! (`feat:`, `fix:` and the rest of Conventional Commits, or none), and
//! the subject as it is typed with a count of its letters, which turns
//! yellow past 50 and red past 72, where git's tools cut it. Alt+N asks
//! Nebix, on this computer, for a draft from the staged changes. Enter
//! puts the `git commit -m` command it makes on the line, to run or not:
//! the sheet never commits by itself.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::git::{self, Repo};

/// The prefixes offered, the first meaning none.
pub const KINDS: &[(&str, &str)] = &[
    ("", "none"),
    ("feat", "a new feature"),
    ("fix", "a bug fixed"),
    ("docs", "documentation only"),
    ("refactor", "code changed, behaviour not"),
    ("test", "tests only"),
    ("chore", "upkeep: builds, tools, versions"),
];

/// Past this many letters a subject is long; past `CUT`, tools cut it.
pub const LONG: usize = 50;
pub const CUT: usize = 72;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sheet {
    pub branch: String,
    /// The staged files and their state.
    pub staged: Vec<(String, String)>,
    /// How many files are changed and not staged.
    pub unstaged: usize,
    /// Whether the commit takes every changed file (`-a`).
    pub all: bool,
    pub kind: usize,
    pub subject: String,
    /// Where the cursor is in the subject, in characters.
    pub at: usize,
    /// A line of help or a problem, under the subject.
    pub note: Option<String>,
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    More,
    /// The command to put on the line.
    Done(String),
    Cancel,
    /// Ask Nebix for a draft.
    Draft,
}

impl Sheet {
    pub fn new(repo: &Repo) -> Sheet {
        let status = git::run_status(&repo.work).unwrap_or_default();
        let branch = match git::head(repo) {
            Some(git::Head::Branch(name)) => name,
            Some(git::Head::Detached { short, .. }) => format!("{short} (detached HEAD)"),
            None => "?".into(),
        };
        let staged = status
            .files
            .iter()
            .filter(|(_, xy)| xy != "??" && !xy.starts_with(' ') && !xy.contains('U'))
            .map(|(path, xy)| (path.clone(), xy.chars().next().unwrap_or(' ').to_string()))
            .collect();
        let unstaged = status
            .files
            .iter()
            .filter(|(_, xy)| xy.chars().nth(1).is_some_and(|y| y != ' ' && y != '?'))
            .count();
        Sheet {
            branch,
            staged,
            unstaged,
            all: false,
            kind: 0,
            subject: String::new(),
            at: 0,
            note: None,
        }
    }

    /// The message: the prefix, if one is chosen, and the subject.
    pub fn message(&self) -> String {
        let subject = self.subject.trim();
        match KINDS[self.kind].0 {
            "" => subject.to_string(),
            kind => format!("{kind}: {subject}"),
        }
    }

    /// The command Enter puts on the line.
    pub fn command(&self) -> String {
        let all = if self.all { "-a " } else { "" };
        format!("git commit {all}-m {}", crate::expand::quote_always(&self.message()))
    }

    pub fn key(&mut self, key: KeyEvent) -> Step {
        self.note = None;
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.subject.chars().count();
        match key.code {
            KeyCode::Esc => return Step::Cancel,
            KeyCode::Char('c' | 'g') if ctrl => return Step::Cancel,
            KeyCode::Enter => {
                if self.subject.trim().is_empty() {
                    self.note = Some("Write a subject first, or Esc to leave".into());
                } else if self.staged.is_empty() && !self.all {
                    self.note = Some(if self.unstaged > 0 {
                        "Nothing is staged: Alt+A commits every changed file".into()
                    } else {
                        "Nothing is staged, and nothing has changed".into()
                    });
                } else {
                    return Step::Done(self.command());
                }
            }
            KeyCode::Tab => self.kind = (self.kind + 1) % KINDS.len(),
            KeyCode::BackTab => self.kind = (self.kind + KINDS.len() - 1) % KINDS.len(),
            KeyCode::Char('a') if alt => {
                self.all = !self.all && self.unstaged > 0;
            }
            KeyCode::Char('n') if alt => return Step::Draft,
            KeyCode::Char('a') if ctrl => self.at = 0,
            KeyCode::Char('e') if ctrl => self.at = len,
            KeyCode::Char('u') if ctrl => {
                self.subject = self.subject.chars().skip(self.at).collect();
                self.at = 0;
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                let byte = self.byte_at(self.at);
                self.subject.insert(byte, c);
                self.at += 1;
            }
            KeyCode::Backspace if self.at > 0 => {
                let byte = self.byte_at(self.at - 1);
                self.subject.remove(byte);
                self.at -= 1;
            }
            KeyCode::Delete if self.at < len => {
                let byte = self.byte_at(self.at);
                self.subject.remove(byte);
            }
            KeyCode::Left => self.at = self.at.saturating_sub(1),
            KeyCode::Right => self.at = (self.at + 1).min(len),
            KeyCode::Home => self.at = 0,
            KeyCode::End => self.at = len,
            _ => {}
        }
        Step::More
    }

    fn byte_at(&self, at: usize) -> usize {
        self.subject.char_indices().nth(at).map_or(self.subject.len(), |(i, _)| i)
    }

    /// Takes a draft as the subject, with its prefix if it has one of
    /// those offered.
    pub fn take_draft(&mut self, draft: &str) {
        let mut text = draft.trim().trim_matches('"').to_string();
        if let Some((kind, rest)) = text.split_once(": ")
            && let Some(i) = KINDS.iter().position(|(k, _)| *k == kind && !k.is_empty())
        {
            self.kind = i;
            text = rest.to_string();
        }
        self.subject = text;
        self.at = self.subject.chars().count();
    }

    /// The sheet's lines, each painted with `paint(key, text)`, and the
    /// column the cursor goes to on the subject's line (its last but
    /// one, or last but two with a note).
    pub fn lines(&self, paint: &dyn Fn(&str, &str) -> String) -> (Vec<String>, usize) {
        let mut lines = Vec::new();
        let count = self.staged.len();
        let files =
            if count == 1 { "1 file staged".to_string() } else { format!("{count} files staged") };
        let all = if self.all {
            format!(", and every changed file with -a ({} more)", self.unstaged)
        } else {
            String::new()
        };
        lines.push(paint("header", &format!("Commit to {} · {files}{all}", self.branch)));
        for (path, state) in self.staged.iter().take(6) {
            let how = match state.as_str() {
                "A" => "new",
                "D" => "deleted",
                "R" => "renamed",
                _ => "changed",
            };
            lines.push(format!("  {}  {path}", paint("state", &format!("{how:<8}"))));
        }
        if count > 6 {
            lines.push(format!("  and {} more", count - 6));
        }
        if count == 0 && !self.all {
            lines.push(paint(
                "hint",
                "  nothing staged yet: git add, or Alt+A for every changed file",
            ));
        }
        let kinds: Vec<String> = KINDS
            .iter()
            .enumerate()
            .map(|(i, (k, _))| {
                let name = if k.is_empty() { "none" } else { k };
                if i == self.kind {
                    paint("chosen", &format!("[{name}]"))
                } else {
                    name.to_string()
                }
            })
            .collect();
        lines.push(format!(
            "Type: {}   {}",
            kinds.join(" "),
            paint("hint", &format!("(Tab: {})", KINDS[self.kind].1))
        ));
        let letters = self.message().chars().count();
        let counter = format!("{letters}/{LONG}");
        let counter = match letters {
            n if n > CUT => paint("cut", &counter),
            n if n > LONG => paint("long", &counter),
            _ => paint("hint", &counter),
        };
        let prefix = match KINDS[self.kind].0 {
            "" => String::new(),
            kind => format!("{kind}: "),
        };
        let lead = format!("Subject {counter}: {prefix}");
        let shown_lead = format!("Subject {letters}/{LONG}: {prefix}");
        let column = shown_lead.chars().count() + self.at;
        lines.push(format!("{lead}{}", self.subject));
        if let Some(note) = &self.note {
            lines.push(paint("long", note));
        }
        lines.push(paint(
            "hint",
            "Enter puts the git commit on the line · Alt+N: a draft from Nebix · Alt+A: -a · Esc leaves",
        ));
        (lines, column)
    }
}

/// Shows the sheet under the line and returns the command it made, or
/// nothing if it was left with Esc.
pub fn run(shell: &crate::shell::Shell) -> Option<String> {
    use std::io::Write as _;
    let here = std::path::PathBuf::from(shell.get("PWD").unwrap_or_default());
    let out = |text: &str| {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    };
    let Some(repo) = git::find(&here) else {
        out("\r\nAlt+C makes a commit in a git repository; this folder is not in one.\r\n");
        return None;
    };
    let paint_on = crate::editor::colors(shell);
    let palette = shell.colors.clone();
    let paint = move |key: &str, text: &str| -> String {
        if !paint_on {
            return text.to_string();
        }
        let style = match key {
            "header" => "prompt.path",
            "state" => "prompt.git-staged",
            "chosen" => "prompt.git-state",
            "long" => "prompt.took",
            "cut" => "prompt.git-conflict",
            _ => "line.hint",
        };
        palette.style(style).paint(text)
    };
    let mut sheet = Sheet::new(&repo);
    if sheet.staged.is_empty() && sheet.unstaged > 0 {
        sheet.all = false;
    }
    let raw = crossterm::terminal::enable_raw_mode().is_ok();
    out("\r\n");
    let mut drawn = 0usize;
    let result = loop {
        let (lines, column) = sheet.lines(&paint);
        // Back to the sheet's top, cleared, and drawn again.
        let mut frame = String::new();
        if drawn > 1 {
            frame.push_str(&format!("\x1b[{}A", drawn - 1));
        }
        frame.push_str("\r\x1b[J");
        frame.push_str(&lines.join("\r\n"));
        // The cursor on the subject's line.
        let subject =
            lines.iter().position(|l| l.starts_with("Subject")).unwrap_or(lines.len() - 1);
        let below = lines.len() - 1 - subject;
        if below > 0 {
            frame.push_str(&format!("\x1b[{below}A"));
        }
        frame.push_str(&format!("\r\x1b[{}C", column.max(1)));
        out(&frame);
        // Where the cursor is, counted from the sheet's top, for the
        // next draw.
        drawn = lines.len() - below;
        let event = crossterm::event::read();
        let Ok(Event::Key(key)) = event else {
            if event.is_err() {
                break None;
            }
            continue;
        };
        if key.kind == crossterm::event::KeyEventKind::Release {
            continue;
        }
        match sheet.key(key) {
            Step::More => {}
            Step::Cancel => break None,
            Step::Done(command) => break Some(command),
            Step::Draft => {
                let diff =
                    git::output(&repo.work, &["diff", "--cached", "--stat", "-p", "--no-color"])
                        .unwrap_or_default();
                match crate::nebix::commit_subject(&diff) {
                    Some(draft) => sheet.take_draft(&draft),
                    None => {
                        sheet.note =
                            Some("Nebix is not running on this computer, so no draft".into());
                    }
                }
            }
        }
    };
    // The sheet cleared away; the line is drawn again under where it was.
    let up = drawn.saturating_sub(1);
    let mut clear = String::new();
    if up > 0 {
        clear.push_str(&format!("\x1b[{up}A"));
    }
    clear.push_str("\r\x1b[J");
    out(&clear);
    if raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(sheet: &mut Sheet, code: KeyCode) -> Step {
        sheet.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn sheet() -> Sheet {
        Sheet {
            branch: "main".into(),
            staged: vec![("src/login.rs".into(), "M".into())],
            unstaged: 2,
            all: false,
            kind: 0,
            subject: String::new(),
            at: 0,
            note: None,
        }
    }

    #[test]
    fn the_sheet_makes_a_commit_command() {
        let mut s = sheet();
        assert_eq!(press(&mut s, KeyCode::Enter), Step::More);
        assert!(s.note.as_deref().unwrap().contains("subject"));
        for c in "Add the sign-in form".chars() {
            press(&mut s, KeyCode::Char(c));
        }
        press(&mut s, KeyCode::Tab);
        assert_eq!(s.message(), "feat: Add the sign-in form");
        assert_eq!(
            press(&mut s, KeyCode::Enter),
            Step::Done("git commit -m 'feat: Add the sign-in form'".into())
        );
        s.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
        assert!(s.command().starts_with("git commit -a -m "));
        assert_eq!(press(&mut s, KeyCode::Esc), Step::Cancel);
    }

    #[test]
    fn nothing_staged_asks_for_alt_a() {
        let mut s = sheet();
        s.staged.clear();
        s.subject = "Fix it".into();
        assert_eq!(press(&mut s, KeyCode::Enter), Step::More);
        assert!(s.note.as_deref().unwrap().contains("Alt+A"));
    }

    #[test]
    fn a_draft_keeps_its_prefix() {
        let mut s = sheet();
        s.take_draft("fix: Stop the footer jumping on phones");
        assert_eq!(KINDS[s.kind].0, "fix");
        assert_eq!(s.subject, "Stop the footer jumping on phones");
        let plain = |_: &str, t: &str| t.to_string();
        let (lines, column) = s.lines(&plain);
        assert_eq!(lines[0], "Commit to main · 1 file staged");
        assert!(lines.iter().any(|l| l == "Subject 38/50: fix: Stop the footer jumping on phones"));
        assert_eq!(column, "Subject 38/50: fix: ".len() + 33);
    }
}
