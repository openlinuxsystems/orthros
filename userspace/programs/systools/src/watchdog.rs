// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! watchdog(8): the hardware watchdog Spark pets (ADR 0143), from
//! `/kernel/class/watchdog/watchdog0`, as util-linux's wdctl shows it.

use std::io::{self, Write};
use std::path::Path;

use crate::args::Args;
use crate::util::{self, Fail};

/// Where the kernel shows it.
const DIR: &str = "/kernel/class/watchdog/watchdog0";

/// Linux's WDIOF_CARDRESET: it restarted the machine.
const CARD_RESET: u32 = 32;

/// The file `name` of the watchdog's directory, without its newline.
fn read(dir: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(name)).ok()?;
    Some(text.trim_end().to_string())
}

/// What the watchdog in `dir` is and does, a line for each.
fn report(dir: &Path) -> Option<Vec<(&'static str, String)>> {
    let identity = read(dir, "identity")?;
    let active = read(dir, "state")? == "active";
    let timeout = read(dir, "timeout")?;
    let (min, max) = (read(dir, "min_timeout")?, read(dir, "max_timeout")?);
    let status: u32 = read(dir, "bootstatus")?.parse().ok()?;
    let state = if active { "running, petted by Spark" } else { "stopped" };
    let timeout = if active { format!("{timeout} seconds") } else { "none".to_string() };
    let restart = if status & CARD_RESET != 0 {
        "it restarted the machine at the end of the last boot"
    } else {
        "the last boot did not end with it"
    };
    Some(vec![
        ("Device", identity),
        ("State", state.to_string()),
        ("Timeout", timeout),
        ("Timeouts", format!("{min} to {max} seconds")),
        ("Last boot", restart.to_string()),
    ])
}

/// watchdog(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    let Some(lines) = report(Path::new(DIR)) else {
        return Err(util::fail("the kernel drives no hardware watchdog".to_string()));
    };
    let mut out = io::stdout().lock();
    for (label, value) in lines {
        writeln!(out, "{:<11}{value}", format!("{label}:"))?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_what_the_kernel_shows() {
        let dir = std::env::temp_dir().join(format!("watchdog-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, value) in [
            ("identity", "the Intel 6300ESB watchdog"),
            ("state", "active"),
            ("timeout", "60"),
            ("min_timeout", "1"),
            ("max_timeout", "2082"),
            ("bootstatus", "32"),
        ] {
            std::fs::write(dir.join(name), format!("{value}\n")).unwrap();
        }
        let lines = report(&dir).unwrap();
        assert_eq!(lines[0].1, "the Intel 6300ESB watchdog");
        assert_eq!(lines[1].1, "running, petted by Spark");
        assert_eq!(lines[2].1, "60 seconds");
        assert_eq!(lines[3].1, "1 to 2082 seconds");
        assert!(lines[4].1.starts_with("it restarted"));
        std::fs::write(dir.join("state"), "inactive\n").unwrap();
        std::fs::write(dir.join("bootstatus"), "0\n").unwrap();
        let lines = report(&dir).unwrap();
        assert_eq!(lines[1].1, "stopped");
        assert_eq!(lines[2].1, "none");
        assert!(lines[4].1.starts_with("the last boot did not"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(report(&dir).is_none());
    }
}
