// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! nvme(8): lists the NVMe namespaces and shows a controller's SMART /
//! Health Information log, as nvme-cli's `nvme list` and `nvme
//! smart-log` do, from what the kernel shows of each disk
//! (radix-sysfs(5)) and radix-block(2)'s GET_HEALTH (ADR 0157).

use std::io::{self, Write};

use crate::args::{Arg, Args};
use crate::diskhealth::{self, NvmeLog, Record, grouped};
use crate::util::{self, Fail};

/// What the kernel shows of disk `name`, as text: its device's, or its
/// own.
fn shown(name: &str, what: &str) -> String {
    diskhealth::device_text(name, what)
}

/// The namespace ID of disk `name`: the number after its `n`.
fn namespace(name: &str) -> Option<u32> {
    name.strip_prefix("nvme")?.split_once('n')?.1.parse().ok()
}

/// A size as nvme-cli prints it: in powers of 1000, two decimals.
fn size(bytes: u64) -> String {
    let units = ["B", "kB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < units.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.2} {}", units[unit])
}

/// The bytes of disk `name`, from its size in 512-byte sectors.
fn bytes_of(name: &str) -> u64 {
    std::fs::read_to_string(format!("/kernel/block/{name}/size"))
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map_or(0, |sectors| sectors.saturating_mul(512))
}

/// `nvme list`: a line for each namespace.
fn list(out: &mut impl Write) -> Result<i32, Fail> {
    let names = diskhealth::disks().map_err(|e| util::failed(&"/kernel/block", e))?;
    let names: Vec<String> = names.into_iter().filter(|n| namespace(n).is_some()).collect();
    let rows: Vec<[String; 6]> = names
        .iter()
        .map(|name| {
            let sector =
                std::fs::read_to_string(format!("/kernel/block/{name}/queue/logical_block_size"))
                    .map_or_else(|_| "512".to_string(), |text| text.trim().to_string());
            [
                format!("/devices/{name}"),
                shown(name, "serial"),
                shown(name, "model"),
                format!("{:#x}", namespace(name).unwrap_or(0)),
                format!("{}  {sector} B", size(bytes_of(name))),
                shown(name, "firmware_rev"),
            ]
        })
        .collect();
    let heads = ["Node", "SN", "Model", "Namespace", "Size, Format", "FW Rev"];
    let widths: Vec<usize> = (0..heads.len())
        .map(|c| rows.iter().map(|r| r[c].len()).chain([heads[c].len()]).max().unwrap_or(0))
        .collect();
    let line = |cells: &[&str]| {
        let parts: Vec<String> =
            cells.iter().zip(&widths).map(|(cell, &w)| format!("{cell:<w$}")).collect();
        format!("{}\n", parts.join(" ").trim_end())
    };
    let dashes: Vec<String> = widths.iter().map(|&w| "-".repeat(w)).collect();
    let mut text = line(&heads);
    text.push_str(&line(&dashes.iter().map(String::as_str).collect::<Vec<_>>()));
    for row in &rows {
        text.push_str(&line(&row.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    let _ = out.write_all(text.as_bytes());
    Ok(0)
}

/// `nvme smart-log`'s lines for the log `log` of disk `name`.
fn smart_log(name: &str, log: &NvmeLog) -> String {
    // Data units are thousands of 512-byte units.
    let units = |n: u128| {
        let bytes = u64::try_from(n.saturating_mul(512_000)).unwrap_or(u64::MAX);
        format!("{} ({})", grouped(n), size(bytes))
    };
    let celsius = i64::from(log.kelvin) - 273;
    let rows = [
        ("critical_warning", format!("{:#x}", log.critical)),
        ("temperature", format!("{celsius}°C ({} Kelvin)", log.kelvin)),
        ("available_spare", format!("{}%", log.spare)),
        ("available_spare_threshold", format!("{}%", log.spare_threshold)),
        ("percentage_used", format!("{}%", log.used)),
        ("Data Units Read", units(log.units_read)),
        ("Data Units Written", units(log.units_written)),
        ("power_cycles", grouped(log.power_cycles)),
        ("power_on_hours", grouped(log.hours)),
        ("unsafe_shutdowns", grouped(log.unsafe_shutdowns)),
        ("media_errors", grouped(log.media_errors)),
        ("num_err_log_entries", grouped(log.error_entries)),
        ("Warning Temperature Time", log.warning_minutes.to_string()),
        ("Critical Composite Temperature Time", log.critical_minutes.to_string()),
    ];
    let mut text = format!("Smart Log for NVME device:{name} namespace-id:ffffffff\n");
    for (what, value) in rows {
        text.push_str(&format!("{what:<40}: {value}\n"));
    }
    text
}

/// nvme(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let usage = || util::usage("say list, or smart-log and a disk".into());
    let Some(action) = args.subcommand() else { return Err(usage()) };
    let mut operands = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Operand(operand) => operands.push(operand),
            other => return Err(args.unknown(&other)),
        }
    }
    let mut out = io::stdout().lock();
    match (action.to_str(), operands.as_slice()) {
        (Some("list"), []) => list(&mut out),
        (Some("smart-log"), [disk]) => {
            let path = disk.to_string_lossy();
            let name = path.rsplit('/').next().unwrap_or(&path).to_string();
            match diskhealth::read(&name)? {
                Some(Record::Nvme(log)) => {
                    let _ = out.write_all(smart_log(&name, &log).as_bytes());
                    Ok(0)
                }
                _ => Err(util::fail(format!("{path}: not an NVMe device"))),
            }
        }
        _ => Err(usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_and_sizes_read_as_nvme_cli_has_them() {
        assert_eq!(namespace("nvme0n1"), Some(1));
        assert_eq!(namespace("nvme1n12"), Some(12));
        assert_eq!(namespace("nvme0n1p2"), None);
        assert_eq!(namespace("sda"), None);
        assert_eq!(size(134_217_728), "134.22 MB");
        assert_eq!(grouped(1234567), "1,234,567");
    }

    #[test]
    fn the_smart_log_has_nvme_clis_lines() {
        let log = NvmeLog {
            kelvin: 300,
            spare: 100,
            spare_threshold: 10,
            units_read: 2,
            hours: 5,
            ..NvmeLog::default()
        };
        let text = smart_log("nvme0n1", &log);
        assert!(text.starts_with("Smart Log for NVME device:nvme0n1 namespace-id:ffffffff\n"));
        assert!(
            text.contains("temperature                             : 27°C (300 Kelvin)\n"),
            "{text}"
        );
        assert!(text.contains("Data Units Read                         : 2 (1.02 MB)\n"), "{text}");
        assert!(text.contains("power_on_hours                          : 5\n"), "{text}");
    }
}
