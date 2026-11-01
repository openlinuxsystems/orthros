// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The interactive shell's settings that are not colours, from
//! settings.toml(5) (ADRs 0263 and 0264): the system's file,
//! `/system/settings/nukesh/settings.toml`, then a person's own,
//! `~/.config/nukesh/settings.toml`, key by key.
//!
//! `[files] show-hidden` shows hidden files everywhere the shell lists
//! them: `ls` is `ls -A`, `*` matches names that start with a dot
//! (`shopt -s dotglob`), and Tab completes those names and the ones a
//! folder's `.hidden` file lists. The startup files run after, so they
//! can still change any of the three.
//!
//! `[notify]` says whether a long command that ends while you are on
//! another console tells you, and after how many seconds (notify.rs).

use crate::colors::Problem;

/// The system's settings file.
pub const SYSTEM_FILE: &str = "/system/settings/nukesh/settings.toml";

/// A person's own, under their home.
const SECTIONS: &[&str] =
    &["files", "prompt", "notify", "nebix", "suggest", "accessibility", "terminal", "history", "commands", "folders"];

pub const USER_FILE: &str = ".config/nukesh/settings.toml";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub show_hidden: bool,
    pub git_dirty: bool,
    pub notify: bool,
    pub notify_after: u64,
    /// `[nebix] offer`: after a command fails, the empty line offers
    /// Alt+E (ADR 0268).
    pub explain_offer: bool,
    /// `[suggest] next`: on an empty line, the command you usually run
    /// next, faded (ADR 0270).
    pub suggest_next: bool,
    /// `[accessibility] screen-reader`: plain output for screen readers
    /// (ADR 0269).
    pub screen_reader: bool,
    /// `[terminal] marks`: OSC 133 marks around prompts and output.
    pub marks: bool,
    /// `[terminal] links`: OSC 8 links on paths in messages.
    pub links: bool,
    /// `[history] skip-secrets`: commands holding a secret are not kept.
    pub skip_secrets: bool,
    /// `[commands] offer-install`: a missing command names its package.
    pub offer_install: bool,
    /// `[folders] project-files`: a trusted folder's `.nukesh.toml` applies.
    pub project_files: bool,
    /// `[folders] resume`: a new shell on a console starts where the last
    /// one there left off.
    pub resume: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            show_hidden: false,
            git_dirty: true,
            notify: true,
            notify_after: 30,
            explain_offer: true,
            suggest_next: true,
            screen_reader: false,
            marks: true,
            links: true,
            skip_secrets: true,
            offer_install: true,
            project_files: true,
            resume: true,
        }
    }
}

impl Settings {
    /// The system's file, then the person's, over the defaults.
    pub fn load(home: Option<&str>) -> (Settings, Vec<Problem>) {
        let mut settings = Settings::default();
        let mut problems = Vec::new();
        let mut files = vec![SYSTEM_FILE.to_string()];
        if let Some(home) = home.filter(|h| !h.is_empty()) {
            files.push(format!("{}/{USER_FILE}", home.trim_end_matches('/')));
        }
        for file in files {
            if let Ok(text) = std::fs::read_to_string(&file) {
                for message in settings.apply(&text) {
                    problems.push(Problem { file: file.clone(), message });
                }
            }
        }
        (settings, problems)
    }

    /// Takes the keys `text` sets; returns what was wrong in it.
    /// The on-off key `[section] key`.
    fn switch(&mut self, section: &str, key: &str) -> Option<&mut bool> {
        Some(match (section, key) {
            ("files", "show-hidden") => &mut self.show_hidden,
            ("prompt", "git-dirty") => &mut self.git_dirty,
            ("notify", "enabled") => &mut self.notify,
            ("nebix", "offer") => &mut self.explain_offer,
            ("suggest", "next") => &mut self.suggest_next,
            ("accessibility", "screen-reader") => &mut self.screen_reader,
            ("terminal", "marks") => &mut self.marks,
            ("terminal", "links") => &mut self.links,
            ("history", "skip-secrets") => &mut self.skip_secrets,
            ("commands", "offer-install") => &mut self.offer_install,
            ("folders", "project-files") => &mut self.project_files,
            ("folders", "resume") => &mut self.resume,
            _ => return None,
        })
    }

    pub fn apply(&mut self, text: &str) -> Vec<String> {
        let table: toml::Table = match text.parse() {
            Ok(table) => table,
            Err(err) => return vec![err.to_string().trim().replace('\n', " ")],
        };
        let mut problems = Vec::new();
        for (section, value) in &table {
            let Some(keys) = value.as_table() else {
                problems.push(format!("{section}: no such key"));
                continue;
            };
            for (key, value) in keys {
                match (section.as_str(), key.as_str()) {
                    ("notify", "after") => match value.as_integer().and_then(|n| u64::try_from(n).ok()) {
                        Some(n) => self.notify_after = n,
                        None => problems.push(format!("[notify] {key}: a number of seconds")),
                    },
                    (section, key) if self.switch(section, key).is_some() => match value.as_bool() {
                        Some(on) => *self.switch(section, key).unwrap_or_else(|| unreachable!()) = on,
                        None => problems.push(format!("[{section}] {key}: true or false")),
                    },
                    (section, _) if SECTIONS.contains(&section) => problems.push(format!("[{section}] {key}: no such key")),
                    _ => {
                        problems.push(format!("[{section}]: no such section"));
                        break;
                    }
                }
            }
        }
        problems
    }
}

/// At the start of an interactive shell, before its startup files:
/// `show-hidden` turns on `dotglob` and adds `-A` to `ls`, and `links`
/// adds `--hyperlink=auto`.
pub fn apply_to(shell: &mut crate::shell::Shell) {
    let mut ls = shell.aliases.get("ls").cloned().unwrap_or_else(|| "ls".into());
    let had = ls.clone();
    if shell.settings.show_hidden {
        shell.shopts.insert("dotglob", true);
        if !ls.split_whitespace().any(|w| w == "-A" || w == "-a") {
            ls.push_str(" -A");
        }
    }
    // `[terminal] links`: ls names files as links a terminal can open
    // (OSC 8, ADR 0272); `auto` leaves pipes and files plain.
    if shell.settings.links && !ls.contains("--hyperlink") {
        ls.push_str(" --hyperlink=auto");
    }
    if ls != had {
        shell.aliases.insert("ls".into(), ls);
    }
}

#[cfg(test)]
const SHIPPED: &str = include_str!("../../../defaults/nukesh/settings.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_file_says_the_defaults() {
        let mut settings = Settings::default();
        assert!(settings.apply(SHIPPED).is_empty());
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn keys_are_checked() {
        let mut settings = Settings::default();
        let problems = settings.apply("[files]\nshow-hidden = true\n[notify]\nafter = \"x\"\nloud = 1\n[nope]\na = 1\n");
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(settings.show_hidden);
        assert_eq!(settings.notify_after, 30);
    }

    #[test]
    fn show_hidden_shows_them() {
        let mut shell = crate::shell::Shell::new("test", false);
        shell.aliases.insert("ls".into(), "ls --color=auto".into());
        shell.settings = std::rc::Rc::new(Settings { show_hidden: true, links: false, ..Settings::default() });
        apply_to(&mut shell);
        assert!(shell.shopt("dotglob"));
        assert_eq!(shell.aliases.get("ls").map(String::as_str), Some("ls --color=auto -A"));
        shell.settings = std::rc::Rc::new(Settings::default());
        apply_to(&mut shell);
        assert_eq!(shell.aliases.get("ls").map(String::as_str), Some("ls --color=auto -A --hyperlink=auto"));
    }
}
