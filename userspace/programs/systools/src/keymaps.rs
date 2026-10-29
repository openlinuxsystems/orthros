// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! keymap(1): the keyboard layout the consoles type in (ADR 0149).
//!
//! The layout is a setting, `/system/settings/keyboard`, which consoled
//! reads when it starts and when Spark sends it SIGHUP (`service consoled
//! reload`). `keymap NAME` saves it and asks Spark for that; Linux's
//! `localectl set-keymap` does as much for its consoles.

use std::io::{self, Write};

use crate::args::{Arg, Args};
use crate::power;
use crate::util::{self, Fail, fail, usage};

const USAGE: &str = "usage: keymap [--list | NAME]";

/// Writes the setting naming `layout`: a new file put in the old one's
/// place, so that consoled never reads half of one.
fn save(layout: &str) -> Result<(), Fail> {
    let new = format!("{}.new", keymap::SETTING);
    std::fs::write(&new, keymap::setting_text(layout)).map_err(|e| util::failed(&new, e))?;
    std::fs::rename(&new, keymap::SETTING).map_err(|e| util::failed(&keymap::SETTING, e))
}

/// keymap(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut list = false;
    let mut names = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('l') => list = true,
            Arg::Long(l) if l == "list" => list = true,
            Arg::Long(l) if l == "help" => {
                println!("{USAGE}");
                return Ok(0);
            }
            Arg::Operand(name) => names.push(name.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    let mut out = io::stdout().lock();
    match (list, names.as_slice()) {
        (true, []) => {
            for &(name, text) in keymap::LAYOUTS {
                let description = keymap::Keymap::parse(text).map_or_default(|k| k.name);
                writeln!(out, "{name:16}{description}")?;
            }
            for name in keymap::own() {
                match keymap::load(&name) {
                    Ok(layout) => writeln!(out, "{name:16}{} (own)", layout.name)?,
                    Err(err) => writeln!(out, "{name:16}(own; {err})")?,
                }
            }
            Ok(0)
        }
        (false, []) => {
            let name = keymap::chosen().map_err(fail)?;
            let layout = keymap::load(&name).map_err(fail)?;
            writeln!(out, "{name:16}{}", layout.name)?;
            Ok(0)
        }
        (false, [name]) => {
            let layout = keymap::load(name).map_err(fail)?;
            save(name)?;
            match power::request("reload consoled") {
                Ok(answer) if answer.starts_with("ok") => {
                    writeln!(out, "The consoles type in {} ({name}) now.", layout.name)?;
                }
                _ => writeln!(
                    out,
                    "The consoles type in {} ({name}) from consoled's next start.",
                    layout.name
                )?,
            }
            Ok(0)
        }
        _ => Err(usage(USAGE.into())),
    }
}
