// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The welcome banner above the sign-in prompt (ADR 0261): Duke and the
//! name drawn in plain ASCII, so that any console font has every
//! character, then the tagline and the system's name and version.
//! welcome.toml(5), `/system/settings/console/welcome.toml`, says when it
//! shows and what is in it. Where it shows, it takes the place of
//! /system/settings/issue's text; on a screen too small for Duke only the
//! two lines of words are shown.

use std::fmt::Write as _;

/// The banner's settings.
pub const SETTINGS: &str = "/system/settings/console/welcome.toml";

/// Made on the first boot with `show = "first-boot"`; the banner is not
/// shown again once it is there.
pub const SHOWN: &str = "/system/state/consoled/welcome-shown";

/// Duke, the mark, as many columns wide as `DUKE_WIDTH`.
const DUKE: [&str; 12] = [
    r" |\            /|",
    r" | \   /\ /\  / |",
    r" |  \_/  V  \/  |__",
    r" |                 `-.",
    r" |  ____________      \",
    r" | |            `.     \",
    r" | |   `--.__      \     |",
    r" | |     (__)    __/     |",
    r" | |           (__      |",
    r" | |     `-.___.-'     /",
    r" | |______________,-' /",
    r" |__________________,-'",
];

/// The name, in letters four lines high.
const NAME: [&str; 4] = [
    r"    _          _",
    r" __| |  _  _  | |__  ___  __ __  ___",
    r"/ _` | | || | | / / / _ \ \ V / / _ \",
    r"\__,_|  \_,_| |_\_\ \___/  \_/  \___/",
];

/// Where the words beside Duke start, and the widest the banner is.
const DUKE_WIDTH: usize = 29;
const WIDTH: usize = DUKE_WIDTH + 38;

/// The tagline shown unless welcome.toml says another.
pub const TAGLINE: &str = "A bright future. A human future.";

/// The system's name and version, with issue(5)'s escapes.
pub const VERSION: &str = "\\s Server \\r";

/// When the banner shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Show {
    /// Above the sign-in prompt, every time it is shown.
    EveryBoot,
    /// Only during the system's first boot.
    FirstBoot,
    /// Never: /system/settings/issue's text, as before.
    Never,
}

/// What is drawn above the words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Art {
    /// Duke, with the name beside him.
    Duke,
    /// The name only.
    Small,
    /// No art: the words only.
    None,
    /// A file's text, as it is.
    File(String),
}

/// welcome.toml, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub show: Show,
    pub art: Art,
    pub tagline: String,
    pub version: String,
    pub colors: bool,
}

impl Default for Welcome {
    fn default() -> Welcome {
        Welcome {
            show: Show::EveryBoot,
            art: Art::Duke,
            tagline: TAGLINE.into(),
            version: VERSION.into(),
            colors: true,
        }
    }
}

impl Welcome {
    /// Reads welcome.toml's text: the defaults for what it leaves out, and
    /// what was wrong, each mistake leaving its key at the default.
    pub fn parse(text: &str) -> (Welcome, Vec<String>) {
        let mut welcome = Welcome::default();
        let mut problems = Vec::new();
        let table: toml::Table = match text.parse() {
            Ok(table) => table,
            Err(err) => return (welcome, vec![err.to_string().trim().replace('\n', " ")]),
        };
        for (key, value) in &table {
            match (key.as_str(), value) {
                ("show", toml::Value::String(when)) => match when.as_str() {
                    "every-boot" | "always" => welcome.show = Show::EveryBoot,
                    "first-boot" => welcome.show = Show::FirstBoot,
                    "never" => welcome.show = Show::Never,
                    other => problems.push(format!(
                        "show: '{other}' is not \"every-boot\", \"first-boot\" or \"never\""
                    )),
                },
                ("art", toml::Value::String(art)) => {
                    welcome.art = match art.as_str() {
                        "duke" => Art::Duke,
                        "small" => Art::Small,
                        "none" | "" => Art::None,
                        path if path.starts_with('/') => Art::File(path.to_string()),
                        other => {
                            problems.push(format!(
                                "art: '{other}' is not \"duke\", \"small\", \"none\" or a file's path"
                            ));
                            continue;
                        }
                    };
                }
                ("tagline", toml::Value::String(text)) => welcome.tagline.clone_from(text),
                ("tagline", toml::Value::Boolean(on)) => {
                    welcome.tagline = if *on { TAGLINE.into() } else { String::new() };
                }
                ("version", toml::Value::String(text)) => welcome.version.clone_from(text),
                ("version", toml::Value::Boolean(on)) => {
                    welcome.version = if *on { VERSION.into() } else { String::new() };
                }
                ("colors", toml::Value::Boolean(on)) => welcome.colors = *on,
                ("show" | "art" | "tagline" | "version" | "colors", _) => {
                    problems.push(format!("{key}: not a value it takes (welcome.toml(5))"));
                }
                (other, _) => problems.push(format!("{other}: no such key")),
            }
        }
        (welcome, problems)
    }

