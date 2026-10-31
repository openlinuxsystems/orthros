// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! sensors(1): the temperatures and fan speeds of the hardware monitors
//! in `/kernel/class/hwmon` (ADRs 0205 and 0207), as lm-sensors' sensors
//! prints them: each chip by lm-sensors' name for it (`k10temp-pci-00c3`),
//! its adapter, then its fans and its temperatures, each with its limits.
//!
//! A chip's name is its `name`, its bus and its address, as libsensors
//! makes it for the drivers the kernel has: the firmware's thermal zones
//! (`acpitz`) are on ACPI, AMD's processor sensors (`k10temp`) on the
//! northbridge's PCI function 00:18.3, and Intel's packages (`coretemp`)
//! and Framework's embedded controller (`cros_ec`) on Linux's platform
//! bus, which libsensors calls ISA, numbered from 0.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::args::{Arg, Args};
use crate::util::{Fail, fail, usage};

/// Where the kernel shows its hardware monitors.
const HWMON: &str = "/kernel/class/hwmon";

/// libsensors' narrowest label column, before the colon and the space
/// after it.
const LABEL_WIDTH: usize = 11;

/// One hardware monitor, as libsensors names it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Chip {
    /// Its `name`: `k10temp`.
    prefix: String,
    /// `acpi`, `pci` or `isa`.
    bus: &'static str,
    /// Its address on that bus.
    address: u32,
    /// Its directory.
    dir: PathBuf,
}

impl Chip {
    /// Its name, as libsensors prints it: `k10temp-pci-00c3`,
    /// `acpitz-acpi-0`.
    fn name(&self) -> String {
        match self.bus {
            "acpi" => format!("{}-acpi-{}", self.prefix, self.address),
            bus => format!("{}-{bus}-{:04x}", self.prefix, self.address),
        }
    }

    /// The name of its adapter, as libsensors gives it.
    fn adapter(&self) -> &'static str {
        match self.bus {
            "acpi" => "ACPI interface",
            "pci" => "PCI adapter",
            _ => "ISA adapter",
        }
    }
}

/// The chips in `hwmon`, in the kernel's order.
fn chips(hwmon: &Path) -> Vec<Chip> {
    let Ok(entries) = std::fs::read_dir(hwmon) else { return Vec::new() };
    let mut dirs: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some((name.strip_prefix("hwmon")?.parse().ok()?, entry.path()))
        })
        .collect();
    dirs.sort();
    let mut chips: Vec<Chip> = Vec::new();
    for (_, dir) in dirs {
        let Some(prefix) = read(&dir, "name") else { continue };
        // The ones before it of the same name, for its address.
        let before = chips.iter().filter(|chip| chip.prefix == prefix).count() as u32;
        let (bus, address) = match prefix.as_str() {
            "acpitz" => ("acpi", before),
            // The northbridge's function 3, 00:18.3, then the next node's.
            "k10temp" => ("pci", (0x18 + before) << 3 | 3),
            _ => ("isa", before),
        };
        chips.push(Chip { prefix, bus, address, dir });
    }
    chips
}

/// The file `name` of `dir`, without its newline.
fn read(dir: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(name)).ok()?;
    Some(text.trim_end().to_string())
}

/// What a fan or temperature is: `fan`, then `temp`, as libsensors
/// orders them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Fan,
    Temp,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Fan => "fan",
            Kind::Temp => "temp",
        }
    }
}

/// One of a chip's fans or temperatures, with the files the kernel
/// shows for it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Feature {
    kind: Kind,
    number: u32,
    /// The suffixes of its files (`input`, `crit`...), in the order
    /// libsensors prints them raw.
    files: Vec<String>,
}

impl Feature {
    /// `temp1`.
    fn name(&self) -> String {
        format!("{}{}", self.kind.prefix(), self.number)
    }

    /// Its label: its `_label` file, or its name.
    fn label(&self, chip: &Chip) -> String {
        if self.has("label")
            && let Some(label) = read(&chip.dir, &format!("{}_label", self.name()))
        {
            return label;
        }
        self.name()
    }

