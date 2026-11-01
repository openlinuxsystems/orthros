// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The shell's colours (ADR 0258), and those it gives `ls`, `grep`,
//! `diff` and `ip` (ADR 0260), from colors.toml(5).
//!
//! The system's file is `/system/settings/nukesh/colors.toml`; a person's
//! own, `~/.config/nukesh/colors.toml`, changes what it names, key by key.
//! Only the terminal's 16 standard colours are offered, so that the
//! colours follow the terminal's theme. A colour is never all a thing
//! says: an unknown command is underlined as well as red, and the prompt's
//! badges are words. The colours are off when `NO_COLOR` is set, when the
//! terminal is dumb or not a terminal, or with `enabled = false`.
//!
//! A mistake in a file never stops the shell: the key keeps its default
//! and `nukesh --colors --check` says what was wrong.

use std::fmt::Write as _;

use reedline::Color;

/// The system's colours file.
pub const SYSTEM_FILE: &str = "/system/settings/nukesh/colors.toml";

/// A person's own, under their home.
pub const USER_FILE: &str = ".config/nukesh/colors.toml";

/// The 16 colours' names, in the order of their numbers (SGR 30-37, then
/// 90-97).
const NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bright-black",
    "bright-red",
    "bright-green",
    "bright-yellow",
    "bright-blue",
    "bright-magenta",
    "bright-cyan",
    "bright-white",
];

/// How a piece of text looks: one of the 16 colours on another, and
/// bold, dim, italic, underlined or reversed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<u8>,
    pub bg: Option<u8>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Style {
    /// Reads a style as colors.toml(5) writes it: words such as `"bold
    /// blue"`, `"red underline"` or `"bright-white on red"`; `"none"` or
    /// `""` for the terminal's own text.
    pub fn parse(text: &str) -> Result<Style, String> {
        let mut style = Style::default();
        let mut words = text.split_whitespace();
        while let Some(word) = words.next() {
            let word = word.to_ascii_lowercase();
            match word.as_str() {
                "none" | "plain" | "default" => {}
                "bold" => style.bold = true,
                "dim" => style.dim = true,
                "italic" => style.italic = true,
                "underline" | "underlined" => style.underline = true,
                "reverse" => style.reverse = true,
                "on" => {
                    let Some(color) = words.next() else {
                        return Err(format!("'{text}': no colour after 'on'"));
                    };
                    style.bg = Some(color_number(color).ok_or_else(|| not_a_color(color))?);
                }
                other => {
                    let number = color_number(other).ok_or_else(|| not_a_color(other))?;
                    if style.fg.is_some() {
                        return Err(format!(
                            "'{text}': two colours; write 'on' before a background"
                        ));
                    }
                    style.fg = Some(number);
                }
            }
        }
        Ok(style)
    }

    /// The SGR parameters, such as `1;34`, or `0` for plain text.
    pub fn sgr(self) -> String {
        let mut codes: Vec<String> = Vec::new();
        for (on, code) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.reverse, "7"),
        ] {
            if on {
                codes.push(code.into());
            }
        }
        if let Some(fg) = self.fg {
            codes
                .push(if fg < 8 { 30 + u32::from(fg) } else { 90 + u32::from(fg) - 8 }.to_string());
        }
        if let Some(bg) = self.bg {
            codes.push(
                if bg < 8 { 40 + u32::from(bg) } else { 100 + u32::from(bg) - 8 }.to_string(),
            );
        }
        if codes.is_empty() { "0".into() } else { codes.join(";") }
    }

    /// The style in git-config(1)'s words for a colour: attributes, then
    /// the text's colour and the background's, `normal` for the
    /// terminal's own.
    pub fn git(self) -> String {
        const NAMES: [&str; 8] =
            ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
        let name = |c: u8| {
            let base = NAMES[usize::from(c % 8)];
            if c < 8 { base.to_string() } else { format!("bright{base}") }
        };
        let mut words: Vec<String> = Vec::new();
        for (on, word) in [
            (self.bold, "bold"),
            (self.dim, "dim"),
            (self.italic, "italic"),
            (self.underline, "ul"),
            (self.reverse, "reverse"),
        ] {
            if on {
                words.push(word.into());
            }
        }
        match (self.fg, self.bg) {
            (Some(fg), Some(bg)) => words.extend([name(fg), name(bg)]),
            (Some(fg), None) => words.push(name(fg)),
            (None, Some(bg)) => words.extend(["normal".into(), name(bg)]),
            (None, None) => {}
        }
        if words.is_empty() { "normal".into() } else { words.join(" ") }
    }

    /// `text` in this style, reset after it.
    pub fn paint(self, text: &str) -> String {
        if self == Style::default() || text.is_empty() {
            return text.to_string();
        }
        format!("\x1b[{}m{text}\x1b[0m", self.sgr())
    }

    /// The style as reedline paints it.
    pub fn to_reedline(self) -> nu_ansi_term::Style {
        let mut style = nu_ansi_term::Style::new();
        style.foreground = self.fg.map(reedline_color);
        style.background = self.bg.map(reedline_color);
        style.is_bold = self.bold;
        style.is_dimmed = self.dim;
        style.is_italic = self.italic;
        style.is_underline = self.underline;
        style.is_reverse = self.reverse;
        style
    }

    /// The words that give this style, as colors.toml(5) writes them.
    pub fn describe(self) -> String {
        let mut words: Vec<&str> = Vec::new();
        for (on, word) in [
            (self.bold, "bold"),
            (self.dim, "dim"),
            (self.italic, "italic"),
            (self.underline, "underline"),
            (self.reverse, "reverse"),
        ] {
            if on {
                words.push(word);
            }
        }
        if let Some(fg) = self.fg {
            words.push(NAMES[usize::from(fg)]);
        }
        let mut out = words.join(" ");
        if let Some(bg) = self.bg {
            if !out.is_empty() {
                out.push(' ');
            }
            let _ = write!(out, "on {}", NAMES[usize::from(bg)]);
        }
        if out.is_empty() { "none".into() } else { out }
    }
}

