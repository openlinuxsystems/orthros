// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Service files (spark(8)): what Spark starts at boot and keeps running.
//!
//! Each `NAME.service` file in `/system/settings/spark/services`
//! describes one service, as lines of `key = value`; `#` starts a
//! comment. The system's own come from
//! `/system/resources/default-settings/spark/services`, which Spark copies
//! in when they are missing, so an administrator turns one off with
//! `enabled = no` rather than by removing it.
//!
//! A service reaches the network only as its file says (ADR 0183):
//! `network.outbound = yes` lets it connect out, and `network.listen`
//! names the ports it may listen on. `admin.hardware = yes` gives it the
//! platform capability, which sets the fans, the LEDs and the ARGB header
//! (ADR 0208).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libradix::abi::resource_domain::{NAME_MAX, OomClass, WEIGHT_DEFAULT, WEIGHT_MAX};
use libradix::abi::startup::NETWORK_SLOTS;

/// When a service that ended is started again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    /// Whatever its end.
    Always,
    /// When it failed: an exit code other than 0, or a signal.
    OnFailure,
    /// Never.
    Never,
}

impl Restart {
    pub fn name(self) -> &'static str {
        match self {
            Restart::Always => "always",
            Restart::OnFailure => "on-failure",
            Restart::Never => "never",
        }
    }
}

/// One service file.
#[derive(Clone, Debug)]
pub struct Spec {
    /// The file's name without `.service`: the service's and its domain's.
    pub name: String,
    pub description: String,
    /// The program's absolute path.
    pub program: String,
    pub arguments: Vec<String>,
    /// Lower starts first; shutdown stops the highest first.
    pub order: u32,
    pub restart: Restart,
    /// The CPU weight of its domain.
    pub weight: u32,
    /// The memory limit of its domain, in pages.
    pub memory_pages: Option<u64>,
    /// Its domain's out-of-memory class: service or app.
    pub oom: OomClass,
    /// More of the environment, as `NAME=value`.
    pub environment: Vec<String>,
    pub enabled: bool,
    /// Whether it may connect out, to any address and port.
    pub outbound: bool,
    /// The ranges of ports it may listen on, as (first, count).
    pub listen: Vec<(u32, u32)>,
    /// Whether it gets the platform capability: admin.hardware.
    pub hardware: bool,
}

impl Spec {
    /// How many network capabilities it gets: one to connect out, and one
    /// for each range of ports it listens on.
    pub fn network_grants(&self) -> usize {
        usize::from(self.outbound) + self.listen.len()
    }
}

/// The ports `value` names, as `network.listen` takes them: ports and
/// ranges of ports (`8000-8009`), with commas or blanks between, as
/// (first, count), each range once.
fn ports(value: &str) -> Result<Vec<(u32, u32)>, String> {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    let port = |text: &str| -> Result<u32, String> {
        match text.trim().parse::<u32>() {
            Ok(port) if (1..=65_535).contains(&port) => Ok(port),
            _ => Err(format!("`{text}` is not a port from 1 to 65535")),
        }
    };
    for item in value.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
        if item.is_empty() {
            continue;
        }
        let (first, last) = match item.split_once('-') {
            Some((first, last)) => (port(first)?, port(last)?),
            None => (port(item)?, port(item)?),
        };
        if last < first {
            return Err(format!("the range `{item}` ends before it starts"));
        }
        let range = (first, last - first + 1);
        let overlaps = ranges.iter().any(|&(at, count)| first < at + count && at <= last);
        if overlaps {
            return Err(format!("`{item}` overlaps another range"));
        }
        ranges.push(range);
    }
    Ok(ranges)
}

/// Splits `text` into words at blanks, a pair of double quotes keeping
/// blanks in a word.
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

