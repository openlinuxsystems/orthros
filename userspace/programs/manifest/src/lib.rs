// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Dukovo's files that describe programs, read the same way by everything
//! that reads them (ADR 0216): service files (service.toml(5)), which
//! Spark and the service command read, and from Phase 13d the manifests
//! of bundles, whose services section is the same schema.
//!
//! People write TOML, with comments; tools write JSON. Both come through
//! the same types, so the two can never drift apart. An error says which
//! line is wrong and why, in plain words.

#![no_std]

extern crate alloc;

pub mod legacy;
pub mod service;
#[cfg(test)]
mod tests;

use alloc::format;
use alloc::string::String;
use core::fmt;

/// What is wrong with a file, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The line, from 1, when the error is on one.
    pub line: Option<usize>,
    /// What is wrong, in plain words.
    pub message: String,
}

impl Error {
    /// An error on no line in particular.
    pub fn new(message: impl Into<String>) -> Error {
        Error { line: None, message: message.into() }
    }

    /// An error on line `line`.
    pub fn at(line: usize, message: impl Into<String>) -> Error {
        Error { line: Some(line), message: message.into() }
    }

    /// The error `toml` found in `text`, with its line, and its message in
    /// the words service.toml(5) uses.
    fn from_toml(text: &str, err: &toml::de::Error) -> Error {
        let line = err
            .span()
            // A whole table's span is where the table starts: for the
            // file itself, line 1, which says nothing.
            .filter(|span| span.start > 0 || span.end < text.len())
            .map(|span| line_of(text, span.start));
        Error { line, message: plain(err.message()) }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// The line, from 1, that byte `at` of `text` is on.
fn line_of(text: &str, at: usize) -> usize {
    let at = at.min(text.len());
    text.as_bytes()[..at].iter().filter(|&&b| b == b'\n').count() + 1
}

/// `message` from serde or toml, in plain words: on one line, with Rust's
/// names of types as service.toml(5) says them.
fn plain(message: &str) -> String {
    if let Some(rest) = message.strip_prefix("unknown field `")
        && let Some((name, _)) = rest.split_once('`')
    {
        return format!("`{name}` is not a setting here; service.toml(5) lists them");
    }
    let mut text = message.trim().replace('\n', "; ");
    for (rust, words) in [
        ("expected u32", "expected a whole number"),
        ("expected u16", "expected a whole number"),
        ("expected a boolean", "expected true or false"),
        ("expected a sequence", "expected a list such as [\"a\", \"b\"]"),
        ("expected a string", "expected text in double quotes"),
        ("expected a map", "expected a table"),
    ] {
        text = text.replace(rust, words);
    }
    text
}
