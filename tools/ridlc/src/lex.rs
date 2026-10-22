// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Splitting a `.ridl` file into tokens.

/// A token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tok {
    /// A name or keyword.
    Ident(String),
    /// A number, decimal or `0x` hexadecimal, with `_` allowed.
    Int(u64),
    /// A `///` line: documentation of what follows.
    Doc(String),
    /// A `//!` line: documentation of the module.
    ModuleDoc(String),
    /// `->`.
    Arrow,
    /// `=>`.
    FatArrow,
    /// A single punctuation character.
    Punct(char),
}

/// A token and its line.
#[derive(Clone, Debug)]
pub struct Token {
    /// The token.
    pub tok: Tok,
    /// Its line, from 1.
    pub line: usize,
}

/// Splits `source` into tokens, or says where it cannot.
pub fn tokens(source: &str) -> Result<Vec<Token>, (usize, String)> {
    let mut out = Vec::new();
    for (index, text) in source.lines().enumerate() {
        let line = index + 1;
        let mut rest = text;
        loop {
            rest = rest.trim_start();
            if rest.is_empty() {
                break;
            }
            if let Some(doc) = rest.strip_prefix("///") {
                out.push(Token { tok: Tok::Doc(strip_one_space(doc)), line });
                break;
            }
            if let Some(doc) = rest.strip_prefix("//!") {
                out.push(Token { tok: Tok::ModuleDoc(strip_one_space(doc)), line });
                break;
            }
            if rest.starts_with("//") {
                break;
            }
            if let Some(after) = rest.strip_prefix("->") {
                out.push(Token { tok: Tok::Arrow, line });
                rest = after;
                continue;
            }
            if let Some(after) = rest.strip_prefix("=>") {
                out.push(Token { tok: Tok::FatArrow, line });
                rest = after;
                continue;
            }
            let first = rest.chars().next().unwrap_or(' ');
            if first.is_ascii_alphabetic() || first == '_' {
                let end = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len());
                out.push(Token { tok: Tok::Ident(rest[..end].to_string()), line });
                rest = &rest[end..];
            } else if first.is_ascii_digit() {
                let end = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len());
                let literal = rest[..end].replace('_', "");
                let value = match literal.strip_prefix("0x") {
                    Some(hex) => u64::from_str_radix(hex, 16),
                    None => literal.parse(),
                };
                let Ok(value) = value else {
                    return Err((line, format!("`{}` is not a number", &rest[..end])));
                };
                out.push(Token { tok: Tok::Int(value), line });
                rest = &rest[end..];
            } else if "{}()[];:,=<>".contains(first) {
                out.push(Token { tok: Tok::Punct(first), line });
                rest = &rest[1..];
            } else {
                return Err((line, format!("unexpected character `{first}`")));
            }
        }
    }
    Ok(out)
}

fn strip_one_space(text: &str) -> String {
    text.strip_prefix(' ').unwrap_or(text).trim_end().to_string()
}
