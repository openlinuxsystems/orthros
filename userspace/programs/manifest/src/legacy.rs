// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The service files of Phases 9 to 12 (ADR 0101), `NAME.service` with
//! lines of `key = value`, and their conversion to TOML (ADR 0216).
//!
//! Spark converts each one it finds into `NAME.toml` and keeps the old
//! file beside it as `NAME.service.old`. The conversion keeps every
//! comment, with the setting it is above; the settings that TOML puts in
//! tables (`weight`, `memory` and `oom` in `[limits]`, `environment`, and
//! `network.*` in `[permissions]`) move there with their comments.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::Error;
use crate::service::{self, VERSION};

/// The tables a converted setting goes in, in the order they are written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    Top,
    Limits,
    Environment,
    Permissions,
}

impl Table {
    const ALL: [Table; 4] = [Table::Top, Table::Limits, Table::Environment, Table::Permissions];

    fn header(self) -> Option<&'static str> {
        match self {
            Table::Top => None,
            Table::Limits => Some("[limits]"),
            Table::Environment => Some("[environment]"),
            Table::Permissions => Some("[permissions]"),
        }
    }
}

/// Converts an old service file to TOML: the new file's text, which
/// [`service::from_toml`] reads as the old file meant. An old file with an
/// error is not converted, and the error is its line's.
pub fn convert(text: &str) -> Result<String, Error> {
    // Each table's lines, each with the old key it came from, if any; a
    // key set twice keeps its last value, as the old reader did.
    let mut tables: [Vec<(Option<&str>, String)>; 4] = Default::default();
    let mut header: Vec<&str> = Vec::new();
    let mut pending: Vec<&str> = Vec::new();
    let mut first = true;
    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            pending.push(line);
            continue;
        }
        let at = |message: String| Error::at(number + 1, message);
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(at(format!("`{trimmed}` is not `key = value`")));
        };
        let (key, value) = (key.trim(), value.trim());
        let (table, lines) = setting(key, value).map_err(at)?;
        if first {
            // The file's own comment is what comes before the last blank
            // line above its first setting.
            let cut = pending.iter().rposition(|l| l.trim().is_empty()).map_or(0, |i| i + 1);
            header.extend(pending.drain(..cut));
            first = false;
        }
        let lines_of = &mut tables[table as usize];
        lines_of.retain(|(k, _)| *k != Some(key));
        lines_of.extend(pending.drain(..).map(|l| (None, l.to_string())));
        lines_of.extend(lines.into_iter().map(|l| (Some(key), l)));
    }
    let mut out = String::new();
    for line in &header {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("service_version = {VERSION}\n"));
    for table in Table::ALL {
        let lines = &tables[table as usize];
        // Blank lines at a table's ends are the tables' own to write.
        let start = lines.iter().position(|(_, l)| !l.trim().is_empty()).unwrap_or(lines.len());
        let end = lines.iter().rposition(|(_, l)| !l.trim().is_empty()).map_or(start, |i| i + 1);
        if start == end {
            continue;
        }
        if let Some(header) = table.header() {
            out.push('\n');
            out.push_str(header);
            out.push('\n');
        }
        for (_, line) in &lines[start..end] {
            out.push_str(line);
            out.push('\n');
        }
    }
    let trailing: Vec<&str> = pending.into_iter().skip_while(|l| l.trim().is_empty()).collect();
    if !trailing.is_empty() {
        out.push('\n');
        for line in trailing {
            out.push_str(line);
            out.push('\n');
        }
    }
    // What it wrote must read back; an old file the old reader took
    // always does.
    service::from_toml(&out)?;
    Ok(out)
}

/// The TOML lines for the old setting `key = value`, and their table.
fn setting(key: &str, value: &str) -> Result<(Table, Vec<String>), String> {
    let one = |table, line: String| Ok((table, Vec::from([line])));
    match key {
        "description" => one(Table::Top, format!("description = {}", quote(value))),
        "program" => {
            if !value.starts_with('/') {
                return Err(format!("the program `{value}` is not an absolute path"));
            }
            one(Table::Top, format!("program = {}", quote(value)))
        }
        "arguments" => {
            let words: Vec<String> = words(value).iter().map(|w| quote(w)).collect();
            one(Table::Top, format!("arguments = [{}]", words.join(", ")))
        }
        "order" => {
            let order: u32 = value.parse().map_err(|_| format!("`{value}` is not a number"))?;
            one(Table::Top, format!("order = {order}"))
        }
        "restart" => match value {
            "always" | "on-failure" | "never" => one(Table::Top, format!("restart = \"{value}\"")),
            _ => Err("restart is always, on-failure or never".into()),
        },
        "enabled" => one(Table::Top, format!("enabled = {}", yes_no(value)?)),
        "weight" => {
            let weight: u32 = value.parse().map_err(|_| format!("`{value}` is not a number"))?;
            one(Table::Limits, format!("cpu_weight = {weight}"))
        }
        "memory" if value.is_empty() || value == "none" => {
            one(Table::Limits, "memory = \"none\"".into())
        }
        "memory" => match service::size(value) {
            Some(_) => one(Table::Limits, format!("memory = {}", quote(value))),
            None => Err(format!("`{value}` is not a size such as 64M")),
        },
        "oom" => match value {
            "service" | "app" => one(Table::Limits, format!("oom = \"{value}\"")),
            _ => Err("oom is service or app".into()),
        },
        "environment" => {
            let mut lines = Vec::new();
            for word in words(value) {
                let Some((name, value)) = word.split_once('=') else {
                    return Err(format!("`{word}` is not NAME=value"));
                };
                lines.push(format!("{} = {}", bare(name), quote(value)));
            }
            Ok((Table::Environment, lines))
        }
        "network.outbound" => {
            one(Table::Permissions, format!("network.outbound = {}", yes_no(value)?))
        }
        "network.listen" => {
            let mut items = Vec::new();
            for item in value.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
                match item.parse::<u16>() {
                    _ if item.is_empty() => {}
                    Ok(port) => items.push(port.to_string()),
                    Err(_) => items.push(quote(item)),
                }
            }
            one(Table::Permissions, format!("network.listen = [{}]", items.join(", ")))
        }
        "admin.hardware" => one(Table::Permissions, format!("admin.hardware = {}", yes_no(value)?)),
        _ => Err(format!("unknown key `{key}`")),
    }
}

fn yes_no(value: &str) -> Result<bool, String> {
    match value {
        "yes" | "true" | "on" => Ok(true),
        "no" | "false" | "off" => Ok(false),
        _ => Err(format!("`{value}` is neither yes nor no")),
    }
}

/// Splits `text` into words at blanks, a pair of double quotes keeping
/// blanks in a word, as the old reader did.
fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut any) = (false, false);
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            ' ' | '\t' if !quoted => {
                if any {
                    words.push(core::mem::take(&mut word));
                    any = false;
                }
            }
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

/// `text` as a TOML string, in double quotes.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `name` as a TOML key: bare if it may be, else quoted.
fn bare(name: &str) -> String {
    let plain = !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if plain { name.to_string() } else { quote(name) }
}