fn not_a_color(word: &str) -> String {
    format!("'{word}' is not one of the 16 colours ({})", NAMES.join(", "))
}

/// A colour's number, 0 to 15, from its name; `grey` and `gray` are
/// `bright-black`, and `bright-` may be `light-` or `bright` with no dash.
fn color_number(name: &str) -> Option<u8> {
    let name = name.to_ascii_lowercase().replace('_', "-");
    let name = match name.as_str() {
        "grey" | "gray" => "bright-black".to_string(),
        "purple" => "magenta".to_string(),
        other => other
            .strip_prefix("light-")
            .or_else(|| other.strip_prefix("bright").map(|rest| rest.trim_start_matches('-')))
            .map_or_else(|| other.to_string(), |rest| format!("bright-{rest}")),
    };
    NAMES.iter().position(|n| *n == name).map(|n| n as u8)
}

/// nu-ansi-term's names for the 16 colours, which it sends as SGR 30-37
/// and 90-97.
fn reedline_color(number: u8) -> Color {
    match number {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Purple,
        6 => Color::Cyan,
        7 => Color::White,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightPurple,
        14 => Color::LightCyan,
        _ => Color::LightGray,
    }
}

/// A key of a section: its name, its default, and what it colours.
struct Key {
    name: &'static str,
    default: &'static str,
    about: &'static str,
}

const fn key(name: &'static str, default: &'static str, about: &'static str) -> Key {
    Key { name, default, about }
}

/// A section of colors.toml and its keys. Every section but `suggest` also
/// takes `enabled`.
struct Section {
    name: &'static str,
    keys: &'static [Key],
}