/// A size such as `64M`, `512K`, `2G` or a number of bytes, in pages; or
/// none.
fn memory(value: &str) -> Result<Option<u64>, String> {
    if value == "none" || value.is_empty() {
        return Ok(None);
    }
    let (digits, unit) = match value.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => value.split_at(at),
        None => (value, ""),
    };
    let scale: u64 = match unit {
        "" => 1,
        "K" | "k" | "KiB" => 1 << 10,
        "M" | "m" | "MiB" => 1 << 20,
        "G" | "g" | "GiB" => 1 << 30,
        _ => return Err(format!("`{value}` is not a size such as 64M")),
    };
    let number: u64 = digits.parse().map_err(|_| format!("`{value}` is not a size"))?;
    let bytes = number.checked_mul(scale).ok_or(format!("`{value}` is too large"))?;
    Ok(Some(bytes.div_ceil(4096).max(1)))
}

fn yes_no(value: &str) -> Result<bool, String> {
    match value {
        "yes" | "true" | "on" => Ok(true),
        "no" | "false" | "off" => Ok(false),
        _ => Err(format!("`{value}` is neither yes nor no")),
    }
}

/// Reads the service file `name`.service with `text`.
pub fn parse(name: &str, text: &str) -> Result<Spec, String> {
    if name.is_empty() || name.len() > NAME_MAX as usize {
        return Err(format!("a service's name has 1 to {NAME_MAX} bytes"));
    }
    let mut spec = Spec {
        name: name.to_string(),
        description: String::new(),
        program: String::new(),
        arguments: Vec::new(),
        order: 50,
        restart: Restart::Always,
        weight: WEIGHT_DEFAULT,
        memory_pages: None,
        oom: OomClass::Service,
        environment: Vec::new(),
        enabled: true,
        outbound: false,
        listen: Vec::new(),
        hardware: false,
    };
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |message: String| format!("line {}: {message}", number + 1);
        let Some((key, value)) = line.split_once('=') else {
            return Err(at(format!("`{line}` is not `key = value`")));
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "description" => spec.description = value.to_string(),
            "program" => {
                if !value.starts_with('/') {
                    return Err(at(format!("the program `{value}` is not an absolute path")));
                }
                spec.program = value.to_string();
            }
            "arguments" => spec.arguments = words(value),
            "order" => {
                spec.order = value.parse().map_err(|_| at(format!("`{value}` is not a number")))?;
            }
            "restart" => {
                spec.restart = match value {
                    "always" => Restart::Always,
                    "on-failure" => Restart::OnFailure,
                    "never" => Restart::Never,
                    _ => return Err(at("restart is always, on-failure or never".into())),
                }
            }
            "weight" => {
                let weight: u32 =
                    value.parse().map_err(|_| at(format!("`{value}` is not a number")))?;
                if !(1..=WEIGHT_MAX).contains(&weight) {
                    return Err(at(format!("the weight is 1 to {WEIGHT_MAX}")));
                }
                spec.weight = weight;
            }
            "memory" => spec.memory_pages = memory(value).map_err(at)?,
            "oom" => {
                spec.oom = match value {
                    "service" => OomClass::Service,
                    "app" => OomClass::App,
                    _ => return Err(at("oom is service or app".into())),
                }
            }
            "environment" => {
                spec.environment = words(value);
                if let Some(bad) = spec.environment.iter().find(|w| !w.contains('=')) {
                    return Err(at(format!("`{bad}` is not NAME=value")));
                }
            }
            "enabled" => spec.enabled = yes_no(value).map_err(at)?,
            "network.outbound" => spec.outbound = yes_no(value).map_err(at)?,
            "network.listen" => spec.listen = ports(value).map_err(at)?,
            "admin.hardware" => spec.hardware = yes_no(value).map_err(at)?,
            _ => return Err(at(format!("unknown key `{key}`"))),
        }
    }
    if spec.program.is_empty() {
        return Err("it names no program".into());
    }
    if spec.network_grants() > NETWORK_SLOTS as usize {
        return Err(format!(
            "it asks for {} network capabilities, one to connect out and one for each range \
             of ports; a program gets at most {NETWORK_SLOTS}",
            spec.network_grants()
        ));
    }
    Ok(spec)
}
