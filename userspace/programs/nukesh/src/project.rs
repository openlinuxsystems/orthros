// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Folder settings (ADR 0286), as direnv has them: a `.nukesh.toml` in a
//! folder sets variables and abbreviations while an interactive shell is
//! in that folder or under it, and they go when it leaves.
//!
//! ```toml
//! [env]
//! DATABASE_URL = "postgres://localhost/site"
//! [abbr]
//! t = "make test"
//! ```
//!
//! A file runs nothing, but what it sets can change what commands do, so
//! it applies only once you have read it and said `trust`. The trusted
//! copy is kept in `~/.local/state/nukesh/trusted/`; a file that changes
//! after is not trusted again until you say so. `trust -r` takes it back.

use crate::shell::Shell;

/// The file's name.
pub const FILE: &str = ".nukesh.toml";

/// Where trusted copies are kept, under the home folder.
const TRUSTED: &str = ".local/state/nukesh/trusted";

/// What a folder file set, to be undone when the shell leaves.
#[derive(Debug, Default)]
pub struct Applied {
    pub file: String,
    /// Each variable set, and its value before.
    vars: Vec<(String, Option<String>)>,
    /// Each abbreviation set, and the one it replaced.
    abbrs: Vec<(String, Option<String>)>,
}

/// What a file sets.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub env: Vec<(String, String)>,
    pub abbrs: Vec<(String, String)>,
}

/// The file's settings, or what is wrong with it.
pub fn parse(text: &str) -> Result<Settings, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.message().to_string())?;
    let mut settings = Settings::default();
    for (section, value) in &table {
        let keys = value.as_table().ok_or_else(|| format!("{section}: not a section"))?;
        for (key, value) in keys {
            let value = value
                .as_str()
                .ok_or_else(|| format!("[{section}] {key}: not a string"))?
                .to_string();
            match section.as_str() {
                "env" if crate::parse::is_name(key) => settings.env.push((key.clone(), value)),
                "env" => return Err(format!("[env] {key}: not a variable name")),
                "abbr" => settings.abbrs.push((key.clone(), value)),
                _ => return Err(format!("[{section}]: no such section")),
            }
        }
    }
    Ok(settings)
}