/// Every section and key, with its default: the shipped colors.toml says
/// the same, and a unit test checks that it does.
const SECTIONS: &[Section] = &[
    Section {
        name: "prompt",
        keys: &[
            key("user", "green", "your name"),
            key("at", "bright-black", "the @ between name and host"),
            key("host", "green", "the computer's name"),
            key("path", "bold blue", "the working folder"),
            key("symbol", "magenta", "the $ at the end"),
            key("error", "bold bright-white on red", "the ✗ badge after a failed command"),
            key("admin", "bold bright-white on yellow", "the admin badge while you are root"),
            key("private", "bold bright-white on blue", "the private badge of nukesh --private"),
            key("took", "yellow", "how long the last command took, on the right"),
            key("jobs", "cyan", "how many jobs are running or stopped, on the right"),
            key("branch", "magenta", "the git branch of the folder, on the right"),
            key(
                "git-state",
                "bold yellow",
                "an operation under way, or a tag or commit when HEAD is detached",
            ),
            key("git-sync", "cyan", "how far ahead of and behind its upstream the branch is"),
            key("git-conflict", "bold red", "the files in conflict"),
            key("git-staged", "green", "the files staged for the next commit"),
            key("git-changed", "yellow", "the files changed and not staged"),
            key("git-untracked", "red", "the files git does not track"),
            key("git-stash", "bright-black", "the stashes"),
            key("git-clean", "green", "the mark of a work tree with nothing changed"),
        ],
    },
    Section {
        name: "line",
        keys: &[
            key("command", "green", "a command that exists"),
            key("unknown", "red underline", "a command that does not"),
            key("keyword", "bold blue", "if, then, for, while and the rest"),
            key("option", "cyan", "-x and --long"),
            key("string", "yellow", "quoted text"),
            key("variable", "magenta", "$NAME, ${...} and NAME= in an assignment"),
            key("operator", "bold yellow", "|, &&, ;, redirections"),
            key("comment", "bright-black", "# to the end of the line"),
            key("hint", "bright-black", "a suggestion from your history"),
        ],
    },
    Section {
        name: "ls",
        keys: &[
            key("directory", "bold blue", "folders"),
            key("program", "bold green", "files you can run"),
            key("link", "cyan", "symbolic links"),
            key("broken-link", "red reverse", "links to nothing"),
            key("archive", "red", "archives and compressed files"),
            key("pipe", "yellow", "named pipes"),
            key("socket", "magenta", "sockets"),
            key("device", "bold yellow", "devices"),
        ],
    },
    Section {
        name: "grep",
        keys: &[
            key("match", "bold red", "the text that matched"),
            key("file", "magenta", "file names"),
            key("line", "green", "line numbers"),
            key("separator", "bright-black", "the : and - between them"),
        ],
    },
    Section {
        name: "diff",
        keys: &[
            key("added", "green", "lines added"),
            key("removed", "red", "lines removed"),
            key("header", "bold", "the files' names"),
            key("hunk", "cyan", "the @@ line of each change"),
            key("added-word", "green reverse", "the words a changed line gained, in git's diffs"),
            key("removed-word", "red reverse", "the words a changed line lost, in git's diffs"),
            key("moved-added", "bold cyan", "lines git saw moved, where they went"),
            key("moved-removed", "bold magenta", "lines git saw moved, where they were"),
        ],
    },
    Section {
        name: "ip",
        keys: &[
            key("name", "cyan", "interface names"),
            key("up", "green", "UP"),
            key("down", "red", "DOWN"),
            key("address", "magenta", "IPv4 addresses"),
            key("address6", "blue", "IPv6 addresses"),
            key("mac", "yellow", "hardware addresses"),
        ],
    },
];

/// The `suggest` section's switches.
const SUGGEST: &[(&str, &str)] = &[
    ("history", "faded text after the cursor from your history; → takes it"),
    ("did-you-mean", "a command's likely name after \"command not found\""),
    ("options", "what an option does, after the cursor as you type it"),
];

/// The numbers: section, key, default and what it is.
pub const NUMBERS: &[(&str, &str, u64, &str)] =
    &[("prompt", "took-after", 3, "seconds a command takes before the right prompt says how long")];

