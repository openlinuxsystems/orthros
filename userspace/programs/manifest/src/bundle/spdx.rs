// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! SPDX license expressions, as `license` holds them (SPDX 2.3, annex D):
//! license identifiers joined by `AND` and `OR`, an exception after
//! `WITH`, and parentheses.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A token of an expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token<'a> {
    Open,
    Close,
    And,
    Or,
    With,
    Id(&'a str),
}

fn tokens(text: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    for word in text.split_ascii_whitespace() {
        let mut rest = word;
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix('(') {
                out.push(Token::Open);
                rest = after;
                continue;
            }
            let end = rest.find([')', '(']).unwrap_or(rest.len());
            if end > 0 {
                out.push(match &rest[..end] {
                    "AND" | "and" => Token::And,
                    "OR" | "or" => Token::Or,
                    "WITH" | "with" => Token::With,
                    id => Token::Id(id),
                });
                rest = &rest[end..];
            } else if let Some(after) = rest.strip_prefix(')') {
                out.push(Token::Close);
                rest = after;
            }
        }
    }
    out
}

/// Whether `id` is written as a license or exception identifier may be:
/// letters, digits, `.` and `-`, with a `+` at the end of a license's
/// for "or later", or a `LicenseRef-` of the publisher's own.
fn id_ok(id: &str, exception: bool) -> bool {
    let id = if exception { id } else { id.strip_suffix('+').unwrap_or(id) };
    let id = id
        .strip_prefix("DocumentRef-")
        .and_then(|rest| rest.split_once(":LicenseRef-"))
        .map_or(id, |(doc, rest)| if doc.is_empty() { "" } else { rest });
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

struct Parser<'a> {
    tokens: Vec<Token<'a>>,
    at: usize,
    ids: Vec<&'a str>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<Token<'a>> {
        self.tokens.get(self.at).copied()
    }

    fn next(&mut self) -> Option<Token<'a>> {
        let token = self.peek();
        self.at += 1;
        token
    }

    /// `or := and ("OR" and)*`
    fn or(&mut self) -> Result<(), String> {
        self.and()?;
        while self.peek() == Some(Token::Or) {
            self.at += 1;
            self.and()?;
        }
        Ok(())
    }

    /// `and := with ("AND" with)*`
    fn and(&mut self) -> Result<(), String> {
        self.with()?;
        while self.peek() == Some(Token::And) {
            self.at += 1;
            self.with()?;
        }
        Ok(())
    }

    /// `with := "(" or ")" | id ("WITH" exception)?`
    fn with(&mut self) -> Result<(), String> {
        match self.next() {
            Some(Token::Open) => {
                self.or()?;
                match self.next() {
                    Some(Token::Close) => Ok(()),
                    _ => Err("a `(` is not closed".into()),
                }
            }
            Some(Token::Id(id)) if id_ok(id, false) => {
                self.ids.push(id);
                if self.peek() == Some(Token::With) {
                    self.at += 1;
                    match self.next() {
                        Some(Token::Id(exception)) if id_ok(exception, true) => {}
                        _ => return Err("`WITH` takes an exception's identifier".into()),
                    }
                }
                Ok(())
            }
            Some(Token::Id(id)) => Err(format!("`{id}` is not a license identifier")),
            Some(Token::Close) => Err("a `)` closes nothing".into()),
            Some(Token::And | Token::Or | Token::With) => {
                Err("an operator stands where a license should be".into())
            }
            None => Err("it ends where a license should be".into()),
        }
    }
}

/// Checks the expression `text`, and gives the license identifiers it
/// names; an error says what is wrong in plain words.
pub fn check(text: &str) -> Result<Vec<&str>, String> {
    let mut parser = Parser { tokens: tokens(text), at: 0, ids: Vec::new() };
    if parser.tokens.is_empty() {
        return Err("license is empty: write an SPDX expression, such as MIT".into());
    }
    parser.or()?;
    if parser.at < parser.tokens.len() {
        return Err(match parser.tokens[parser.at] {
            Token::Close => "a `)` closes nothing".into(),
            _ => "two licenses need `AND` or `OR` between them".into(),
        });
    }
    Ok(parser.ids)
}

/// Whether license `id` is one of the GNU licenses whose bundles link to
/// their exact source (`source`).
pub fn needs_source(id: &str) -> bool {
    ["GPL-", "LGPL-", "AGPL-"].iter().any(|family| id.starts_with(family))
}
