// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Layout files (rdisk-layout(5)): a partition table in JSON, which
//! `rdisk --script` applies to a disk and `rdisk --dump` writes from one.
//!
//! ```json
//! {
//!   "table": "gpt",
//!   "partitions": [
//!     { "type": "bios-boot", "size": "1MiB" },
//!     { "type": "efi", "size": "512MiB", "name": "EFI system" },
//!     { "type": "swap", "size": "8GiB" },
//!     { "type": "dukovo-root", "size": "rest", "name": "Dukovo root" }
//!   ]
//! }
//! ```
//!
//! A file names its table and its partitions in order, each placed as
//! rdisk's `n` places one: on the first whole MiB of free space it fits
//! in, unless its `start` says where. `"keep": true` keeps the disk's
//! partitions and adds these in its free space, after taking out those
//! `delete` names, as the installer's "Use free space" does. Unknown
//! names are refused, so that a mistyped one is not missed.

use serde::{Deserialize, Serialize};

use crate::guid::Guid;
use crate::rng::Rng;
use crate::size;
use crate::table::{ATTRIBUTES, End, New, Scheme, Table};
use crate::types;

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's skip_serializing_if passes a reference")]
fn is_false(b: &bool) -> bool {
    !*b
}

/// A layout file.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// `gpt` or `mbr` (or `dos`, as sfdisk calls one).
    pub table: String,
    /// The disk's GUID (GPT) or signature (MBR, `0x1234abcd`), or a
    /// random one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Keep the disk's partitions, and add these in its free space.
    #[serde(default, skip_serializing_if = "is_false")]
    pub keep: bool,
    /// With `keep`, the partitions to take out first, by number.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delete: Vec<u32>,
    /// The partitions to add, in order.
    #[serde(default)]
    pub partitions: Vec<Part>,
}

/// A partition of a layout file.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Part {
    /// An alias (`efi`), sgdisk's code (`ef00`), a type GUID, or an MBR
    /// type byte (`0x83`).
    #[serde(rename = "type")]
    pub kind: String,
    /// `512MiB`, `2048s` (sectors), or `rest`: to the end of the free
    /// space it starts in.
    pub size: String,
    /// Where it starts, from the disk's start: `1MiB`, `2048s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// Its number, or the first free one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u32>,
    /// Its name, in a GPT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Its unique GUID, in a GPT, or a random one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Whether it is bootable, in an MBR.
    #[serde(default, skip_serializing_if = "is_false")]
    pub bootable: bool,
    /// Its GPT attributes: `required`, `no-block-io`,
    /// `legacy-bios-bootable`, `read-only`, `hidden`, `no-automount`, or
    /// `bit-N` for bit N.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<String>,
}

/// The layout file `text`.
///
/// # Errors
///
/// In words, with the line and column, if it is not JSON or not a layout.
pub fn parse(text: &str) -> Result<Layout, String> {
    serde_json::from_str(text).map_err(|err| err.to_string())
}

/// The scheme a layout's `table` names.
///
/// # Errors
///
/// If it names none.
pub fn scheme(name: &str) -> Result<Scheme, String> {
    match name {
        "gpt" => Ok(Scheme::Gpt),
        "mbr" | "dos" => Ok(Scheme::Mbr),
        other => Err(format!("\"table\" is \"gpt\" or \"mbr\", not \"{other}\"")),
    }
}

/// The attribute bits `names` stand for.
///
/// # Errors
///
/// If one names none.
pub fn attribute_bits(names: &[String]) -> Result<u64, String> {
    let mut bits = 0u64;
    for name in names {
        let bit = ATTRIBUTES
            .iter()
            .find(|(n, _)| n == name)
            .map(|&(_, bit)| bit)
            .or_else(|| name.strip_prefix("bit-").and_then(|n| n.parse().ok()).filter(|&b| b < 64));
        let Some(bit) = bit else {
            let known: Vec<&str> = ATTRIBUTES.iter().map(|(n, _)| *n).collect();
            return Err(format!(
                "\"{name}\" is no attribute; they are {}, and bit-0 to bit-63",
                known.join(", ")
            ));
        };
        bits |= 1 << bit;
    }
    Ok(bits)
}

/// The names of the attribute bits in `bits`.
pub fn attribute_names(bits: u64) -> Vec<String> {
    (0..64)
        .filter(|bit| bits & (1 << bit) != 0)
        .map(|bit| match ATTRIBUTES.iter().find(|(_, b)| *b == bit) {
            Some((name, _)) => (*name).to_string(),
            None => format!("bit-{bit}"),
        })
        .collect()
}