/// The themes, and the keys each changes from the defaults.
/// A theme: its name, what it is for, and its keys.
pub type Theme = (&'static str, &'static str, &'static [(&'static str, &'static str)]);

pub const THEMES: &[Theme] = &[
    ("default", "the colours above", &[]),
    (
        "high-contrast",
        "bright and bold, nothing faded",
        &[
            ("prompt.user", "bold bright-green"),
            ("prompt.at", "white"),
            ("prompt.host", "bold bright-green"),
            ("prompt.path", "bold bright-blue"),
            ("prompt.symbol", "bold bright-magenta"),
            ("prompt.took", "bold bright-yellow"),
            ("prompt.jobs", "bold bright-cyan"),
            ("prompt.branch", "bold bright-magenta"),
            ("line.command", "bold bright-green"),
            ("line.unknown", "bold bright-red underline"),
            ("line.keyword", "bold bright-blue"),
            ("line.option", "bold bright-cyan"),
            ("line.string", "bold bright-yellow"),
            ("line.variable", "bold bright-magenta"),
            ("line.operator", "bold bright-yellow"),
            ("line.comment", "white italic"),
            ("line.hint", "white italic"),
            ("ls.directory", "bold bright-blue"),
            ("ls.program", "bold bright-green"),
            ("ls.link", "bold bright-cyan"),
            ("grep.match", "bold bright-white on red"),
            ("grep.separator", "white"),
            ("diff.added", "bold bright-green"),
            ("diff.removed", "bold bright-red"),
            ("diff.hunk", "bold bright-cyan"),
            ("diff.added-word", "bold bright-green reverse"),
            ("diff.removed-word", "bold bright-red reverse"),
            ("diff.moved-added", "bold bright-cyan"),
            ("diff.moved-removed", "bold bright-magenta"),
        ],
    ),
    (
        "light",
        "for a terminal with a light background: no yellow or white text",
        &[
            ("prompt.symbol", "bold magenta"),
            ("prompt.took", "bold red"),
            ("prompt.admin", "bold black on bright-yellow"),
            ("line.string", "red"),
            ("line.operator", "bold magenta"),
            ("line.hint", "bright-black italic"),
            ("ls.pipe", "magenta"),
            ("ls.device", "bold magenta"),
            ("grep.line", "blue"),
            ("ip.mac", "magenta"),
        ],
    ),
    (
        "colorblind",
        "blue and yellow for good and bad, for red-green colour blindness",
        &[
            ("prompt.user", "blue"),
            ("prompt.host", "blue"),
            ("prompt.error", "bold black on bright-yellow"),
            ("prompt.admin", "bold bright-white on magenta"),
            ("line.command", "blue"),
            ("line.unknown", "bold yellow underline"),
            ("line.keyword", "bold cyan"),
            ("line.string", "magenta"),
            ("line.variable", "bright-magenta"),
            ("ls.program", "bold yellow"),
            ("ls.broken-link", "yellow reverse"),
            ("ls.archive", "magenta"),
            ("grep.match", "bold yellow"),
            ("grep.line", "blue"),
            ("diff.added", "blue"),
            ("diff.removed", "yellow"),
            ("diff.added-word", "blue reverse"),
            ("diff.removed-word", "yellow reverse"),
            ("ip.up", "blue"),
            ("ip.down", "yellow"),
        ],
    ),
];

/// colors.toml, read: the styles of every key, and the switches.
#[derive(Clone, Debug)]
pub struct Colors {
    /// `enabled` at the top: colours at all.
    pub enabled: bool,
    /// Each section's `enabled`, by section.
    sections_on: Vec<(&'static str, bool)>,
    /// Each key's style, as `section.key`.
    styles: Vec<(String, Style)>,
    pub history_hints: bool,
    pub did_you_mean: bool,
    pub option_help: bool,
    /// The theme the keys started from.
    pub theme: &'static str,
    /// The numbers, as `section.key`.
    numbers: Vec<(String, u64)>,
}

impl Default for Colors {
    fn default() -> Colors {
        let mut styles = Vec::new();
        for section in SECTIONS {
            for key in section.keys {
                let style = Style::parse(key.default).unwrap_or_default();
                styles.push((format!("{}.{}", section.name, key.name), style));
            }
        }
        Colors {
            enabled: true,
            sections_on: SECTIONS.iter().map(|s| (s.name, true)).collect(),
            styles,
            history_hints: true,
            did_you_mean: true,
            option_help: true,
            theme: "default",
            numbers: NUMBERS.iter().map(|(s, k, n, _)| (format!("{s}.{k}"), *n)).collect(),
        }
    }
}

/// A mistake found in a file: the file, then what was wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub file: String,
    pub message: String,
}

impl Colors {
    /// The system's file, then the person's, over the defaults. Files
    /// that are not there are skipped; the mistakes in those that are
    /// come back with the colours.
    pub fn load(home: Option<&str>) -> (Colors, Vec<Problem>) {
        let mut colors = Colors::default();
        let mut problems = Vec::new();
        let mut files = vec![SYSTEM_FILE.to_string()];
        if let Some(home) = home.filter(|h| !h.is_empty()) {
            files.push(format!("{}/{USER_FILE}", home.trim_end_matches('/')));
        }
        for file in files {
            if let Ok(text) = std::fs::read_to_string(&file) {
                for message in colors.apply(&text) {
                    problems.push(Problem { file: file.clone(), message });
                }
            }
        }
        (colors, problems)
    }

    /// Takes the keys `text` sets; returns what was wrong in it.
    pub fn apply(&mut self, text: &str) -> Vec<String> {
        let table: toml::Table = match text.parse() {
            Ok(table) => table,
            Err(err) => return vec![err.to_string().trim().replace('\n', " ")],
        };
        let mut problems = Vec::new();
        // The theme first, so that the file's own keys change it.
        if let Some(value) = table.get("theme") {
            match value.as_str().and_then(|name| THEMES.iter().find(|t| t.0 == name)) {
                Some(theme) => self.use_theme(theme),
                None => problems.push(format!(
                    "theme: one of {}",
                    THEMES.iter().map(|t| format!("\"{}\"", t.0)).collect::<Vec<_>>().join(", ")
                )),
            }
        }
        for (name, value) in &table {
            match (name.as_str(), value) {
                ("theme", _) => {}
                ("enabled", toml::Value::Boolean(on)) => self.enabled = *on,
                ("enabled", _) => problems.push("enabled: true or false".into()),
                ("suggest", toml::Value::Table(keys)) => {
                    for (key, value) in keys {
                        match (key.as_str(), value.as_bool()) {
                            ("history", Some(on)) => self.history_hints = on,
                            ("did-you-mean", Some(on)) => self.did_you_mean = on,
                            ("options", Some(on)) => self.option_help = on,
                            ("history" | "did-you-mean" | "options", None) => {
                                problems.push(format!("[suggest] {key}: true or false"));
                            }
                            _ => problems.push(format!("[suggest] {key}: no such key")),
                        }
                    }
                }
                (section, toml::Value::Table(keys)) => {
                    let Some(known) = SECTIONS.iter().find(|s| s.name == section) else {
                        problems.push(format!("[{section}]: no such section"));
                        continue;
                    };
                    for (key, value) in keys {
                        if key == "enabled" {
                            match value.as_bool() {
                                Some(on) => self.set_section(known.name, on),
                                None => {
                                    problems.push(format!("[{section}] enabled: true or false"));
                                }
                            }
                            continue;
                        }
                        if NUMBERS.iter().any(|(s, k, _, _)| *s == section && k == key) {
                            match value.as_integer().and_then(|n| u64::try_from(n).ok()) {
                                Some(n) => self.set_number(&format!("{section}.{key}"), n),
                                None => {
                                    problems
                                        .push(format!("[{section}] {key}: a number of seconds"));
                                }
                            }
                            continue;
                        }
                        if !known.keys.iter().any(|k| k.name == key) {
                            problems.push(format!("[{section}] {key}: no such key"));
                            continue;
                        }
                        let Some(text) = value.as_str() else {
                            problems.push(format!(
                                "[{section}] {key}: a style in quotes, such as \"bold blue\""
                            ));
                            continue;
                        };
                        match Style::parse(text) {
                            Ok(style) => self.set(&format!("{section}.{key}"), style),
                            Err(err) => problems.push(format!("[{section}] {key}: {err}")),
                        }
                    }
                }
                (other, _) => problems.push(format!("{other}: no such key")),
            }
        }
        problems
    }

    fn set(&mut self, name: &str, style: Style) {
        if let Some(entry) = self.styles.iter_mut().find(|(n, _)| n == name) {
            entry.1 = style;
        }
    }

    fn use_theme(&mut self, theme: &Theme) {
        let defaults = Colors::default();
        self.styles.clone_from(&defaults.styles);
        for (name, style) in theme.2 {
            self.set(name, Style::parse(style).unwrap_or_default());
        }
        self.theme = theme.0;
    }

    fn set_number(&mut self, name: &str, number: u64) {
        if let Some(entry) = self.numbers.iter_mut().find(|(n, _)| n == name) {
            entry.1 = number;
        }
    }

    /// The number `section.key`; 0 for one that is not known.
    pub fn number(&self, name: &str) -> u64 {
        self.numbers.iter().find(|(n, _)| n == name).map_or(0, |(_, n)| *n)
    }

    fn set_section(&mut self, name: &str, on: bool) {
        if let Some(entry) = self.sections_on.iter_mut().find(|(n, _)| *n == name) {
            entry.1 = on;
        }
    }

    /// The style of `section.key`; plain for one that is not known.
    pub fn style(&self, name: &str) -> Style {
        self.styles.iter().find(|(n, _)| n == name).map_or_else(Style::default, |(_, s)| *s)
    }

    /// Whether a section is on: `enabled` at the top and in the section.
    pub fn on(&self, section: &str) -> bool {
        self.enabled && self.sections_on.iter().any(|(n, on)| *n == section && *on)
    }

    /// `LS_COLORS`, for `ls --color`.
    pub fn ls_colors(&self) -> String {
        let s = |key: &str| self.style(&format!("ls.{key}")).sgr();
        let mut out = format!(
            "di={}:ex={}:ln={}:or={}:mi={}:pi={}:so={}:bd={}:cd={}",
            s("directory"),
            s("program"),
            s("link"),
            s("broken-link"),
            s("broken-link"),
            s("pipe"),
            s("socket"),
            s("device"),
            s("device"),
        );
        for ext in ARCHIVES {
            let _ = write!(out, ":*.{ext}={}", s("archive"));
        }
        out
    }

    /// `GREP_COLORS`, for `grep --color`.
    pub fn grep_colors(&self) -> String {
        let s = |key: &str| self.style(&format!("grep.{key}")).sgr();
        format!(
            "ms={}:mc={}:fn={}:ln={}:bn={}:se={}",
            s("match"),
            s("match"),
            s("file"),
            s("line"),
            s("line"),
            s("separator")
        )
    }

    /// diff's `--palette`.
    pub fn diff_palette(&self) -> String {
        let s = |key: &str| self.style(&format!("diff.{key}")).sgr();
        format!("ad={}:de={}:hd={}:ln={}", s("added"), s("removed"), s("header"), s("hunk"))
    }

    /// The settings git takes from colors.toml (ADR 0293), as a
    /// git-config(1) file that the system's gitconfig includes: diffs,
    /// status and branches in the shell's colours, and diffs shown with
    /// the words that changed marked. Empty with diff's colours off.
    pub fn git_config(&self) -> String {
        if !self.on("diff") {
            return String::new();
        }
        let s = |key: &str| self.style(key).git();
        let mut out = String::from(GIT_CONFIG_HEADER);
        let _ = write!(
            out,
            "[color \"diff\"]\n\tnew = {}\n\told = {}\n\tmeta = {}\n\tfrag = {}\n\
             \tnewMoved = {}\n\toldMoved = {}\n",
            s("diff.added"),
            s("diff.removed"),
            s("diff.header"),
            s("diff.hunk"),
            s("diff.moved-added"),
            s("diff.moved-removed"),
        );
        if self.on("prompt") {
            let _ = write!(
                out,
                "[color \"status\"]\n\tadded = {}\n\tchanged = {}\n\tuntracked = {}\n\
                 \tunmerged = {}\n[color \"branch\"]\n\tcurrent = {}\n",
                s("prompt.git-staged"),
                s("prompt.git-changed"),
                s("prompt.git-untracked"),
                s("prompt.git-conflict"),
                s("prompt.branch"),
            );
        }
        out.push_str(
            "[pager]\n\tdiff = nukesh --diff-pager\n\tshow = nukesh --diff-pager\n\
             \tlog = nukesh --diff-pager\n[interactive]\n\tdiffFilter = nukesh --diff-highlight\n",
        );
        out
    }

    /// `IP_COLORS`, which ip(8) reads, in `GREP_COLORS`'s form.
    pub fn ip_colors(&self) -> String {
        let keys = ["name", "up", "down", "address", "address6", "mac"];
        keys.iter()
            .map(|key| format!("{key}={}", self.style(&format!("ip.{key}")).sgr()))
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// What the file of [`Colors::git_config`] says of itself.
const GIT_CONFIG_HEADER: &str = "# Made by Nukesh from colors.toml(5) each time a shell starts, for the\n\
# system's gitconfig to include (ADR 0293). Change colors.toml, not this\n\
# file; settings in ~/.gitconfig win over these.\n";

/// Where [`write_git_config`] puts git's settings, beneath the user's
/// folder.
pub const GIT_CONFIG: &str = ".local/state/nukesh/gitconfig";

/// Writes git's settings from colors.toml to [`GIT_CONFIG`] in `home`,
/// if they changed (ADR 0293).
pub fn write_git_config(colors: &Colors, home: &str) {
    let path = format!("{}/{GIT_CONFIG}", home.trim_end_matches('/'));
    let text = colors.git_config();
    if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
        return;
    }
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, text);
}

/// The file name endings `ls` colours as archives.
const ARCHIVES: &[&str] = &[
    "7z", "ar", "bz2", "cpio", "deb", "gz", "iso", "jar", "lz", "lz4", "lzma", "rar", "rpm", "tar",
    "tbz", "tgz", "txz", "tzst", "xz", "z", "zip", "zst",
];

/// Whether colours may be used on the terminal `fd`: it is one, `TERM` is
/// not dumb, and `NO_COLOR` is not set to something (no-color.org).
pub fn allowed(no_color: Option<&str>, term: Option<&str>, is_tty: bool) -> bool {
    is_tty
        && no_color.is_none_or(str::is_empty)
        && term.is_some_and(|t| !t.is_empty() && t != "dumb")
}

/// Gives `ls`, `grep`, `diff` and `ip` their colours in an interactive
/// shell (ADR 0260): an alias that asks each for colour on a terminal
/// (`--color=auto`, which leaves pipes and files plain), and the variable
/// it takes its colours from, made from colors.toml. An alias or variable
/// already there is kept; the startup files may change or remove them.
/// Nothing is set with `NO_COLOR` set or `enabled = false`.
pub fn color_the_tools(shell: &mut crate::shell::Shell) {
    let colors = shell.colors.clone();
    if !colors.enabled || shell.get("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let mut tool = |section: &str, alias: String, variable: Option<(&str, String)>| {
        if !colors.on(section) {
            return;
        }
        if !shell.aliases.contains_key(section) {
            shell.aliases.insert(section.to_string(), alias);
        }
        if let Some((name, value)) = variable
            && shell.get(name).is_none()
        {
            let var = shell.vars.entry(name);
            var.value = Some(crate::vars::Value::Str(value));
            var.attrs |= crate::vars::attr::EXPORT;
        }
    };
    tool("ls", "ls --color=auto".into(), Some(("LS_COLORS", colors.ls_colors())));
    tool("grep", "grep --color=auto".into(), Some(("GREP_COLORS", colors.grep_colors())));
    tool("diff", format!("diff --color=auto --palette='{}'", colors.diff_palette()), None);
    tool("ip", "ip -color=auto".into(), Some(("IP_COLORS", colors.ip_colors())));
}

/// `nukesh --colors [--check] [file]`: shows every key in its colour, or
/// with `--check`, only checks the files. Returns the exit status.
pub fn command(args: &[String]) -> i32 {
    let mut check = false;
    let mut files = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--check" => check = true,
            "-h" | "--help" => {
                println!("usage: nukesh --colors [--check] [file ...]");
                return 0;
            }
            other if other.starts_with('-') => {
                eprintln!("nukesh: --colors: unknown option {other}");
                eprintln!("usage: nukesh --colors [--check] [file ...]");
                return 2;
            }
            file => files.push(file.to_string()),
        }
    }
    let home = std::env::var("HOME").ok();
    let (colors, problems) = if files.is_empty() {
        Colors::load(home.as_deref())
    } else {
        let mut colors = Colors::default();
        let mut problems = Vec::new();
        for file in &files {
            match std::fs::read_to_string(file) {
                Ok(text) => {
                    for message in colors.apply(&text) {
                        problems.push(Problem { file: file.clone(), message });
                    }
                }
                Err(err) => problems.push(Problem { file: file.clone(), message: err.to_string() }),
            }
        }
        (colors, problems)
    };
    for problem in &problems {
        eprintln!("nukesh: {}: {}", problem.file, problem.message);
    }
    if check {
        if problems.is_empty() {
            println!("nukesh: the colours are as they should be");
        }
        return i32::from(!problems.is_empty());
    }
    let paint = allowed(
        std::env::var("NO_COLOR").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
        crate::sys::isatty(1),
    ) && colors.enabled;
    print!("{}", preview(&colors, paint));
    i32::from(!problems.is_empty())
}