    fn has(&self, file: &str) -> bool {
        self.files.iter().any(|f| f == file)
    }

    /// The value of its file `file`, in the units libsensors gives:
    /// degrees, RPM, or 0 and 1; None if it is not there or cannot be
    /// read.
    fn value(&self, chip: &Chip, file: &str) -> Option<f64> {
        if !self.has(file) {
            return None;
        }
        let raw: f64 = read(&chip.dir, &format!("{}_{file}", self.name()))?.parse().ok()?;
        let scaled = self.kind == Kind::Temp && !file.ends_with("alarm") && file != "fault";
        Some(if scaled { raw / 1000.0 } else { raw })
    }
}

/// The order libsensors gives a feature's files in, raw.
const ORDER: [&str; 9] =
    ["input", "max", "min", "crit", "crit_alarm", "emergency", "alarm", "fault", "label"];

/// `chip`'s fans, then its temperatures, each by number.
fn features(chip: &Chip) -> Vec<Feature> {
    let Ok(entries) = std::fs::read_dir(&chip.dir) else { return Vec::new() };
    let mut features: Vec<Feature> = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else { continue };
        let Some((head, file)) = name.split_once('_') else { continue };
        let (kind, number) = if let Some(number) = head.strip_prefix("temp") {
            (Kind::Temp, number)
        } else if let Some(number) = head.strip_prefix("fan") {
            (Kind::Fan, number)
        } else {
            continue;
        };
        let Ok(number) = number.parse::<u32>() else { continue };
        match features.iter_mut().find(|f| f.kind == kind && f.number == number) {
            Some(feature) => feature.files.push(file.to_string()),
            None => features.push(Feature { kind, number, files: vec![file.to_string()] }),
        }
    }
    for feature in &mut features {
        let rank = |file: &String| ORDER.iter().position(|o| o == file).unwrap_or(ORDER.len());
        feature.files.sort_by_key(rank);
    }
    // A feature is one with an input, as libsensors has it.
    features.retain(|feature| feature.has("input"));
    features.sort_by_key(|feature| (feature.kind, feature.number));
    features
}

/// How the values are printed.
#[derive(Clone, Copy, Debug, Default)]
struct Style {
    fahrenheit: bool,
    no_adapter: bool,
}

impl Style {
    /// A temperature in the chosen unit.
    fn degrees(self, celsius: f64) -> f64 {
        if self.fahrenheit { celsius * 9.0 / 5.0 + 32.0 } else { celsius }
    }

    fn unit(self) -> &'static str {
        if self.fahrenheit { "°F" } else { "°C" }
    }
}

/// `chip`, as sensors prints it.
fn print_chip(chip: &Chip, style: Style, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "{}", chip.name())?;
    if !style.no_adapter {
        writeln!(out, "Adapter: {}", chip.adapter())?;
    }
    let features = features(chip);
    let labels: Vec<String> = features.iter().map(|f| f.label(chip)).collect();
    // The widest label, one more for the colon and one for a space.
    let width = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(LABEL_WIDTH) + 2;
    for (feature, label) in features.iter().zip(&labels) {
        let pad = width.saturating_sub(label.chars().count() + 1);
        write!(out, "{label}:{:pad$}", "")?;
        match feature.kind {
            Kind::Fan => print_fan(chip, feature, out)?,
            Kind::Temp => print_temp(chip, feature, style, width, out)?,
        }
        writeln!(out)?;
    }
    writeln!(out)
}

/// A fan's speed, or its fault.
fn print_fan(chip: &Chip, feature: &Feature, out: &mut dyn Write) -> io::Result<()> {
    if feature.value(chip, "fault").is_some_and(|fault| fault != 0.0) {
        return write!(out, "   FAULT");
    }
    match feature.value(chip, "input") {
        Some(rpm) => write!(out, "{rpm:4.0} RPM"),
        None => write!(out, "     N/A"),
    }
}

