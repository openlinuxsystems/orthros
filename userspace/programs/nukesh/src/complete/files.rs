// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Completion files (nukesh(1), COMPLETION FILES): a program's options,
//! with what each one's argument is and what it does, and what its
//! operands are, for the commands that `complete` says nothing for. The
//! system's are written from each program's manual page when the system
//! is built (ADR 0102); a user's own, in the same form, come first.

use std::rc::Rc;

use super::{Candidate, action_words, files};
use crate::shell::Shell;

/// Where the system's completion files are.
const SYSTEM_DIR: &str = "/system/resources/nukesh/completions";

/// One option of a completion file.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FileOption {
    /// Its forms: `-a`, `--all`.
    pub forms: Vec<String>,
    /// What its argument completes as, a `complete -A` action or `value`
    /// for anything, if it takes one.
    pub argument: Option<String>,
    /// Whether the argument is joined to the option, as in 7-Zip's
    /// `-o{directory}`, and not the next word.
    pub glued: bool,
    pub description: String,
}

/// What an operand completes as.
#[derive(Debug, PartialEq, Eq)]
pub enum Operand {
    /// A `complete -A` action; `none` or `value` for nothing.
    Action(String),
    Words(Vec<String>),
}

/// A program's completion file.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CompletionFile {
    pub options: Vec<FileOption>,
    /// The operands, by position from 1, and 0 for every one not named.
    pub operands: Vec<(usize, Operand)>,
}

impl CompletionFile {
    /// Reads a completion file, leaving out lines it does not know, such
    /// as those of a later version of the form.
    pub fn parse(text: &str) -> Self {
        let mut file = Self::default();
        for line in text.lines() {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                ["option", forms, argument, rest @ ..] => {
                    let (glued, argument) = match argument.strip_prefix('+') {
                        Some(kind) => (true, kind),
                        None => (false, *argument),
                    };
                    let forms: Vec<String> = forms.split(' ').map(str::to_string).collect();
                    if forms.iter().any(|f| !f.starts_with(['-', '+']) || f.len() < 2) {
                        continue;
                    }
                    file.options.push(FileOption {
                        forms,
                        argument: (!argument.is_empty()).then(|| argument.to_string()),
                        glued,
                        description: rest.first().copied().unwrap_or_default().to_string(),
                    });
                }
                ["operand", position, what] => {
                    let Ok(position) = position.parse() else { continue };
                    let operand = match what.strip_prefix("words ") {
                        Some(words) => {
                            Operand::Words(words.split(' ').map(str::to_string).collect())
                        }
                        None => Operand::Action(what.to_string()),
                    };
                    file.operands.push((position, operand));
                }
                _ => {}
            }
        }
        file
    }

    /// The option that `word` is one of the forms of.
    fn option(&self, word: &str) -> Option<&FileOption> {
        self.options.iter().find(|o| o.forms.iter().any(|f| f == word))
    }

    /// What the operand at `position` (from 1) completes as: files when
    /// the file does not say.
    fn operand(&self, position: usize) -> Operand {
        let named = self.operands.iter().find(|(p, _)| *p == position);
        match named.or_else(|| self.operands.iter().find(|(p, _)| *p == 0)) {
            Some((_, Operand::Action(action))) => Operand::Action(action.clone()),
            Some((_, Operand::Words(words))) => Operand::Words(words.clone()),
            None => Operand::Action("file".into()),
        }
    }
}

/// What a completion file gives for the word at the cursor.
pub enum FromFile {
    /// Options, ready to show.
    Options(Vec<Candidate>),
    /// Words for an argument or operand, quoted as the shell's own are,
    /// each after `prefix` (as `--file=`), and whether they are files.
    Words { list: Vec<String>, is_files: bool, prefix: String, word: String },
}