/// The nearest folder file at or above `dir`.
pub fn find(dir: &str) -> Option<String> {
    let mut dir = std::path::PathBuf::from(dir);
    loop {
        let file = dir.join(FILE);
        if file.is_file() {
            return Some(file.to_string_lossy().into_owned());
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn trusted_copy(shell: &Shell, file: &str) -> Option<String> {
    let home = shell.get("HOME").filter(|h| h.starts_with('/'))?;
    let name: String = file.chars().map(|c| if c == '/' { '%' } else { c }).collect();
    Some(format!("{}/{TRUSTED}/{name}", home.trim_end_matches('/')))
}

/// Whether `file` is trusted as it is now.
pub fn trusted(shell: &Shell, file: &str) -> bool {
    let Some(copy) = trusted_copy(shell, file) else { return false };
    match (std::fs::read(file), std::fs::read(copy)) {
        (Ok(now), Ok(then)) => now == then,
        _ => false,
    }
}

fn undo(shell: &mut Shell, applied: Applied) {
    for (name, before) in applied.vars.into_iter().rev() {
        match before {
            Some(value) => {
                let _ = shell.assign(&name, value, false);
            }
            None => {
                shell.unset(&name);
            }
        }
    }
    for (name, before) in applied.abbrs.into_iter().rev() {
        match before {
            Some(expansion) => {
                shell.abbrs.insert(name, expansion);
            }
            None => {
                shell.abbrs.shift_remove(&name);
            }
        }
    }
}

fn apply(shell: &mut Shell, file: &str, settings: Settings) -> Applied {
    let mut applied = Applied { file: file.to_string(), ..Applied::default() };
    for (name, value) in settings.env {
        let before = shell.get(&name);
        if shell.assign(&name, value, false).is_ok() {
            shell.vars.entry(&name).attrs |= crate::vars::attr::EXPORT;
            applied.vars.push((name, before));
        }
    }
    for (name, expansion) in settings.abbrs {
        let before = shell.abbrs.insert(name.clone(), expansion);
        applied.abbrs.push((name, before));
    }
    applied
}

/// Before each prompt: the folder file for where the shell is now,
/// applied if trusted, and the last one's settings undone if it left.
pub fn before_prompt(shell: &mut Shell) {
    if !shell.settings.project_files {
        return;
    }
    let here = shell.get("PWD").unwrap_or_default();
    let found = find(&here).filter(|f| {
        trusted(shell, f) || {
            if shell.project_warned.insert(f.clone()) {
                shell.error(&format!(
                    "{f} sets variables or abbreviations here; read it, then run `trust` to let it"
                ));
            }
            false
        }
    });
    let current = shell.project.as_ref().map(|a| a.file.clone());
    if found == current {
        return;
    }
    if let Some(applied) = shell.project.take() {
        undo(shell, applied);
    }
    let Some(file) = found else { return };
    match std::fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|t| parse(&t)) {
        Ok(settings) => shell.project = Some(apply(shell, &file, settings)),
        Err(problem) => shell.error(&format!("{file}: {problem}")),
    }
}

/// `trust [-r] [file]`: trusts the nearest folder file as it is now, or
/// with `-r` stops trusting it.
pub fn trust(shell: &mut Shell, args: &[String]) -> crate::shell::Res {
    let args = &args[1..];
    let (remove, named) = match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            println!("usage: trust [-r] [file]");
            return Ok(0);
        }
        Some("-r") => (true, args.get(1)),
        _ => (false, args.first()),
    };
    let here = shell.get("PWD").unwrap_or_default();
    let file = match named {
        Some(name) => std::fs::canonicalize(name).map(|p| p.to_string_lossy().into_owned()).ok(),
        None => find(&here),
    };
    let Some(file) = file else {
        shell.error(&format!("trust: no {FILE} here or above"));
        return Ok(1);
    };
    let Some(copy) = trusted_copy(shell, &file) else {
        shell.error("trust: HOME is not set");
        return Ok(1);
    };
    if remove {
        let _ = std::fs::remove_file(&copy);
        if let Some(applied) = shell.project.take_if(|a| a.file == file) {
            undo(shell, applied);
        }
        println!("{file} is no longer trusted");
        return Ok(0);
    }
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) => {
            shell.error(&format!("trust: {file}: {err}"));
            return Ok(1);
        }
    };
    if let Err(problem) = parse(&text) {
        shell.error(&format!("trust: {file}: {problem}"));
        return Ok(1);
    }
    if let Some(dir) = std::path::Path::new(&copy).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(err) = std::fs::write(&copy, &text) {
        shell.error(&format!("trust: {copy}: {err}"));
        return Ok(1);
    }
    println!("{file} is trusted; it applies while you are in its folder");
    before_prompt(shell);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_set_variables_and_abbreviations() {
        let settings = parse("[env]\nA = \"1\"\n[abbr]\nt = \"make test\"\n").unwrap();
        assert_eq!(settings.env, [("A".to_string(), "1".to_string())]);
        assert_eq!(settings.abbrs, [("t".to_string(), "make test".to_string())]);
        assert!(parse("[env]\n\"no name\" = \"1\"\n").is_err());
        assert!(parse("[run]\nx = \"rm -rf /\"\n").is_err());
        assert!(parse("[env]\nA = 1\n").is_err());
    }

    #[test]
    fn leaving_undoes_what_was_set() {
        let dir = std::env::temp_dir().join(format!("nukesh-project-{}", std::process::id()));
        let inner = dir.join("site/src");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(
            dir.join("site").join(FILE),
            "[env]\nSITE = \"yes\"\n[abbr]\nt = \"make test\"\n",
        )
        .unwrap();
        let mut shell = Shell::new("test", false);
        shell.set_var("HOME", dir.to_str().unwrap());
        shell.set_var("PWD", inner.to_str().unwrap());
        // Not trusted: nothing.
        before_prompt(&mut shell);
        assert_eq!(shell.get("SITE"), None);
        let _ = trust(&mut shell, &["trust".into()]);
        assert_eq!(shell.get("SITE").as_deref(), Some("yes"));
        assert!(shell.abbrs.contains_key("t"));
        shell.set_var("PWD", dir.to_str().unwrap());
        before_prompt(&mut shell);
        assert_eq!(shell.get("SITE"), None);
        assert!(!shell.abbrs.contains_key("t"));
        // A changed file is not trusted.
        std::fs::write(dir.join("site").join(FILE), "[env]\nSITE = \"other\"\n").unwrap();
        shell.set_var("PWD", inner.to_str().unwrap());
        before_prompt(&mut shell);
        assert_eq!(shell.get("SITE"), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