    /// The banner's text for a screen `cols` by `rows`, with terminal
    /// escapes for its colours; `expand` gives the version line's escapes
    /// their values. Lines end with `\n`, and a blank line ends it.
    pub fn render(&self, cols: usize, rows: usize, expand: &dyn Fn(&str) -> String) -> String {
        let paint = |sgr: &str, text: &str| {
            if self.colors && !text.is_empty() {
                format!("\x1b[{sgr}m{text}\x1b[0m")
            } else {
                text.to_string()
            }
        };
        let version = expand(&self.version);
        let tagline = self.tagline.clone();
        let mut out = String::new();
        let art = match &self.art {
            Art::File(path) => match std::fs::read_to_string(path) {
                Ok(text) => {
                    out.push_str(&paint("1;33", text.trim_end_matches('\n')));
                    out.push('\n');
                    Art::None
                }
                Err(_) => Art::Duke,
            },
            other => other.clone(),
        };
        // Duke takes 12 lines and 67 columns; the name alone, 4 and 38.
        let art = match art {
            Art::Duke if cols > WIDTH && rows >= DUKE.len() + 6 => Art::Duke,
            Art::Duke | Art::Small if cols > NAME[3].len() && rows >= NAME.len() + 6 => Art::Small,
            Art::File(_) => Art::None,
            _ => Art::None,
        };
        match art {
            Art::Duke => {
                for (row, duke) in DUKE.iter().enumerate() {
                    let beside = match row {
                        2..=5 => paint("1;97", NAME[row - 2]),
                        7 => tagline.clone(),
                        9 => paint("90", &version),
                        _ => String::new(),
                    };
                    let mut line = paint("1;33", duke);
                    if !beside.is_empty() {
                        line.push_str(&" ".repeat(DUKE_WIDTH - duke.len()));
                        line.push_str(&beside);
                    }
                    let _ = writeln!(out, "{line}");
                }
            }
            Art::Small => {
                for name in NAME {
                    let _ = writeln!(out, " {}", paint("1;97", name));
                }
                out.push('\n');
                words(&mut out, &tagline, &paint("90", &version));
            }
            _ => words(&mut out, &tagline, &paint("90", &version)),
        }
        out.push('\n');
        out
    }
}

/// The tagline and the version, each on a line of its own if it has
/// words.
fn words(out: &mut String, tagline: &str, version: &str) {
    for line in [tagline, version] {
        if !line.is_empty() {
            let _ = writeln!(out, " {line}");
        }
    }
}

/// Whether the banner shows on this boot: by welcome.toml's `show`, and
/// for `first-boot`, whether `shown` says it has been shown already, which
/// it then records.
pub fn shows_this_boot(welcome: &Welcome, shown: &str) -> bool {
    match welcome.show {
        Show::EveryBoot => true,
        Show::Never => false,
        Show::FirstBoot => {
            if std::fs::metadata(shown).is_ok() {
                return false;
            }
            if let Some(dir) = std::path::Path::new(shown).parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(shown, b"");
            true
        }
    }
}

/// The shipped welcome.toml, which says the defaults.
#[cfg(test)]
const SHIPPED: &str = include_str!("../../../defaults/console/welcome.toml");

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn version(text: &str) -> String {
        text.replace("\\s", "Dukovo").replace("\\r", "0.10.4")
    }

    #[test]
    fn the_shipped_file_says_the_defaults() {
        let (welcome, problems) = Welcome::parse(SHIPPED);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(welcome, Welcome::default());
    }

    #[test]
    fn duke_fits_an_80_column_console() {
        let banner = Welcome::default().render(80, 30, &version);
        let text = plain(&banner);
        assert!(text.lines().all(|line| line.chars().count() <= WIDTH), "{text}");
        assert!(text.lines().all(str::is_ascii), "{text}");
        assert_eq!(text.lines().count(), DUKE.len() + 1);
        assert!(
            text.lines().nth(7).unwrap().ends_with("   A bright future. A human future."),
            "{text}"
        );
        assert!(text.contains("Dukovo Server 0.10.4\n"), "{text}");
        assert!(banner.contains("\x1b[1;33m |\\"), "{banner}");
    }

    #[test]
    fn small_screens_get_less() {
        let text = plain(&Welcome::default().render(60, 30, &version));
        assert!(!text.contains("(__)"), "{text}");
        assert!(text.contains("__| |"), "{text}");
        let text = plain(&Welcome::default().render(30, 10, &version));
        assert_eq!(text, " A bright future. A human future.\n Dukovo Server 0.10.4\n\n");
    }

    #[test]
    fn settings_change_it() {
        let (welcome, problems) = Welcome::parse(
            "show = \"first-boot\"\nart = \"none\"\ntagline = false\ncolors = false\n",
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(welcome.show, Show::FirstBoot);
        assert_eq!(welcome.render(80, 30, &version), " Dukovo Server 0.10.4\n\n");
        let (welcome, problems) = Welcome::parse("show = \"sometimes\"\nart = \"cat\"\nfoo = 1\n");
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert_eq!(welcome, Welcome::default());
        let (_, problems) = Welcome::parse("show = ");
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn first_boot_shows_once() {
        let dir = std::env::temp_dir().join(format!("welcome-test-{}", std::process::id()));
        let shown = dir.join("state/shown");
        let shown = shown.to_str().unwrap();
        let first = Welcome { show: Show::FirstBoot, ..Welcome::default() };
        assert!(shows_this_boot(&first, shown));
        assert!(!shows_this_boot(&first, shown));
        assert!(shows_this_boot(&Welcome::default(), shown));
        let never = Welcome { show: Show::Never, ..Welcome::default() };
        assert!(!shows_this_boot(&never, shown));
        let _ = std::fs::remove_dir_all(dir);
    }
}
