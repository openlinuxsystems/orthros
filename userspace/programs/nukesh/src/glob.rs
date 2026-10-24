// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Pathname expansion: `*.c`, `src/*/mod.rs`, and with `globstar`, `**`.
//!
//! The pattern is split at its slashes; a part with nothing special in it
//! is taken as it is, and a part with a pattern is matched against the
//! names in the directory reached so far. Names that start with a dot need
//! a dot in the pattern (unless `dotglob`), and `.` and `..` match only
//! themselves, unless `dots` (as in dash, where `.*` matches them too).
//! The matches are sorted by their bytes, as in the C locale.

use std::os::unix::ffi::OsStrExt;

use crate::pattern::{self, Options, Pattern};
use crate::text;

#[derive(Clone, Copy, Debug, Default)]
pub struct GlobOptions {
    pub extglob: bool,
    pub nocase: bool,
    pub dotglob: bool,
    pub globstar: bool,
    /// A pattern that starts with a dot matches `.` and `..` too.
    pub dots: bool,
}

/// The paths `pattern` matches, sorted; none if it matches nothing.
pub fn glob(pattern: &str, options: GlobOptions) -> Vec<String> {
    let absolute = pattern.starts_with('/');
    let parts: Vec<&str> = split_slashes(pattern);
    let trailing_slash = pattern.ends_with('/') && pattern.len() > 1;
    let mut paths: Vec<String> = vec![if absolute { "/".into() } else { String::new() }];
    let mut literal_tail = false;
    let count = parts.len();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let last = i + 1 == count;
        let mut next = Vec::new();
        if options.globstar && *part == "**" {
            for path in &paths {
                if last {
                    // `**` at the end matches every file and directory.
                    walk(path, options, false, &mut next);
                } else {
                    next.push(path.clone());
                    walk(path, options, true, &mut next);
                }
            }
            if last {
                for path in &mut next {
                    if path.len() > 1 {
                        *path = path.trim_end_matches('/').to_string();
                    }
                }
            }
            paths = next;
            literal_tail = false;
            continue;
        }
        if !pattern::has_meta(part, options.extglob) {
            let name = pattern::unescape(part);
            for path in &paths {
                next.push(join(path, &name, last && !trailing_slash));
            }
            paths = next;
            literal_tail = true;
            continue;
        }
        literal_tail = false;
        let matcher = Pattern::new(
            part,
            Options {
                extglob: options.extglob,
                nocase: options.nocase,
                file_name: true,
                dotglob: options.dotglob,
            },
        );
        let explicit_dot = part.starts_with('.') || part.starts_with("\\.");
        for path in &paths {
            let dir = if path.is_empty() { "." } else { path.as_str() };
            let Ok(entries) = std::fs::read_dir(text::to_os(dir)) else { continue };
            let mut names: Vec<String> = Vec::new();
            for entry in entries.flatten() {
                names.push(text::decode(entry.file_name().as_bytes()));
            }
            if explicit_dot {
                // read_dir leaves these out, but a pattern can still name
                // them when written out, or with `dots` match them.
                for dots in [".", ".."] {
                    if part == &dots || options.dots {
                        names.push(dots.to_string());
                    }
                }
            }
            for name in names {
                if !matcher.matches(&name) {
                    continue;
                }
                let full = join(path, &name, last && !trailing_slash);
                if (!last || trailing_slash) && !is_dir(&full) {
                    continue;
                }
                next.push(full);
            }
        }
        paths = next;
    }
    if literal_tail {
        paths.retain(|p| exists(p.trim_end_matches('/')) || p == "/");
    }
    if trailing_slash {
        paths.retain(|p| is_dir(p));
    }
    paths.retain(|p| !p.is_empty());
    paths.sort_by_key(|p| text::encode(p));
    paths.dedup();
    paths
}

/// The parts of a pattern between unquoted slashes.
fn split_slashes(pattern: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'/' => {
                parts.push(&pattern[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&pattern[start..]);
    parts
}

fn join(path: &str, name: &str, last: bool) -> String {
    let mut out = String::with_capacity(path.len() + name.len() + 1);
    out.push_str(path);
    out.push_str(name);
    if !last {
        out.push('/');
    }
    out
}

fn is_dir(path: &str) -> bool {
    std::fs::metadata(text::to_os(path.trim_end_matches('/'))).is_ok_and(|m| m.is_dir())
        || path == "/"
}

fn exists(path: &str) -> bool {
    std::fs::symlink_metadata(text::to_os(path)).is_ok()
}

/// What is below `path`, for `**`: directories with a trailing slash,
/// and unless `dirs_only`, files too.
fn walk(path: &str, options: GlobOptions, dirs_only: bool, out: &mut Vec<String>) {
    let dir = if path.is_empty() { "." } else { path };
    let Ok(entries) = std::fs::read_dir(text::to_os(dir)) else { return };
    let mut names: Vec<String> =
        entries.flatten().map(|e| text::decode(e.file_name().as_bytes())).collect();
    names.sort();
    for name in names {
        if name.starts_with('.') && !options.dotglob {
            continue;
        }
        let full = format!("{path}{name}");
        let Ok(meta) = std::fs::symlink_metadata(text::to_os(&full)) else { continue };
        if meta.is_dir() {
            let full = full + "/";
            out.push(full.clone());
            walk(&full, options, dirs_only, out);
        } else if !dirs_only {
            out.push(full);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nukesh-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for path in ["a.c", "b.c", "c.h", ".hidden.c", "sub/x.c", "sub/deep/y.c", "sub2/z.h"] {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        dir
    }

    fn run(dir: &std::path::Path, pattern: &str, options: GlobOptions) -> Vec<String> {
        let base = format!("{}/", dir.display());
        glob(&format!("{base}{pattern}"), options)
            .into_iter()
            .map(|p| p.strip_prefix(&base).unwrap().to_string())
            .collect()
    }

    #[test]
    fn expands_patterns() {
        let dir = tree();
        let plain = GlobOptions::default();
        assert_eq!(run(&dir, "*.c", plain), ["a.c", "b.c"]);
        assert_eq!(run(&dir, "*/*.c", plain), ["sub/x.c"]);
        assert_eq!(run(&dir, "sub*/", plain), ["sub/", "sub2/"]);
        assert_eq!(run(&dir, ".*.c", plain), [".hidden.c"]);
        assert_eq!(run(&dir, "[ab].c", plain), ["a.c", "b.c"]);
        assert_eq!(run(&dir, "*/x.c", plain), ["sub/x.c"]);
        assert!(run(&dir, "*.none", plain).is_empty());
        assert!(run(&dir, "*/nothing", plain).is_empty());
        let dot = GlobOptions { dotglob: true, ..plain };
        assert_eq!(run(&dir, "*.c", dot), [".hidden.c", "a.c", "b.c"]);
        let star = GlobOptions { globstar: true, ..plain };
        assert_eq!(run(&dir, "**/*.c", star), ["a.c", "b.c", "sub/deep/y.c", "sub/x.c"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn splits_at_unquoted_slashes() {
        assert_eq!(split_slashes("a/b\\/c/d"), ["a", "b\\/c", "d"]);
        assert_eq!(split_slashes("/x"), ["", "x"]);
    }
}