/// A disk's GUID or signature, as `uuid` gives it.
fn disk_id(table: &mut Table, uuid: &str) -> Result<(), String> {
    match table.scheme {
        Scheme::Gpt => {
            table.disk_guid =
                Guid::from_text(uuid).ok_or_else(|| format!("\"{uuid}\" is not a GUID"))?;
        }
        Scheme::Mbr => {
            let hex = uuid.strip_prefix("0x").unwrap_or(uuid);
            table.signature = u32::from_str_radix(hex, 16)
                .ok()
                .filter(|_| !hex.is_empty() && hex.len() <= 8)
                .ok_or_else(|| format!("\"{uuid}\" is not an MBR's signature, such as 0x1234abcd"))?;
        }
    }
    Ok(())
}

/// The table `layout` makes on a disk of `sectors` logical sectors of
/// `sector_size` bytes, whose table now is `existing`.
///
/// # Errors
///
/// In words, naming the partition, if the layout cannot be made.
pub fn apply(
    layout: &Layout,
    sector_size: u32,
    sectors: u64,
    existing: Option<&Table>,
    rng: &mut Rng,
) -> Result<Table, String> {
    let scheme = scheme(&layout.table)?;
    let mut table = if layout.keep {
        match existing {
            Some(table) if table.scheme == scheme => table.clone(),
            Some(table) => {
                return Err(format!(
                    "\"keep\" keeps the disk's table, which is {}, not {}",
                    table.scheme.name(),
                    scheme.name()
                ));
            }
            None => return Err("\"keep\" keeps the disk's table, and it has none".to_string()),
        }
    } else {
        if !layout.delete.is_empty() {
            return Err("\"delete\" takes partitions out of a table kept with \"keep\"".to_string());
        }
        let mut table = Table::new(scheme, sector_size, sectors, rng).map_err(|e| e.0)?;
        if let Some(existing) = existing {
            table.boot_code.clone_from(&existing.boot_code);
        }
        table
    };
    if let Some(uuid) = &layout.uuid {
        disk_id(&mut table, uuid)?;
    }
    for &number in &layout.delete {
        table.delete(number).map_err(|e| format!("\"delete\": {e}"))?;
    }
    for (i, part) in layout.partitions.iter().enumerate() {
        let which = format!("partition {} of the file ({})", i + 1, part.kind);
        let new = new_partition(part, scheme, sector_size).map_err(|e| format!("{which}: {e}"))?;
        table.add(new, rng).map_err(|e| format!("{which}: {e}"))?;
    }
    Ok(table)
}

/// What `part` asks for.
fn new_partition(part: &Part, scheme: Scheme, sector_size: u32) -> Result<New, String> {
    let kind = types::parse(&part.kind, scheme).ok_or_else(|| {
        format!("\"{}\" is no type of a {}; `rdisk --list-types` lists them", part.kind, scheme.name())
    })?;
    let end = if part.size == "rest" {
        End::Rest
    } else {
        let amount = size::parse(&part.size)
            .ok_or_else(|| format!("\"{}\" is not a size, such as 512MiB, 2048s or rest", part.size))?;
        End::Sectors(amount.sectors_up(sector_size))
    };
    let first = match &part.start {
        None => None,
        Some(start) => {
            let amount = size::parse(start).ok_or_else(|| format!("\"{start}\" is not a start"))?;
            Some(amount.sectors(sector_size).ok_or_else(|| {
                format!("\"{start}\" is not a whole number of {sector_size}-byte sectors")
            })?)
        }
    };
    let guid = match &part.uuid {
        Some(text) if scheme == Scheme::Gpt => {
            Some(Guid::from_text(text).ok_or_else(|| format!("\"{text}\" is not a GUID"))?)
        }
        Some(_) => return Err("an MBR partition has no GUID".to_string()),
        None => None,
    };
    if scheme == Scheme::Mbr && part.name.is_some() {
        return Err("an MBR partition has no name".to_string());
    }
    if scheme == Scheme::Gpt && part.bootable {
        return Err(
            "a GPT marks no partition bootable; the attribute legacy-bios-bootable is what some \
             BIOSes look for"
                .to_string(),
        );
    }
    if scheme == Scheme::Mbr && !part.attributes.is_empty() {
        return Err("an MBR partition has no attributes".to_string());
    }
    Ok(New {
        number: part.number,
        first,
        end,
        kind,
        name: part.name.clone().unwrap_or_default(),
        guid,
        attributes: attribute_bits(&part.attributes)?,
        bootable: part.bootable,
    })
}

/// The layout of `table`, exactly: starts and sizes in sectors, and
/// every GUID, so that applying it makes the same table.
///
/// # Errors
///
/// If the table has logical partitions, which a layout cannot make.
pub fn dump(table: &Table) -> Result<Layout, String> {
    if table.partitions.iter().any(|p| p.logical) {
        return Err(
            "the disk has logical partitions, which a layout file cannot make; rdisk keeps them \
             as they are"
                .to_string(),
        );
    }
    let partitions = table
        .partitions
        .iter()
        .map(|p| Part {
            kind: p.kind.alias().map_or_else(|| p.kind.to_string(), str::to_string),
            size: format!("{}s", p.sectors()),
            start: Some(format!("{}s", p.first)),
            number: Some(p.number),
            name: (!p.name.is_empty()).then(|| p.name.clone()),
            uuid: (table.scheme == Scheme::Gpt).then(|| p.guid.lower()),
            bootable: p.bootable,
            attributes: attribute_names(p.attributes),
        })
        .collect();
    let uuid = match table.scheme {
        Scheme::Gpt => table.disk_guid.lower(),
        Scheme::Mbr => format!("0x{:08x}", table.signature),
    };
    Ok(Layout {
        table: table.scheme.name().to_string(),
        uuid: Some(uuid),
        keep: false,
        delete: Vec::new(),
        partitions,
    })
}