/// A temperature, and its limits two to a line.
fn print_temp(
    chip: &Chip,
    feature: &Feature,
    style: Style,
    width: usize,
    out: &mut dyn Write,
) -> io::Result<()> {
    if feature.value(chip, "fault").is_some_and(|fault| fault != 0.0) {
        write!(out, "   FAULT  ")?;
    } else {
        match feature.value(chip, "input") {
            Some(celsius) => write!(out, "{:+6.1}{}  ", style.degrees(celsius), style.unit())?,
            None => write!(out, "     N/A  ")?,
        }
    }
    let limits: Vec<(&str, f64)> = [("low", "min"), ("high", "max"), ("crit", "crit")]
        .into_iter()
        .filter_map(|(name, file)| Some((name, feature.value(chip, file)?)))
        .collect();
    for (slot, (name, celsius)) in limits.iter().enumerate() {
        if slot % 2 == 0 {
            if slot > 0 {
                write!(out, "\n{:1$}", "", width + 10)?;
            }
            write!(out, "(")?;
        } else {
            write!(out, ", ")?;
        }
        write!(out, "{name:<4} = {:+5.1}{}", style.degrees(*celsius), style.unit())?;
        if slot % 2 == 1 || slot + 1 == limits.len() {
            write!(out, ")")?;
        }
    }
    let alarms: Vec<&str> = [("crit_alarm", "CRIT"), ("alarm", "")]
        .into_iter()
        .filter(|(file, _)| feature.value(chip, file).is_some_and(|alarm| alarm != 0.0))
        .map(|(_, name)| name)
        .collect();
    if !alarms.is_empty() {
        let named: Vec<&str> = alarms.into_iter().filter(|name| !name.is_empty()).collect();
        if named.is_empty() {
            write!(out, "  ALARM")?;
        } else {
            write!(out, "  ALARM ({})", named.join(", "))?;
        }
    }
    Ok(())
}

/// `chip`'s files and their values, as `sensors -u` prints them.
fn print_raw(chip: &Chip, style: Style, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "{}", chip.name())?;
    if !style.no_adapter {
        writeln!(out, "Adapter: {}", chip.adapter())?;
    }
    for feature in features(chip) {
        writeln!(out, "{}:", feature.label(chip))?;
        for file in feature.files.iter().filter(|file| *file != "label") {
            if let Some(value) = feature.value(chip, file) {
                writeln!(out, "  {}_{file}: {value:.3}", feature.name())?;
            }
        }
    }
    writeln!(out)
}

/// The chips as `sensors -j` prints them, in JSON.
fn print_json(chips: &[Chip], out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "{{")?;
    for (index, chip) in chips.iter().enumerate() {
        writeln!(out, "   \"{}\":{{", chip.name())?;
        let features = features(chip);
        let comma = if features.is_empty() { "" } else { "," };
        writeln!(out, "      \"Adapter\": \"{}\"{comma}", chip.adapter())?;
        for (at, feature) in features.iter().enumerate() {
            writeln!(out, "      \"{}\":{{", json_text(&feature.label(chip)))?;
            let values: Vec<(String, f64)> = feature
                .files
                .iter()
                .filter(|file| *file != "label")
                .filter_map(|file| {
                    Some((format!("{}_{file}", feature.name()), feature.value(chip, file)?))
                })
                .collect();
            for (n, (name, value)) in values.iter().enumerate() {
                let comma = if n + 1 < values.len() { "," } else { "" };
                writeln!(out, "         \"{name}\": {value:.3}{comma}")?;
            }
            let comma = if at + 1 < features.len() { "," } else { "" };
            writeln!(out, "      }}{comma}")?;
        }
        let comma = if index + 1 < chips.len() { "," } else { "" };
        writeln!(out, "   }}{comma}")?;
    }
    writeln!(out, "}}")
}

/// `text`, with what JSON escapes escaped.
fn json_text(text: &str) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            c if u32::from(c) < 0x20 => escaped.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Whether `name` matches `pattern`, a chip name whose parts may be `*`:
/// `k10temp-*`, `*-isa-*`, `coretemp-isa-0000`.
fn matches(pattern: &str, chip: &Chip) -> bool {
    let mut parts = pattern.splitn(3, '-');
    let prefix = parts.next().unwrap_or("*");
    let bus = parts.next().unwrap_or("*");
    let address = parts.next().unwrap_or("*");
    let prefix_ok = prefix == "*" || prefix == chip.prefix;
    let bus_ok = bus == "*" || bus == chip.bus;
    let address_ok = address == "*"
        || u32::from_str_radix(address, if chip.bus == "acpi" { 10 } else { 16 })
            .is_ok_and(|a| a == chip.address);
    prefix_ok && bus_ok && address_ok
}

