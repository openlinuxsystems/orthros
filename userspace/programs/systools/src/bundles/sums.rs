// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A bundle's list of its files (ADR 0222): `files.sha256`, each file's
//! SHA-256 and path, a line each, sorted by path, as `sha256sum` writes and
//! `sha256sum -c` checks them. The signature signs this list, so it signs
//! every file through it, the manifest among them.

use std::fmt::Write as _;

use manifest::bundle::check_bundle_path;
use sha2::{Digest, Sha256};

/// The list's name in a bundle.
pub const LIST: &str = "files.sha256";
/// The signature's name in a bundle: minisign's signature of the list.
pub const SIGNATURE: &str = "signature";
/// The longest path of a file in a bundle, and of each of its parts, as
/// Linux's PATH_MAX and NAME_MAX.
pub const PATH_MAX: usize = 4095;
pub const NAME_MAX: usize = 255;
/// The most files a bundle holds.
pub const FILES_MAX: usize = 1 << 20;

/// What the list says about one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub sha256: [u8; 32],
}

/// The SHA-256 of a file, as it is read.
#[derive(Clone, Default)]
pub struct Hasher(Sha256);

impl Hasher {
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

/// The SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// `bytes` in lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Checks a path a bundle holds a file at: one of the bundle's paths
/// (manifest.json(5)), within Linux's limits, and not the list or the
/// signature themselves.
pub fn check_path(path: &str) -> Result<(), String> {
    check_bundle_path(path)?;
    if path.len() > PATH_MAX || path.split('/').any(|part| part.len() > NAME_MAX) {
        return Err(format!(
            "`{}` is longer than a path may be: {PATH_MAX} bytes, {NAME_MAX} for each part",
            path.escape_debug()
        ));
    }
    Ok(())
}

/// The list's text for `entries`, which it sorts.
pub fn write(entries: &mut [Entry]) -> String {
    entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut text = String::new();
    for entry in entries.iter() {
        let _ = writeln!(text, "{}  {}", hex(&entry.sha256), entry.path);
    }
    text
}

/// Reads the list: every file once, sorted, with paths a bundle may have.
/// Neither the list nor the signature is in it.
pub fn read(text: &str) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = Vec::new();
    for (number, line) in text.split_terminator('\n').enumerate() {
        let line_no = number + 1;
        let bad = |why: &str| format!("{LIST}, line {line_no}: {why}");
        let (hash, path) = line
            .split_once("  ")
            .ok_or_else(|| bad("not a SHA-256 and a path, two spaces apart"))?;
        if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(bad("not a SHA-256 in lowercase hex"));
        }
        let mut sha256 = [0u8; 32];
        for (i, byte) in sha256.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hash[2 * i..2 * i + 2], 16).unwrap_or_default();
        }
        check_path(path).map_err(|why| bad(&why))?;
        if path == LIST || path == SIGNATURE {
            return Err(bad(&format!("the list does not list {path}")));
        }
        if let Some(last) = entries.last()
            && last.path.as_bytes() >= path.as_bytes()
        {
            return Err(bad(if last.path == path {
                "a file listed twice"
            } else {
                "not in order: the paths are sorted, as bytes"
            }));
        }
        if entries.len() == FILES_MAX {
            return Err(bad(&format!("more than {FILES_MAX} files")));
        }
        entries.push(Entry { path: path.into(), sha256 });
    }
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(format!("{LIST}: its last line has no line break"));
    }
    // A file cannot also be a folder of others.
    for pair in entries.windows(2) {
        if pair[1].path.strip_prefix(&pair[0].path).is_some_and(|rest| rest.starts_with('/')) {
            return Err(format!("{LIST}: {} is a file and a folder", pair[0].path));
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_what_sha256sum_writes() {
        let mut entries = vec![
            Entry { path: "manifest.json".into(), sha256: sha256(b"{}\n") },
            Entry { path: "bin/hello world".into(), sha256: sha256(b"") },
        ];
        let text = write(&mut entries);
        assert_eq!(
            text,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  bin/hello world\n\
             ca3d163bab055381827226140568f3bef7eaac187cebd76878e0b63e9e442356  manifest.json\n"
        );
        assert_eq!(read(&text).unwrap(), entries);
        assert_eq!(read("").unwrap(), []);
    }

    #[test]
    fn a_list_that_is_not_one_is_refused() {
        let hash = hex(&sha256(b""));
        let cases = [
            (format!("{hash} a\n"), "two spaces"),
            (format!("{}  a\n", hash.to_uppercase()), "lowercase hex"),
            (format!("{}  a\n", &hash[1..]), "lowercase hex"),
            (format!("{hash}  /etc/passwd\n"), "not a path in the bundle"),
            (format!("{hash}  a/../../b\n"), "not a path in the bundle"),
            (format!("{hash}  a\\b\n"), "not a path in the bundle"),
            (format!("{hash}  a\u{1b}[2J\n"), "not a path in the bundle"),
            (format!("{hash}  signature\n"), "does not list"),
            (format!("{hash}  b\n{hash}  a\n"), "not in order"),
            (format!("{hash}  a\n{hash}  a\n"), "twice"),
            (format!("{hash}  a\n{hash}  a/b\n"), "a file and a folder"),
            (format!("{hash}  a"), "no line break"),
            (format!("{hash}  {}\n", "x".repeat(256)), "longer than"),
        ];
        for (text, why) in cases {
            let error = read(&text).unwrap_err();
            assert!(error.contains(why), "{text:?}: {error}");
        }
    }
}
