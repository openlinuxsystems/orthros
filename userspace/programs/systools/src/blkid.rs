// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! blkid(8): what each disk and partition holds, as util-linux's blkid
//! prints it from libblkid: a filesystem's TYPE, LABEL and UUID, a
//! disk's partition table, a partition's PARTUUID and PARTLABEL.

use std::io::{self, Write};

use crate::args::{Arg, Args};
use crate::probe::{self, BLOCKS};
use crate::util::{self, Fail, usage};

/// The exit status when nothing was found, as util-linux's.
const NOT_FOUND: i32 = 2;

/// How the tags are printed (`-o`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Output {
    /// `/devices/sda1: UUID="..." TYPE="ext4"`.
    Full,
    /// Each value on a line of its own.
    Value,
    /// `NAME=value` lines, a blank line after each device.
    Export,
    /// The device's path alone.
    Device,
}

/// A device's tags, in libblkid's order.
pub fn tags(name: &str) -> Vec<(&'static str, String)> {
    let mut tags = Vec::new();
    let Some(mut read) = probe::reader(name) else { return tags };
    if let Some(fs) = probe::filesystem(&mut read) {
        tags.extend(fs.label.map(|label| ("LABEL", label)));
        tags.extend(fs.uuid.map(|uuid| ("UUID", uuid)));
        tags.extend(fs.uuid_sub.map(|uuid| ("UUID_SUB", uuid)));
        tags.extend(fs.block_size.map(|size| ("BLOCK_SIZE", size.to_string())));
        tags.push(("TYPE", fs.kind.to_string()));
    }
    let block = |disk: &str| {
        probe::block_text(disk, "queue/logical_block_size").and_then(|n| n.parse().ok())
    };
    match probe::block_text(name, "partition").and_then(|n| n.parse::<u32>().ok()) {
        Some(number) => {
            let disk = probe::disk_of(name);
            if let (Some(mut read), Some(size)) = (probe::reader(disk), block(disk)) {
                let ids = probe::partition_ids(&mut read, size, number);
                tags.extend(ids.label.map(|label| ("PARTLABEL", label)));
                tags.extend(ids.uuid.map(|uuid| ("PARTUUID", uuid)));
            }
        }
        // A disk with a filesystem on the whole of it has no table.
        None if tags.is_empty() => {
            if let Some(table) = block(name).and_then(|size| probe::table(&mut read, size)) {
                tags.push(("PTUUID", table.uuid));
                tags.push(("PTTYPE", table.kind.to_string()));
            }
        }
        None => {}
    }
    tags
}

/// A value as blkid quotes it: `"` and `\` after a backslash.
fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// A value as `-o export` writes it, for a shell: what is not a letter,
/// digit or `-_.:/,` after a backslash.
fn exported(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if !(c.is_ascii_alphanumeric() || "-_.:/,+".contains(c)) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let (mut output, mut wanted, mut names) = (Output::Full, Vec::new(), Vec::new());
    // The tag a device must have, and whether only its path is wanted.
    let (mut find, mut lookup): (Option<(String, String)>, bool) = (None, false);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('o') => output = output_of(&args.text()?)?,
            Arg::Long(long) if long == "output" => output = output_of(&args.text()?)?,
            Arg::Short('s') => wanted.push(args.text()?),
            Arg::Long(long) if long == "match-tag" => wanted.push(args.text()?),
            Arg::Short('L') => (find, lookup) = (Some(("LABEL".into(), args.text()?)), true),
            Arg::Long(long) if long == "label" => {
                (find, lookup) = (Some(("LABEL".into(), args.text()?)), true);
            }
            Arg::Short('U') => (find, lookup) = (Some(("UUID".into(), args.text()?)), true),
            Arg::Long(long) if long == "uuid" => {
                (find, lookup) = (Some(("UUID".into(), args.text()?)), true);
            }
            Arg::Short('t') => find = Some(token(&args.text()?)?),
            Arg::Long(long) if long == "match-token" => find = Some(token(&args.text()?)?),
            // The cache file and probing outright: there is no cache.
            Arg::Short('c') => {
                args.value()?;
            }
            Arg::Long(long) if long == "cache-file" => {
                args.value()?;
            }
            Arg::Short('p' | 'g') => {}
            Arg::Long(long) if long == "probe" || long == "garbage-collect" => {}
            Arg::Operand(name) => {
                let shown = name.to_string_lossy();
                let name = shown.strip_prefix("/devices/").or_else(|| shown.strip_prefix("/dev/"));
                names.push(name.unwrap_or(&shown).to_string());
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let named = !names.is_empty();
    if !named {
        names = probe::devices()
            .map_err(|e| util::failed(&BLOCKS, e))?
            .into_iter()
            .map(|(name, _)| name)
            .collect();
    }
    // -L and -U print the device alone, as util-linux does.
    let by_name = lookup && output == Output::Full && wanted.is_empty();
    let mut out = io::stdout().lock();
    let mut found = false;
    for name in &names {
        if named && probe::block_text(name, "size").is_none() {
            eprintln!("blkid: /devices/{name}: no such disk or partition");
            continue;
        }
        let mut tags = tags(name);
        if let Some((tag, value)) = &find
            && !tags.iter().any(|(t, v)| t == tag && v == value)
        {
            continue;
        }
        if !wanted.is_empty() {
            tags.retain(|(tag, _)| wanted.iter().any(|w| w == tag));
        }
        if tags.is_empty() {
            continue;
        }
        found = true;
        let path = format!("/devices/{name}");
        match (output, by_name) {
            (_, true) | (Output::Device, _) => writeln!(out, "{path}")?,
            (Output::Full, false) => {
                let fields: Vec<String> =
                    tags.iter().map(|(tag, value)| format!("{tag}={}", quoted(value))).collect();
                writeln!(out, "{path}: {}", fields.join(" "))?;
            }
            (Output::Value, false) => {
                for (_, value) in &tags {
                    writeln!(out, "{value}")?;
                }
            }
            (Output::Export, false) => {
                writeln!(out, "DEVNAME={path}")?;
                for (tag, value) in &tags {
                    writeln!(out, "{tag}={}", exported(value))?;
                }
                writeln!(out)?;
            }
        }
    }
    Ok(if found { 0 } else { NOT_FOUND })
}

/// The format `-o` names.
fn output_of(name: &str) -> Result<Output, Fail> {
    Ok(match name {
        "full" => Output::Full,
        "value" => Output::Value,
        "export" => Output::Export,
        "device" => Output::Device,
        other => return Err(usage(format!("unsupported output format {other}"))),
    })
}

/// `-t NAME=value`: the tag and the value it must have.
fn token(token: &str) -> Result<(String, String), Fail> {
    let Some((tag, value)) = token.split_once('=') else {
        return Err(usage(format!("invalid token {token}: not NAME=value")));
    };
    Ok((tag.to_string(), value.trim_matches('"').to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_quoted_and_escaped() {
        assert_eq!(quoted(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(exported("my disk"), r"my\ disk");
        assert_eq!(exported("1234-ABCD"), "1234-ABCD");
    }
}
