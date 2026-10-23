// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Paths: what a program names files with, beneath a directory.
//!
//! A path is UTF-8 of at most [`PATH_MAX`] bytes without NULs, and never
//! absolute: it is resolved beneath the directory capability it comes
//! with, so a leading `/` is refused ([`Error::Outside`]). It is a
//! sequence of components separated by `/` (repeated slashes count as
//! one, and a trailing one is ignored): `.` stays, `..` goes back one
//! component, and any other is a name of at most [`NAME_MAX`] bytes. The
//! empty path, or `.`, is the directory itself.
//!
//! Resolution (`mount::Vfs::resolve`) keeps the chain of directories it
//! walked through, so `..` returns to the previous one and can never go
//! above where the walk started: `a/../../b` is refused, wherever the
//! directory is in the tree, but from the root `..` stays at the root,
//! as `/..` is `/`. A symbolic link on the way puts its target in its
//! place; a target that starts with `/` starts again from where the walk
//! did, so links cannot lead out of a directory either.

use crate::{Error, NAME_MAX, PATH_MAX, Result};

/// A component of a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Component<'a> {
    /// `.`: the same directory.
    Current,
    /// `..`: the directory before.
    Parent,
    /// A name.
    Name(&'a str),
}

/// Checks a path a program gave as bytes: UTF-8, short enough, without
/// NULs, not absolute.
pub fn parse(bytes: &[u8]) -> Result<&str> {
    if bytes.len() > PATH_MAX {
        return Err(Error::NameTooLong);
    }
    // One loop finds a NUL and whether the path is ASCII, as most are:
    // then it needs no UTF-8 decoding, whose set-up alone costs more than
    // checking a short name.
    let mut high = 0;
    for &byte in bytes {
        if byte == 0 {
            return Err(Error::BadName);
        }
        high |= byte;
    }
    let path = if high < 0x80
        && let Some(ascii) = bytes.as_ascii()
    {
        ascii.as_str()
    } else {
        core::str::from_utf8(bytes).map_err(|_| Error::BadName)?
    };
    if path.starts_with('/') {
        return Err(Error::Outside);
    }
    Ok(path)
}

/// Checks a name to create or look up: not empty, not `.` or `..`,
/// without `/` or NUL, at most [`NAME_MAX`] bytes.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(Error::BadName);
    }
    if name.len() > NAME_MAX {
        return Err(Error::NameTooLong);
    }
    Ok(())
}

/// The components of `path`, which [`parse`] checked, each name checked:
/// what [`check_name`] would check, but that splitting on `/` and
/// [`parse`] have already ruled out a `/` or a NUL in it.
pub fn components(path: &str) -> Components<'_> {
    Components { rest: path }
}

/// The iterator [`components`] returns. It looks for each `/` itself, a
/// byte at a time: `str::split`'s general searcher costs more than the
/// short names of a path.
pub struct Components<'a> {
    rest: &'a str,
}

impl<'a> Components<'a> {
    /// What follows the component last returned: the `/` after it and the
    /// rest of the path, or nothing at the end.
    pub fn rest(&self) -> &'a str {
        self.rest
    }

    /// Whether the component last returned was the path's last: only
    /// slashes, if anything, follow it.
    pub fn at_end(&self) -> bool {
        self.rest.bytes().all(|b| b == b'/')
    }

    /// Whether the component last returned was the last and a `/` follows
    /// it, which makes a symbolic link there followed (POSIX.1-2024 XBD
    /// 4.16) and a file there not a directory.
    pub fn trailing_slash(&self) -> bool {
        !self.rest.is_empty() && self.at_end()
    }
}

impl<'a> Iterator for Components<'a> {
    type Item = Result<Component<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        // `/` is one byte of UTF-8, so both sides of one are text.
        let start = self.rest.bytes().position(|b| b != b'/')?;
        let tail = &self.rest[start..];
        let end = tail.bytes().position(|b| b == b'/').unwrap_or(tail.len());
        let (part, rest) = tail.split_at(end);
        // The `/` stays, so that `rest` shows a trailing one.
        self.rest = rest;
        Some(match part {
            "." => Ok(Component::Current),
            ".." => Ok(Component::Parent),
            name if name.len() > NAME_MAX => Err(Error::NameTooLong),
            name => Ok(Component::Name(name)),
        })
    }
}

/// Splits `path` into the path of its directory and its last name, for
/// operations that make or remove an entry: `a/b/c` is `("a/b", "c")`.
/// Fails if the last component is not a name (`a/..`, `.`, the empty
/// path).
pub fn split_last(path: &str) -> Result<(&str, &str)> {
    let trimmed = path.trim_end_matches('/');
    let (dir, last) = match trimmed.rfind('/') {
        Some(at) => (&trimmed[..at], &trimmed[at + 1..]),
        None => ("", trimmed),
    };
    check_name(last)?;
    Ok((dir, last))
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// A byte that matters to paths: `/`, `.`, a letter, NUL, or one that
    /// is never UTF-8. Symbolic UTF-8 decoding of every byte value is too
    /// slow for CBMC, and the others behave as the letter does.
    fn path_byte() -> u8 {
        let byte: u8 = kani::any();
        kani::assume(matches!(byte, b'/' | b'.' | b'a' | 0 | 0xff));
        byte
    }

    /// No input makes [`parse`] panic, and a path it accepts is never
    /// absolute and holds no NUL.
    #[kani::proof]
    #[kani::unwind(5)]
    fn parsing_never_panics() {
        let bytes: [u8; 3] = [path_byte(), path_byte(), path_byte()];
        let len: usize = kani::any_where(|&l: &usize| l <= 3);
        if let Ok(path) = parse(&bytes[..len]) {
            assert!(!path.starts_with('/'));
            assert!(!path.as_bytes().contains(&0));
        }
    }

    /// Every name [`split_last`] yields is one a directory may hold, and
    /// the directory part is shorter than the path.
    #[kani::proof]
    #[kani::unwind(5)]
    fn split_names_are_valid() {
        let bytes: [u8; 3] = [path_byte(), path_byte(), path_byte()];
        let len: usize = kani::any_where(|&l: &usize| l <= 3);
        kani::assume(bytes[..len].iter().all(|&b| b != 0 && b != 0xff));
        let Ok(path) = core::str::from_utf8(&bytes[..len]) else { return };
        if let Ok((dir, last)) = split_last(path) {
            assert!(!last.is_empty() && last != "." && last != "..");
            assert!(!last.as_bytes().contains(&b'/'));
            assert!(dir.len() < path.len());
        }
    }
}