/// [`dump`] as the JSON of a layout file.
///
/// # Errors
///
/// As [`dump`].
pub fn dump_text(table: &Table) -> Result<String, String> {
    let layout = dump(table)?;
    serde_json::to_string_pretty(&layout).map(|mut text| {
        text.push('\n');
        text
    }).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUIDED: &str = r#"{
        "table": "gpt",
        "partitions": [
            { "type": "bios-boot", "size": "1MiB" },
            { "type": "efi", "size": "512MiB", "name": "EFI system" },
            { "type": "swap", "size": "8GiB" },
            { "type": "dukovo-root", "size": "rest", "name": "Dukovo root" }
        ]
    }"#;

    #[test]
    fn the_guided_layout() {
        let mut rng = Rng::seeded(5);
        let layout = parse(GUIDED).unwrap();
        let table = apply(&layout, 512, 41_943_040, None, &mut rng).unwrap();
        let spans: Vec<(u32, u64, u64)> =
            table.partitions.iter().map(|p| (p.number, p.first, p.last)).collect();
        assert_eq!(
            spans,
            vec![
                (1, 2048, 4095),
                (2, 4096, 1_052_671),
                (3, 1_052_672, 17_829_887),
                (4, 17_829_888, 41_943_006)
            ]
        );
        assert_eq!(table.get(4).unwrap().name, "Dukovo root");
        let text = dump_text(&table).unwrap();
        let again = apply(&parse(&text).unwrap(), 512, 41_943_040, None, &mut rng).unwrap();
        assert_eq!(again, table);
    }

    #[test]
    fn mistakes_are_named() {
        let mut rng = Rng::seeded(6);
        let mut bad = |text: &str| {
            let layout = parse(text)?;
            apply(&layout, 512, 1 << 21, None, &mut rng).map(drop)
        };
        let err = bad(r#"{"table":"gpt","partitions":[{"type":"efi","size":"1MiB","nmae":"x"}]}"#);
        assert!(err.unwrap_err().contains("unknown field `nmae`"));
        let err = bad(r#"{"table":"gpt","partitions":[{"type":"nope","size":"1MiB"}]}"#);
        assert!(err.unwrap_err().contains("partition 1 of the file (nope)"));
        let err = bad(r#"{"table":"mbr","partitions":[{"type":"efi","size":"1MiB","name":"x"}]}"#);
        assert!(err.unwrap_err().contains("no name"));
        let err = bad(r#"{"table":"gpt","partitions":[{"type":"efi","size":"2GiB"}]}"#);
        assert!(err.unwrap_err().contains("no free space"));
        let err = bad(r#"{"table":"zfs"}"#);
        assert!(err.unwrap_err().contains("\"gpt\" or \"mbr\""));
        let err = bad(r#"{"table":"gpt","delete":[1]}"#);
        assert!(err.unwrap_err().contains("keep"));
    }

    #[test]
    fn keep_adds_in_free_space() {
        let mut rng = Rng::seeded(7);
        let first = r#"{"table":"mbr","uuid":"0x1234abcd","partitions":[
            {"type":"fat32","size":"64MiB","bootable":true},
            {"type":"linux","size":"64MiB"}]}"#;
        let table = apply(&parse(first).unwrap(), 512, 1 << 20, None, &mut rng).unwrap();
        assert_eq!(table.signature, 0x1234_abcd);
        let more = r#"{"table":"mbr","keep":true,"delete":[1],"partitions":[
            {"type":"swap","size":"rest"}]}"#;
        let kept = apply(&parse(more).unwrap(), 512, 1 << 20, Some(&table), &mut rng).unwrap();
        let spans: Vec<(u32, u64, u64, String)> =
            kept.partitions.iter().map(|p| (p.number, p.first, p.last, p.kind.to_string())).collect();
        assert_eq!(
            spans,
            vec![
                (1, 2048, 133_119, "0x82".to_string()),
                (2, 133_120, 264_191, "0x83".to_string())
            ]
        );
        assert_eq!(attribute_bits(&["legacy-bios-bootable".into(), "bit-48".into()]), Ok(1 << 2 | 1 << 48));
        assert_eq!(attribute_names(1 << 2 | 1 << 48), ["legacy-bios-bootable", "bit-48"]);
    }
}