impl Shell {
    /// The completion file for the program `name`: the first found in
    /// the directories of `NUKESH_COMPLETION_PATH`, or else the user's
    /// (`$XDG_DATA_HOME/nukesh/completions`, by default
    /// `~/.local/share/nukesh/completions`) and the system's. Each is
    /// read once.
    pub fn completion_file(&mut self, name: &str) -> Option<Rc<CompletionFile>> {
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return None;
        }
        if let Some(known) = self.completions.files.get(name) {
            return known.clone();
        }
        let dirs: Vec<String> = match self.get("NUKESH_COMPLETION_PATH") {
            Some(path) => path.split(':').filter(|d| !d.is_empty()).map(str::to_string).collect(),
            None => {
                let data = self
                    .get("XDG_DATA_HOME")
                    .filter(|d| !d.is_empty())
                    .or_else(|| self.get("HOME").map(|home| format!("{home}/.local/share")));
                data.map(|d| format!("{d}/nukesh/completions"))
                    .into_iter()
                    .chain([SYSTEM_DIR.to_string()])
                    .collect()
            }
        };
        let found = dirs
            .iter()
            .find_map(|dir| std::fs::read_to_string(format!("{dir}/{name}")).ok())
            .map(|text| Rc::new(CompletionFile::parse(&text)));
        self.completions.files.insert(name.to_string(), found.clone());
        found
    }

    /// Completes `words[index]` from `file`: an option, the argument of
    /// the option before it, or an operand.
    pub fn complete_from_file(
        &mut self,
        file: &CompletionFile,
        words: &[String],
        index: usize,
    ) -> FromFile {
        let word = words[index].as_str();
        // Which operand this is, and whether the word before it was an
        // option whose argument it is; after `--`, only operands.
        let mut operand = 1;
        let mut wants: Option<&FileOption> = None;
        let mut options_end = false;
        for before in &words[1..index] {
            if wants.take().is_some() {
                continue;
            }
            if options_end || before == "-" || !before.starts_with(['-', '+']) {
                operand += 1;
            } else if before == "--" {
                options_end = true;
            } else if let Some(option) = file.option(before)
                && option.argument.is_some()
                && !option.glued
            {
                wants = Some(option);
            }
        }
        if let Some(option) = wants {
            return self.argument(option.argument.as_deref().unwrap_or("value"), word, "");
        }
        if !options_end && word.starts_with(['-', '+']) {
            // `--name=value`: the argument after the `=`.
            if let Some((name, value)) = word.split_once('=')
                && name.starts_with("--")
            {
                return match file.option(name).and_then(|o| o.argument.as_deref()) {
                    Some(kind) => self.argument(kind, value, &format!("{name}=")),
                    None => FromFile::Options(Vec::new()),
                };
            }
            // `-o{directory}`: the argument joined to its option.
            for option in file.options.iter().filter(|o| o.glued) {
                for form in &option.forms {
                    if let Some(value) = word.strip_prefix(form.as_str())
                        && !value.is_empty()
                    {
                        let kind = option.argument.as_deref().unwrap_or("value");
                        return self.argument(kind, value, form);
                    }
                }
            }
            let mut candidates = Vec::new();
            for option in &file.options {
                for form in option.forms.iter().filter(|f| f.starts_with(word)) {
                    // A long option's argument goes after `=`.
                    let joined =
                        option.glued || (form.starts_with("--") && option.argument.is_some());
                    let text =
                        if joined && !option.glued { format!("{form}=") } else { form.clone() };
                    candidates.push(Candidate {
                        display: text.clone(),
                        text,
                        space: !joined,
                        description: (!option.description.is_empty())
                            .then(|| option.description.clone()),
                    });
                }
            }
            candidates.sort_by(|a, b| a.text.cmp(&b.text));
            candidates.dedup_by(|a, b| a.text == b.text);
            if !candidates.is_empty() || word.starts_with('-') {
                return FromFile::Options(candidates);
            }
        }
        match file.operand(operand) {
            Operand::Action(action) => self.argument(&action, word, ""),
            Operand::Words(list) => FromFile::Words {
                list: list.into_iter().filter(|w| w.starts_with(word)).collect(),
                is_files: false,
                prefix: String::new(),
                word: word.to_string(),
            },
        }
    }

    /// The words an argument of `kind` (an action, or `value` or `none`
    /// for nothing) can be, for `word`.
    fn argument(&mut self, kind: &str, word: &str, prefix: &str) -> FromFile {
        let (list, is_files) = match kind {
            "file" => (files(self, word, false), true),
            "directory" => (files(self, word, true), true),
            "value" | "none" => (Vec::new(), false),
            action => (action_words(self, action, word), action == "command"),
        };
        FromFile::Words { list, is_files, prefix: prefix.to_string(), word: word.to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GREP: &str = "\
# What nukesh(1) completes for grep, from grep(1).
option\t-e --regexp\tvalue\tUse PATTERNS as the patterns
option\t-f --file\tfile\tObtain patterns from FILE, one per line
option\t-i --ignore-case\t\tIgnore case distinctions in patterns and data
option\t-o\t+directory\tExtract into directory
option\tnot-an-option\t\tLeft out
something new\tthat this version does not know
operand\t1\tvalue
";

    fn texts(found: FromFile) -> Vec<String> {
        match found {
            FromFile::Options(candidates) => candidates.into_iter().map(|c| c.text).collect(),
            FromFile::Words { list, prefix, .. } => {
                list.into_iter().map(|w| format!("{prefix}{w}")).collect()
            }
        }
    }

    fn complete(shell: &mut Shell, line: &[&str]) -> Vec<String> {
        let file = CompletionFile::parse(GREP);
        let words: Vec<String> = line.iter().map(ToString::to_string).collect();
        texts(shell.complete_from_file(&file, &words, words.len() - 1))
    }

    #[test]
    fn reads_completion_files() {
        let file = CompletionFile::parse(GREP);
        assert_eq!(file.options.len(), 4);
        assert_eq!(file.options[1].forms, ["-f", "--file"]);
        assert_eq!(file.options[1].argument.as_deref(), Some("file"));
        assert_eq!(file.options[2].argument, None);
        assert!(file.options[3].glued);
        assert_eq!(file.operand(1), Operand::Action("value".into()));
        assert_eq!(file.operand(2), Operand::Action("file".into()));
    }

    #[test]
    fn completes_options_and_their_arguments() {
        let mut shell = Shell::new("test", false);
        assert_eq!(
            complete(&mut shell, &["grep", "--"]),
            ["--file=", "--ignore-case", "--regexp="]
        );
        assert_eq!(complete(&mut shell, &["grep", "-"]).len(), 7);
        let file = CompletionFile::parse(GREP);
        let words: Vec<String> = ["grep", "--ig"].iter().map(ToString::to_string).collect();
        let FromFile::Options(found) = shell.complete_from_file(&file, &words, 1) else {
            panic!("options expected");
        };
        assert_eq!(found[0].text, "--ignore-case");
        assert!(found[0].space);
        assert_eq!(
            found[0].description.as_deref(),
            Some("Ignore case distinctions in patterns and data")
        );
        // The pattern, the first operand, is anything; `-e`'s argument too.
        assert!(complete(&mut shell, &["grep", "-i", ""]).is_empty());
        assert!(complete(&mut shell, &["grep", "-e", ""]).is_empty());
        // Files, after the pattern and for `-f`, `--file=` and `-o`.
        let dir = std::env::temp_dir().join(format!("nukesh-files-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("words.txt"), "").unwrap();
        let at = |name: &str| format!("{}/{name}", dir.display());
        assert_eq!(complete(&mut shell, &["grep", "-e", "x", "y", &at("wo")]), [at("words.txt")]);
        assert_eq!(complete(&mut shell, &["grep", "-f", &at("wo")]), [at("words.txt")]);
        assert_eq!(
            complete(&mut shell, &["grep", &format!("--file={}", at("wo"))]),
            [format!("--file={}", at("words.txt"))]
        );
        assert_eq!(
            complete(&mut shell, &["grep", &format!("-o{}", at("s"))]),
            [format!("-o{}", at("sub/"))]
        );
        // After `--`, a word with a dash is an operand.
        assert_eq!(complete(&mut shell, &["grep", "--", "x", &at("wo")]), [at("words.txt")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finds_files_by_name() {
        let dir = std::env::temp_dir().join(format!("nukesh-completions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("grep"), GREP).unwrap();
        let mut shell = Shell::new("test", false);
        shell.set_var("NUKESH_COMPLETION_PATH", &format!("/nowhere:{}", dir.display()));
        assert_eq!(shell.completion_file("grep").unwrap().options.len(), 4);
        assert!(shell.completion_file("cat").is_none());
        assert!(shell.completion_file("../grep").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