/// Every section and key with its style's words, each in its colour when
/// `paint`.
pub fn preview(colors: &Colors, paint: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "enabled = {}", colors.enabled);
    let _ = writeln!(out, "theme = \"{}\"", colors.theme);
    for section in SECTIONS {
        let on = colors.sections_on.iter().any(|(n, on)| *n == section.name && *on);
        let _ = writeln!(out, "\n[{}]{}", section.name, if on { "" } else { "  (off)" });
        for key in section.keys {
            let style = colors.style(&format!("{}.{}", section.name, key.name));
            let shown = format!("{:<12}", key.name);
            let shown = if paint { style.paint(&shown) } else { shown };
            let _ = writeln!(out, "{shown} {:<28} {}", style.describe(), key.about);
        }
        for (_, key, _, about) in NUMBERS.iter().filter(|n| n.0 == section.name) {
            let number = colors.number(&format!("{}.{key}", section.name));
            let _ = writeln!(out, "{key:<12} {number:<28} {about}");
        }
    }
    let _ = writeln!(out, "\n[suggest]");
    for (name, about) in SUGGEST {
        let on = match *name {
            "history" => colors.history_hints,
            "did-you-mean" => colors.did_you_mean,
            _ => colors.option_help,
        };
        let _ = writeln!(out, "{name:<12} {:<28} {about}", if on { "true" } else { "false" });
    }
    out
}