/// What sensors was asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Pretty,
    Raw,
    Json,
}

/// Prints the chips in `hwmon` that `patterns` name (all, if none) to
/// `out`, as `mode` and `style` say.
fn run(
    hwmon: &Path,
    patterns: &[String],
    mode: Mode,
    style: Style,
    out: &mut dyn Write,
) -> Result<i32, Fail> {
    let all = chips(hwmon);
    if all.is_empty() {
        return Err(fail(
            "No sensors found!\nThe kernel reads none of this machine's sensors (sensors(1))."
                .to_string(),
        ));
    }
    let chosen: Vec<Chip> = if patterns.is_empty() {
        all
    } else {
        all.into_iter().filter(|chip| patterns.iter().any(|p| matches(p, chip))).collect()
    };
    if chosen.is_empty() {
        return Err(fail("Specified sensor(s) not found!".to_string()));
    }
    match mode {
        Mode::Json => print_json(&chosen, out)?,
        Mode::Raw => chosen.iter().try_for_each(|chip| print_raw(chip, style, out))?,
        Mode::Pretty => chosen.iter().try_for_each(|chip| print_chip(chip, style, out))?,
    }
    Ok(0)
}

/// sensors(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut mode = Mode::Pretty;
    let mut style = Style::default();
    let mut patterns = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('u') => mode = Mode::Raw,
            Arg::Short('j') => mode = Mode::Json,
            Arg::Short('f') => style.fahrenheit = true,
            Arg::Long(ref name) if name == "fahrenheit" => style.fahrenheit = true,
            Arg::Short('A') => style.no_adapter = true,
            Arg::Long(ref name) if name == "no-adapter" => style.no_adapter = true,
            Arg::Short('h') => {
                return Err(usage(
                    "Usage: sensors [OPTION]... [CHIP]...\n  -f, --fahrenheit  temperatures \
                     in degrees Fahrenheit\n  -A, --no-adapter  leave out each chip's adapter\n  \
                     -u                raw output\n  -j                JSON output"
                        .to_string(),
                ));
            }
            Arg::Operand(chip) => patterns.push(chip.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    let mut out = io::stdout().lock();
    run(Path::new(HWMON), &patterns, mode, style, &mut out)
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;

    /// A machine's hardware monitors: the thermal zones, AMD's sensors,
    /// one of Intel's packages and a Framework EC.
    fn hwmon() -> &'static Path {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("sensors-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let monitors: [&[(&str, &str)]; 4] = [
                &[("name", "acpitz"), ("temp1_input", "27800"), ("temp1_crit", "105000")],
                &[
                    ("name", "k10temp"),
                    ("temp1_input", "45875"),
                    ("temp1_label", "Tctl"),
                    ("temp3_input", "43250"),
                    ("temp3_label", "Tccd1"),
                ],
                &[
                    ("name", "coretemp"),
                    ("temp1_label", "Package id 0"),
                    ("temp1_input", "44000"),
                    ("temp1_crit", "100000"),
                    ("temp1_max", "80000"),
                    ("temp1_crit_alarm", "0"),
                    ("temp2_label", "Core 0"),
                    ("temp2_input", "101000"),
                    ("temp2_crit", "100000"),
                    ("temp2_crit_alarm", "1"),
                ],
                &[
                    ("name", "cros_ec"),
                    ("fan1_input", "1190"),
                    ("fan1_fault", "0"),
                    ("fan2_input", "0"),
                    ("fan2_fault", "1"),
                    ("temp1_input", "36900"),
                    ("temp1_fault", "0"),
                    ("temp1_label", "F75303_Local"),
                    ("temp2_input", "0"),
                    ("temp2_fault", "1"),
                    ("temp2_label", "APU"),
                ],
            ];
            for (n, files) in monitors.iter().enumerate() {
                let monitor = dir.join(format!("hwmon{n}"));
                std::fs::create_dir_all(&monitor).unwrap();
                for (name, value) in *files {
                    std::fs::write(monitor.join(name), format!("{value}\n")).unwrap();
                }
            }
            dir
        })
    }

    fn output(patterns: &[&str], mode: Mode, style: Style) -> String {
        let patterns: Vec<String> = patterns.iter().map(ToString::to_string).collect();
        let mut out = Vec::new();
        run(hwmon(), &patterns, mode, style, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn chips_are_named_as_libsensors_names_them() {
        let names: Vec<String> = chips(hwmon()).iter().map(Chip::name).collect();
        assert_eq!(
            names,
            ["acpitz-acpi-0", "k10temp-pci-00c3", "coretemp-isa-0000", "cros_ec-isa-0000"]
        );
    }

    #[test]
    fn values_and_limits_are_printed_as_sensors_prints_them() {
        let text = output(&[], Mode::Pretty, Style::default());
        let expected = [
            "acpitz-acpi-0",
            "Adapter: ACPI interface",
            "temp1:        +27.8°C  (crit = +105.0°C)",
            "",
            "k10temp-pci-00c3",
            "Adapter: PCI adapter",
            "Tctl:         +45.9°C  ",
            "Tccd1:        +43.2°C  ",
            "",
            "coretemp-isa-0000",
            "Adapter: ISA adapter",
            "Package id 0:  +44.0°C  (high = +80.0°C, crit = +100.0°C)",
            "Core 0:       +101.0°C  (crit = +100.0°C)  ALARM (CRIT)",
            "",
            "cros_ec-isa-0000",
            "Adapter: ISA adapter",
            "fan1:         1190 RPM",
            "fan2:            FAULT",
            "F75303_Local:  +36.9°C  ",
            "APU:             FAULT  ",
            "",
            "",
        ];
        assert_eq!(text, expected.join("\n"));
    }

    #[test]
    fn chips_are_chosen_by_name_and_wildcards() {
        let text =
            output(&["*-isa-*"], Mode::Pretty, Style { no_adapter: true, ..Style::default() });
        assert!(text.starts_with("coretemp-isa-0000\nPackage id 0:"), "{text}");
        assert!(text.contains("\ncros_ec-isa-0000\nfan1:"), "{text}");
        assert!(!text.contains("acpitz"));
        let text = output(&["k10temp-pci-00c3"], Mode::Pretty, Style::default());
        assert!(text.starts_with("k10temp-pci-00c3\n"));
        let mut out = Vec::new();
        let missing =
            run(hwmon(), &["nct6775-*".to_string()], Mode::Pretty, Style::default(), &mut out);
        assert!(missing.is_err());
    }

    #[test]
    fn raw_json_and_fahrenheit() {
        let text = output(&["acpitz-*"], Mode::Raw, Style::default());
        assert_eq!(
            text,
            "acpitz-acpi-0\nAdapter: ACPI interface\ntemp1:\n  temp1_input: 27.800\n  temp1_crit: 105.000\n\n"
        );
        let text = output(&["acpitz-*"], Mode::Json, Style::default());
        assert_eq!(
            text,
            "{\n   \"acpitz-acpi-0\":{\n      \"Adapter\": \"ACPI interface\",\n      \"temp1\":{\n         \
             \"temp1_input\": 27.800,\n         \"temp1_crit\": 105.000\n      }\n   }\n}\n"
        );
        let text =
            output(&["acpitz-*"], Mode::Pretty, Style { fahrenheit: true, ..Style::default() });
        assert!(text.contains("temp1:        +82.0°F  (crit = +221.0°F)"), "{text}");
    }

    #[test]
    fn a_machine_without_sensors_says_so() {
        let empty = std::env::temp_dir().join(format!("sensors-none-{}", std::process::id()));
        let mut out = Vec::new();
        let result = run(&empty, &[], Mode::Pretty, Style::default(), &mut out);
        assert!(result.is_err_and(|fail| fail.message.starts_with("No sensors found!")));
    }
}