/// The file shipped as /system/settings/nukesh/colors.toml.
#[cfg(test)]
const SHIPPED: &str = include_str!("../../../defaults/nukesh/colors.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_read_as_words() {
        let style = Style::parse("bold blue").unwrap();
        assert_eq!(style.sgr(), "1;34");
        assert_eq!(Style::parse("bright-white on red").unwrap().sgr(), "97;41");
        assert_eq!(Style::parse("red underline").unwrap().sgr(), "4;31");
        assert_eq!(Style::parse("light-cyan").unwrap().sgr(), "96");
        assert_eq!(Style::parse("grey").unwrap().sgr(), "90");
        assert_eq!(Style::parse("none").unwrap().sgr(), "0");
        assert_eq!(Style::parse("").unwrap(), Style::default());
        assert!(Style::parse("orange").is_err());
        assert!(Style::parse("red blue").is_err());
        assert!(Style::parse("on").is_err());
        assert_eq!(
            Style::parse("dim bright-black on blue").unwrap().describe(),
            "dim bright-black on blue"
        );
    }

    #[test]
    fn painted_text_resets_after_it() {
        assert_eq!(Style::parse("green").unwrap().paint("ok"), "\x1b[32mok\x1b[0m");
        assert_eq!(Style::default().paint("ok"), "ok");
    }

    #[test]
    fn files_change_only_what_they_name() {
        let mut colors = Colors::default();
        let problems = colors.apply("[line]\ncommand = \"bright-green\"\n[ls]\nenabled = false\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(colors.style("line.command").sgr(), "92");
        assert_eq!(colors.style("line.string").sgr(), "33");
        assert!(!colors.on("ls"));
        assert!(colors.on("grep"));
    }

    #[test]
    fn mistakes_are_reported_and_skipped() {
        let mut colors = Colors::default();
        let problems = colors.apply(
            "colour = 1\n[line]\ncommand = \"blu\"\nnope = \"red\"\n[nothing]\nx = 1\n[suggest]\nhistory = \"yes\"\n",
        );
        assert_eq!(problems.len(), 5, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("'blu' is not one of the 16 colours")));
        assert_eq!(colors.style("line.command").sgr(), "32");
        assert!(colors.history_hints);
        assert!(!Colors::default().apply("[line").is_empty());
    }

    #[test]
    fn enabled_false_turns_everything_off() {
        let mut colors = Colors::default();
        assert!(colors.apply("enabled = false").is_empty());
        assert!(!colors.on("line"));
        assert!(!colors.on("prompt"));
    }

    #[test]
    fn the_tools_get_their_colours() {
        let colors = Colors::default();
        assert!(colors.ls_colors().starts_with("di=1;34:ex=1;32:ln=36:"));
        assert!(colors.ls_colors().contains(":*.zst=31"));
        assert_eq!(colors.grep_colors(), "ms=1;31:mc=1;31:fn=35:ln=32:bn=32:se=90");
        assert_eq!(colors.diff_palette(), "ad=32:de=31:hd=1:ln=36");
        assert_eq!(colors.ip_colors(), "name=36:up=32:down=31:address=35:address6=34:mac=33");
    }

    #[test]
    fn colour_needs_a_terminal_and_no_no_color() {
        assert!(allowed(None, Some("linux"), true));
        assert!(allowed(Some(""), Some("linux"), true));
        assert!(!allowed(Some("1"), Some("linux"), true));
        assert!(!allowed(None, Some("dumb"), true));
        assert!(!allowed(None, None, true));
        assert!(!allowed(None, Some("linux"), false));
    }

    #[test]
    fn themes_start_from_the_defaults_and_the_file_wins() {
        for (name, _, keys) in THEMES {
            for (key, style) in *keys {
                assert!(Style::parse(style).is_ok(), "{name}: {key}");
                assert!(Colors::default().styles.iter().any(|(n, _)| n == key), "{name}: {key}");
            }
        }
        let mut colors = Colors::default();
        let problems = colors.apply("theme = \"colorblind\"\n[diff]\nadded = \"cyan\"\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(colors.style("diff.added").sgr(), "36");
        assert_eq!(colors.style("diff.removed").sgr(), "33");
        assert_eq!(colors.theme, "colorblind");
        assert!(!Colors::default().apply("theme = \"neon\"").is_empty());
        let mut colors = Colors::default();
        assert_eq!(colors.apply("[prompt]\ntook-after = 10\n[ls]\nenabled = 1").len(), 1);
        assert_eq!(colors.number("prompt.took-after"), 10);
        assert_eq!(colors.apply("[prompt]\ntook-after = -1").len(), 1);
    }

    /// The shipped file sets every key to its default, so that what it
    /// says is what the shell does without it.
    #[test]
    fn the_shipped_file_says_the_defaults() {
        let mut colors = Colors::default();
        let problems = colors.apply(SHIPPED);
        assert!(problems.is_empty(), "{problems:?}");
        let defaults = Colors::default();
        assert_eq!(colors.styles, defaults.styles);
        assert!(
            colors.enabled && colors.history_hints && colors.did_you_mean && colors.option_help
        );
        assert_eq!(colors.numbers, defaults.numbers);
        assert_eq!(colors.theme, "default");
        for (section, key, number, _) in NUMBERS {
            assert!(
                SHIPPED.contains(&format!("{key} = {number}")),
                "colors.toml lacks {key} in [{section}]"
            );
        }
        for section in SECTIONS {
            for key in section.keys {
                let line = format!("{} = \"{}\"", key.name, key.default);
                assert!(SHIPPED.contains(&line), "colors.toml lacks {line} in [{}]", section.name);
            }
        }
    }
}
