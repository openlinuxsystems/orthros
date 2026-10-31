// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ectool(8): the commands of the Chromium EC's ectool that set the fans,
//! the LEDs and the ARGB header of a Framework machine's embedded
//! controller (ADR 0208), with ectool's syntax and output, and `fancurve`
//! for the Framework Desktop's fan curves, which ectool lacks.
//!
//! ChromeOS's ectool sends the EC whatever host command it is given;
//! this one asks the kernel, through the platform capability in
//! SLOT_PLATFORM (radix-platform(2)), which a program has only when it
//! was given admin.hardware. The fans' speeds and the sensors' names come
//! from `cros_ec`'s directory in `/kernel/class/hwmon`, which anyone may
//! read.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use radix_abi::Status;
use radix_abi::errors::{File as FileError, Kernel};
use radix_abi::platform::{self, Controls, Led};
use radix_abi::startup::SLOT_PLATFORM;

use crate::args::{Arg, Args};
use crate::sys::SYS;
use crate::util::{Fail, fail, usage};

/// Where the kernel shows its hardware monitors.
const HWMON: &str = "/kernel/class/hwmon";

/// The LEDs' colors, in the EC's order.
const COLORS: [&str; 6] = ["red", "green", "blue", "yellow", "white", "amber"];

/// The commands, with their help as ectool prints it.
const COMMANDS: &[(&str, &str)] = &[
    ("autofanctrl", "[idx]\n\tTurn on automatic fan speed control."),
    (
        "fancurve",
        "<idx> [<key>=<value>...]\n\tPrint a fan's curve, or set min_temp, max_temp, min_duty,\n\t\
         max_duty, sensor and always_on (Framework Desktop).",
    ),
    ("fanduty", "<percent>\n\tForces the fan PWM to a constant duty cycle."),
    (
        "led",
        "<name> <query | auto | off | <color> | <color>=<value>...>\n\t\
         Set the color of an LED or query brightness range.",
    ),
    ("pwmgetfanrpm", "[<index> | all]\n\tPrints current fan RPM."),
    ("pwmgetnumfans", "\n\tPrints the number of fans present."),
    ("rgbkbd", "...\n\tSet the colors of the ARGB header's or RGB keyboard's LEDs."),
];

/// What ectool asks the kernel: radix-platform(2)'s methods, so that the
/// tests can answer instead.
pub trait Platform {
    fn describe(&self) -> Result<platform::DescribeResults, Status>;
    fn set_fan_duty(&self, fan: u32, percent: u32) -> Result<(), Status>;
    fn auto_fan(&self, fan: u32) -> Result<(), Status>;
    fn fan_curve(&self, fan: u32) -> Result<platform::FanCurveResults, Status>;
    fn set_fan_curve(&self, fan: u32, curve: &platform::FanCurveResults) -> Result<(), Status>;
    fn led(&self, led: Led) -> Result<[u32; 6], Status>;
    fn set_led(&self, led: Led, brightness: [u32; 6]) -> Result<(), Status>;
    fn auto_led(&self, led: Led) -> Result<(), Status>;
    fn set_rgb(&self, first: u32, colors: &[u8]) -> Result<(), Status>;
}

/// The kernel, through the capability in SLOT_PLATFORM.
struct Slot;

impl Platform for Slot {
    fn describe(&self) -> Result<platform::DescribeResults, Status> {
        platform::describe(&SYS, SLOT_PLATFORM)
    }

    fn set_fan_duty(&self, fan: u32, percent: u32) -> Result<(), Status> {
        platform::set_fan_duty(&SYS, SLOT_PLATFORM, fan, percent)
    }

    fn auto_fan(&self, fan: u32) -> Result<(), Status> {
        platform::auto_fan(&SYS, SLOT_PLATFORM, fan)
    }

    fn fan_curve(&self, fan: u32) -> Result<platform::FanCurveResults, Status> {
        platform::fan_curve(&SYS, SLOT_PLATFORM, fan)
    }

    fn set_fan_curve(&self, fan: u32, c: &platform::FanCurveResults) -> Result<(), Status> {
        platform::set_fan_curve(
            &SYS,
            SLOT_PLATFORM,
            fan,
            c.min_temperature,
            c.max_temperature,
            c.min_duty,
            c.max_duty,
            c.always_on,
            c.sensor,
        )
    }

    fn led(&self, led: Led) -> Result<[u32; 6], Status> {
        let r = platform::led(&SYS, SLOT_PLATFORM, led)?;
        Ok([r.red, r.green, r.blue, r.yellow, r.white, r.amber])
    }

    fn set_led(&self, led: Led, b: [u32; 6]) -> Result<(), Status> {
        platform::set_led(&SYS, SLOT_PLATFORM, led, b[0], b[1], b[2], b[3], b[4], b[5])
    }

    fn auto_led(&self, led: Led) -> Result<(), Status> {
        platform::auto_led(&SYS, SLOT_PLATFORM, led)
    }

    fn set_rgb(&self, first: u32, colors: &[u8]) -> Result<(), Status> {
        platform::set_rgb(&SYS, SLOT_PLATFORM, first, colors)
    }
}

/// What a status says, for a command that changes or asks the EC.
fn refused(status: Status) -> Fail {
    let why = match Kernel::from_status(status) {
        Some(Kernel::NoSuchSlot | Kernel::EmptySlot | Kernel::WrongKind) => {
            "this program holds no platform capability: it needs admin.hardware (spark(8))"
        }
        Some(Kernel::MissingRights) => "its platform capability does not allow this",
        Some(Kernel::NotSupported) => "the embedded controller does not take this command",
        Some(Kernel::OutOfRange) => "the embedded controller has no such fan, sensor or LED",
        Some(Kernel::InvalidArgument) => "the embedded controller does not take these values",
        Some(Kernel::TimedOut) => "the embedded controller did not answer",
        _ if FileError::from_status(status) == Some(FileError::Io) => {
            "the embedded controller refused the command"
        }
        _ => return fail(format!("the kernel refused: {}", crate::sys::status_name(status.0))),
    };
    fail(why.to_string())
}

/// What the EC has, or why there is none to ask.
fn describe(platform: &dyn Platform) -> Result<platform::DescribeResults, Fail> {
    platform.describe().map_err(|status| match Kernel::from_status(status) {
        Some(Kernel::NotSupported) => fail("the kernel drives no embedded controller".to_string()),
        _ => refused(status),
    })
}

/// A number as C's strtol reads one with base 0: decimal, `0x` hex or
/// `0` octal, with a sign.
fn number(text: &str) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let value = if let Some(hex) = digits.strip_prefix("0x").or(digits.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()?
    } else if digits.len() > 1 && digits.starts_with('0') {
        i64::from_str_radix(&digits[1..], 8).ok()?
    } else {
        digits.parse().ok()?
    };
    Some(if negative { -value } else { value })
}

/// The fans `fans` has, a bit each, as ectool counts them: those up to
/// the first missing one.
fn fan_count(fans: u32) -> u32 {
    fans.trailing_ones()
}

/// `cros_ec`'s directory under `hwmon`, if the kernel shows one.
fn cros_ec(hwmon: &Path) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        std::fs::read_dir(hwmon).ok()?.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs.into_iter().find(|dir| read(dir, "name").as_deref() == Some("cros_ec"))
}

/// The file `name` of `dir`, without its newline.
fn read(dir: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(name)).ok()?;
    Some(text.trim_end().to_string())
}

/// ectool(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    args.stop_at_operand();
    let mut words = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Operand(word) => words.push(word.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    let mut out = io::stdout().lock();
    run(&Slot, Path::new(HWMON), &words, &mut out)
}

/// Runs the command in `words` against `platform`, with the hardware
/// monitors in `hwmon`, writing to `out`.
pub fn run(
    platform: &dyn Platform,
    hwmon: &Path,
    words: &[String],
    out: &mut dyn Write,
) -> Result<i32, Fail> {
    let Some((command, rest)) = words.split_first() else {
        writeln!(out, "Usage: ectool <command> [params]\n")?;
        writeln!(out, "Use 'ectool help' to print a list of commands.")?;
        return Ok(1);
    };
    let command = command.to_ascii_lowercase();
    match command.as_str() {
        "help" => {
            writeln!(out, "Usage: ectool <command> [params]\n\nCommands:")?;
            for (name, help) in COMMANDS {
                writeln!(out, "  {name} {help}")?;
            }
            Ok(1)
        }
        "autofanctrl" => autofanctrl(platform, rest, out),
        "fancurve" => fancurve(platform, hwmon, rest, out),
        "fanduty" => fanduty(platform, rest, out),
        "led" => led(platform, rest, out),
        "pwmgetfanrpm" => pwmgetfanrpm(hwmon, rest, out),
        "pwmgetnumfans" => {
            let fans = fans_shown(hwmon);
            writeln!(out, "Number of fans = {}", fan_count(fans))?;
            Ok(0)
        }
        "rgbkbd" => rgbkbd(platform, rest),
        _ => {
            writeln!(out, "Usage: ectool <command> [params]\n")?;
            writeln!(out, "Use 'ectool help' to print a list of commands.")?;
            Err(fail(format!("Unknown command '{command}'")))
        }
    }
}

/// `fanduty [idx] <percent>`.
fn fanduty(platform: &dyn Platform, rest: &[String], out: &mut dyn Write) -> Result<i32, Fail> {
    if rest.is_empty() || rest.len() > 2 || (rest.len() == 1 && rest[0] == "help") {
        writeln!(out, "Usage: fanduty [idx] <percent>")?;
        writeln!(out, "'fanduty 0 50' - Set fan 0 duty cycle to 50 percent")?;
        writeln!(out, "'fanduty 30' - Set all fans duty cycle to 30 percent")?;
        return Ok(1);
    }
    let found = describe(platform)?;
    let percent = number(&rest[rest.len() - 1])
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| fail("Bad percent arg.".to_string()))?;
    if rest.len() == 1 {
        for fan in (0..32).filter(|fan| found.fans & 1 << fan != 0) {
            platform.set_fan_duty(fan, percent).map_err(refused)?;
        }
        writeln!(out, "Fan duty cycle set for all fans.")?;
    } else {
        let fan = fan_index(&rest[0], found.fans)?;
        platform.set_fan_duty(fan, percent).map_err(refused)?;
        writeln!(out, "Fan {fan} duty cycle set.")?;
    }
    Ok(0)
}

/// The fan `text` names, if the EC has it.
fn fan_index(text: &str, fans: u32) -> Result<u32, Fail> {
    number(text)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&fan| fan < fan_count(fans))
        .ok_or_else(|| fail("Bad fan index.".to_string()))
}

/// `autofanctrl [idx]`.
fn autofanctrl(platform: &dyn Platform, rest: &[String], out: &mut dyn Write) -> Result<i32, Fail> {
    if rest.len() > 1 || rest.first().is_some_and(|word| word == "help") {
        writeln!(out, "Usage: autofanctrl [idx]")?;
        return Ok(1);
    }
    let found = describe(platform)?;
    match rest.first() {
        None => {
            for fan in (0..32).filter(|fan| found.fans & 1 << fan != 0) {
                platform.auto_fan(fan).map_err(refused)?;
            }
            writeln!(out, "Automatic fan control is now on for all fans.")?;
        }
        Some(text) => {
            let fan = fan_index(text, found.fans)?;
            platform.auto_fan(fan).map_err(refused)?;
            writeln!(out, "Automatic fan control is now on for fan {fan}")?;
        }
    }
    Ok(0)
}

/// The fans `cros_ec` shows, a bit each, by their numbers from 0.
fn fans_shown(hwmon: &Path) -> u32 {
    let Some(dir) = cros_ec(hwmon) else { return 0 };
    (0..32)
        .filter(|fan| dir.join(format!("fan{}_input", fan + 1)).exists())
        .fold(0, |bits, fan| bits | 1 << fan)
}

/// `pwmgetfanrpm [<index> | all]`, from `cros_ec`'s files.
fn pwmgetfanrpm(hwmon: &Path, rest: &[String], out: &mut dyn Write) -> Result<i32, Fail> {
    let dir =
        cros_ec(hwmon).ok_or_else(|| fail("the kernel drives no embedded controller".into()))?;
    let count = fan_count(fans_shown(hwmon));
    let fans: Vec<u32> = match rest.first() {
        None => (0..count).collect(),
        Some(word) if word.eq_ignore_ascii_case("all") => (0..count).collect(),
        Some(word) => {
            let fan = number(word).and_then(|n| u32::try_from(n).ok()).filter(|&fan| fan < count);
            vec![fan.ok_or_else(|| fail("Bad index.".to_string()))?]
        }
    };
    for fan in fans {
        let number = fan + 1;
        let rpm = read(&dir, &format!("fan{number}_input")).unwrap_or_default();
        if read(&dir, &format!("fan{number}_fault")).as_deref() == Some("1") {
            // The memory map's word for a stalled fan, as ectool prints it.
            writeln!(out, "Fan {fan} stalled (RPM: 65534)")?;
        } else {
            writeln!(out, "Fan {fan} RPM: {rpm}")?;
        }
    }
    Ok(0)
}

/// `fancurve <idx> [<key>=<value>...]`.
fn fancurve(
    platform: &dyn Platform,
    hwmon: &Path,
    rest: &[String],
    out: &mut dyn Write,
) -> Result<i32, Fail> {
    let Some((fan, settings)) = rest.split_first().filter(|(fan, _)| *fan != "help") else {
        writeln!(out, "Usage: fancurve <idx> [<key>=<value>...]")?;
        writeln!(out, "Keys: min_temp, max_temp (degrees Celsius), min_duty, max_duty")?;
        writeln!(out, "(percent), sensor (its number) and always_on (0 or 1).")?;
        writeln!(out, "'fancurve 0 min_temp=45 max_temp=80' - Fan 0 starts at 45 C")?;
        writeln!(out, "and runs at its maximum duty from 80 C")?;
        return Ok(1);
    };
    let found = describe(platform)?;
    if !found.controls.contains(Controls::FAN_CURVE) {
        return Err(fail("the embedded controller has no fan curves".to_string()));
    }
    let fan = fan_index(fan, found.fans)?;
    let mut curve = platform.fan_curve(fan).map_err(refused)?;
    if settings.is_empty() {
        let label = read_label(hwmon, curve.sensor);
        writeln!(out, "Fan {fan} curve:")?;
        writeln!(out, "\tDuty now:\t{}%", curve.duty)?;
        writeln!(out, "\tmin_temp:\t{} C", curve.min_temperature)?;
        writeln!(out, "\tmax_temp:\t{} C", curve.max_temperature)?;
        writeln!(out, "\tmin_duty:\t{}%", curve.min_duty)?;
        writeln!(out, "\tmax_duty:\t{}%", curve.max_duty)?;
        writeln!(out, "\tsensor:\t\t{}{label}", curve.sensor)?;
        writeln!(out, "\talways_on:\t{}", u8::from(curve.always_on))?;
        return Ok(0);
    }
    for setting in settings {
        let Some((key, value)) = setting.split_once('=') else {
            return Err(fail(format!("Missing value: {setting}")));
        };
        let value = number(value)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| fail(format!("Bad value: {setting}")))?;
        match key.to_ascii_lowercase().as_str() {
            "min_temp" => curve.min_temperature = value,
            "max_temp" => curve.max_temperature = value,
            "min_duty" => curve.min_duty = value,
            "max_duty" => curve.max_duty = value,
            "sensor" => curve.sensor = value,
            "always_on" => curve.always_on = value != 0,
            _ => return Err(fail(format!("Bad key: {key}"))),
        }
    }
    platform.set_fan_curve(fan, &curve).map_err(refused)?;
    writeln!(out, "Fan {fan} curve set.")?;
    Ok(0)
}

/// ` (name)` for sensor `sensor`, from `cros_ec`'s labels, or nothing.
fn read_label(hwmon: &Path, sensor: u32) -> String {
    let label = cros_ec(hwmon).and_then(|dir| read(&dir, &format!("temp{}_label", sensor + 1)));
    label.map_or_default(|label| format!(" ({label})"))
}

/// `led <name> <query | auto | off | <color> | <color>=<value>...>`.
fn led(platform: &dyn Platform, rest: &[String], out: &mut dyn Write) -> Result<i32, Fail> {
    if rest.len() < 2 {
        return Err(usage(
            "Usage: led <name> <query | auto | off | <color> | <color>=<value>...>".to_string(),
        ));
    }
    let Some(led) = Led::ALL.into_iter().find(|led| led.name().eq_ignore_ascii_case(&rest[0]))
    else {
        let names: Vec<&str> = Led::ALL.iter().map(|led| led.name()).collect();
        return Err(fail(format!(
            "Bad LED name: {}\nValid LED names: {} ",
            rest[0],
            names.join(" ")
        )));
    };
    describe(platform)?;
    let what = rest[1].to_ascii_lowercase();
    match what.as_str() {
        "query" => {
            writeln!(out, "Brightness range for LED {}:", led.raw())?;
            let range = platform.led(led).map_err(|status| match Kernel::from_status(status) {
                Some(Kernel::OutOfRange) => fail("Error: Unsupported LED.".to_string()),
                _ => refused(status),
            })?;
            for (color, brightest) in COLORS.iter().zip(range) {
                writeln!(out, "\t{color}\t: {brightest:#x}")?;
            }
            return Ok(0);
        }
        "auto" => platform.auto_led(led).map_err(refused)?,
        "off" => platform.set_led(led, [0; 6]).map_err(refused)?,
        _ => {
            let mut brightness = [0; 6];
            if let Some(at) = COLORS.iter().position(|color| *color == what) {
                brightness[at] = 0xff;
            } else {
                for setting in &rest[1..] {
                    let (name, value) = match setting.split_once('=') {
                        Some((name, value)) => (name, Some(value)),
                        None => (setting.as_str(), None),
                    };
                    let Some(at) = COLORS.iter().position(|c| c.eq_ignore_ascii_case(name)) else {
                        return Err(fail(format!(
                            "Bad color name: {name}\nValid colors: {} ",
                            COLORS.join(" ")
                        )));
                    };
                    let value = value.ok_or_else(|| fail("Missing brightness value".into()))?;
                    brightness[at] = number(value)
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| fail(format!("Bad brightness: {value}")))?;
                }
            }
            platform.set_led(led, brightness).map_err(refused)?;
        }
    }
    Ok(0)
}

/// rgbkbd's help, as ectool prints it, for what this one does.
const RGBKBD_HELP: &str = "  Usage1: rgbkbd <key> <RGB> [<RGB> ...]
          Set the color of <key> to <RGB>. Multiple colors for
          adjacent keys can be set at once.

  Usage2: rgbkbd clear <RGB>
          Set the color of all keys to <RGB>.
";

/// A color as `0xRRGGBB`, or any number C's strtoul reads.
fn rgb(text: &str) -> Result<[u8; 3], Fail> {
    match number(text) {
        Some(value @ 0..=0xff_ffff) => {
            let [_, r, g, b] = (value as u32).to_be_bytes();
            Ok([r, g, b])
        }
        _ => Err(fail(format!("Invalid color '{text}'."))),
    }
}

/// `rgbkbd <key> <RGB> [<RGB> ...]` and `rgbkbd clear <RGB>`.
fn rgbkbd(platform: &dyn Platform, rest: &[String]) -> Result<i32, Fail> {
    if rest.is_empty() {
        return Err(usage(RGBKBD_HELP.trim_end().to_string()));
    }
    let leds = platform::RGB_LEDS;
    let (first, colors) = if rest.len() == 2 && rest[0].eq_ignore_ascii_case("clear") {
        (0, vec![rgb(&rest[1])?; leds as usize])
    } else {
        let key = number(&rest[0])
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&key| key < leds)
            .ok_or_else(|| fail(format!("Invalid key ID '{}'.", rest[0])))?;
        if rest.len() - 1 > leds as usize {
            return Err(fail("# of colors exceed max key count.".to_string()));
        }
        let colors = rest[1..].iter().map(|text| rgb(text)).collect::<Result<Vec<_>, _>>()?;
        (key, colors)
    };
    let found = describe(platform)?;
    if !found.controls.contains(Controls::RGB) {
        return Err(fail("the embedded controller has no addressable LEDs".to_string()));
    }
    platform.set_rgb(first, colors.as_flattened()).map_err(refused)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// A Framework Desktop's EC, as the kernel would answer for it.
    struct Desktop {
        calls: RefCell<Vec<String>>,
        curve: RefCell<platform::FanCurveResults>,
        controls: Controls,
        given: bool,
    }

    impl Desktop {
        fn new() -> Desktop {
            let curve = platform::FanCurveResults {
                min_temperature: 40,
                max_temperature: 54,
                min_duty: 20,
                max_duty: 100,
                always_on: false,
                sensor: 1,
                duty: 35,
            };
            Desktop {
                calls: RefCell::new(Vec::new()),
                curve: RefCell::new(curve),
                controls: Controls::ALL,
                given: true,
            }
        }

        fn call(&self, call: String) -> Result<(), Status> {
            if !self.given {
                return Err(Kernel::EmptySlot.status(0));
            }
            self.calls.borrow_mut().push(call);
            Ok(())
        }
    }

    impl Platform for Desktop {
        fn describe(&self) -> Result<platform::DescribeResults, Status> {
            if !self.given {
                return Err(Kernel::EmptySlot.status(0));
            }
            Ok(platform::DescribeResults {
                controls: self.controls,
                fans: 0b111,
                sensors: 0b11111,
                leds: 1 << 1 | 1 << 7,
            })
        }

        fn set_fan_duty(&self, fan: u32, percent: u32) -> Result<(), Status> {
            self.call(format!("duty {fan} {percent}"))
        }

        fn auto_fan(&self, fan: u32) -> Result<(), Status> {
            self.call(format!("auto {fan}"))
        }

        fn fan_curve(&self, _fan: u32) -> Result<platform::FanCurveResults, Status> {
            Ok(*self.curve.borrow())
        }

        fn set_fan_curve(&self, fan: u32, c: &platform::FanCurveResults) -> Result<(), Status> {
            *self.curve.borrow_mut() = *c;
            self.call(format!(
                "curve {fan} {}-{} C {}-{}% sensor {} on {}",
                c.min_temperature, c.max_temperature, c.min_duty, c.max_duty, c.sensor, c.always_on
            ))
        }

        fn led(&self, led: Led) -> Result<[u32; 6], Status> {
            match led {
                Led::Power => Ok([0, 0, 0, 0, 100, 0]),
                Led::SecondPower => Ok([0, 0, 0, 0, 0, 1]),
                _ => Err(Kernel::OutOfRange.status(0)),
            }
        }

        fn set_led(&self, led: Led, b: [u32; 6]) -> Result<(), Status> {
            self.call(format!("led {} {b:?}", led.name()))
        }

        fn auto_led(&self, led: Led) -> Result<(), Status> {
            self.call(format!("led {} auto", led.name()))
        }

        fn set_rgb(&self, first: u32, colors: &[u8]) -> Result<(), Status> {
            self.call(format!("rgb {first} {} {:02x?}", colors.len() / 3, &colors[..3]))
        }
    }

    /// A hwmon directory with `cros_ec`'s files, and an `acpitz` first,
    /// made once for the tests, which run at once.
    fn hwmon() -> PathBuf {
        static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        DIR.get_or_init(make_hwmon).clone()
    }

    fn make_hwmon() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ectool-test-{}", std::process::id()));
        let files: [(&str, &str); 9] = [
            ("hwmon0/name", "acpitz"),
            ("hwmon1/name", "cros_ec"),
            ("hwmon1/fan1_input", "1200"),
            ("hwmon1/fan1_fault", "0"),
            ("hwmon1/fan2_input", "0"),
            ("hwmon1/fan2_fault", "1"),
            ("hwmon1/fan3_input", "860"),
            ("hwmon1/fan3_fault", "0"),
            ("hwmon1/temp2_label", "memory-f75303"),
        ];
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{text}\n")).unwrap();
        }
        dir
    }

    /// Runs `line` against `platform`: the exit status, or the failure's
    /// message, and what it wrote.
    fn ectool(platform: &dyn Platform, line: &str) -> (Result<i32, String>, String) {
        let words: Vec<String> = line.split_whitespace().map(String::from).collect();
        let mut out = Vec::new();
        let result = run(platform, &hwmon(), &words, &mut out).map_err(|fail| fail.message);
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn numbers_are_read_as_strtol_reads_them() {
        assert_eq!(number("50"), Some(50));
        assert_eq!(number("0x1f"), Some(31));
        assert_eq!(number("010"), Some(8));
        assert_eq!(number("0"), Some(0));
        assert_eq!(number("-3"), Some(-3));
        assert_eq!(number("5%"), None);
        assert_eq!(rgb("0xff8000").ok(), Some([0xff, 0x80, 0]));
        assert!(rgb("0x1000000").is_err());
    }

    #[test]
    fn fan_duties_and_automatic_control() {
        let desktop = Desktop::new();
        let (result, out) = ectool(&desktop, "fanduty 30");
        assert_eq!((result, out.as_str()), (Ok(0), "Fan duty cycle set for all fans.\n"));
        let (result, out) = ectool(&desktop, "fanduty 2 60");
        assert_eq!((result, out.as_str()), (Ok(0), "Fan 2 duty cycle set.\n"));
        assert_eq!(ectool(&desktop, "fanduty 3 60").0, Err("Bad fan index.".to_string()));
        assert_eq!(ectool(&desktop, "fanduty 1 half").0, Err("Bad percent arg.".to_string()));
        let (result, out) = ectool(&desktop, "autofanctrl 1");
        assert_eq!((result, out.as_str()), (Ok(0), "Automatic fan control is now on for fan 1\n"));
        let (result, out) = ectool(&desktop, "autofanctrl");
        assert_eq!(result, Ok(0));
        assert_eq!(out, "Automatic fan control is now on for all fans.\n");
        assert_eq!(
            *desktop.calls.borrow(),
            [
                "duty 0 30",
                "duty 1 30",
                "duty 2 30",
                "duty 2 60",
                "auto 1",
                "auto 0",
                "auto 1",
                "auto 2"
            ]
        );
        let (result, out) = ectool(&desktop, "fanduty");
        assert_eq!(result, Ok(1));
        assert!(out.starts_with("Usage: fanduty [idx] <percent>\n"));
    }

    #[test]
    fn fan_speeds_come_from_hwmon() {
        let desktop = Desktop::new();
        let (result, out) = ectool(&desktop, "pwmgetfanrpm");
        assert_eq!(result, Ok(0));
        assert_eq!(out, "Fan 0 RPM: 1200\nFan 1 stalled (RPM: 65534)\nFan 2 RPM: 860\n");
        assert_eq!(ectool(&desktop, "pwmgetfanrpm 2").1, "Fan 2 RPM: 860\n");
        assert_eq!(ectool(&desktop, "pwmgetfanrpm 3").0, Err("Bad index.".to_string()));
        assert_eq!(ectool(&desktop, "pwmgetnumfans").1, "Number of fans = 3\n");
    }

    #[test]
    fn fan_curves_are_shown_and_changed_a_key_at_a_time() {
        let desktop = Desktop::new();
        let (result, out) = ectool(&desktop, "fancurve 0");
        assert_eq!(result, Ok(0));
        assert_eq!(
            out,
            "Fan 0 curve:\n\tDuty now:\t35%\n\tmin_temp:\t40 C\n\tmax_temp:\t54 C\n\
             \tmin_duty:\t20%\n\tmax_duty:\t100%\n\tsensor:\t\t1 (memory-f75303)\n\
             \talways_on:\t0\n"
        );
        let (result, out) = ectool(&desktop, "fancurve 1 max_temp=80 always_on=1 min_duty=0x10");
        assert_eq!((result, out.as_str()), (Ok(0), "Fan 1 curve set.\n"));
        assert_eq!(*desktop.calls.borrow(), ["curve 1 40-80 C 16-100% sensor 1 on true"]);
        assert_eq!(ectool(&desktop, "fancurve 1 loud=1").0, Err("Bad key: loud".to_string()));
        let chromebook =
            Desktop { controls: Controls::FAN_DUTY.union(Controls::LEDS), ..Desktop::new() };
        assert_eq!(
            ectool(&chromebook, "fancurve 0").0,
            Err("the embedded controller has no fan curves".to_string())
        );
    }

    #[test]
    fn leds_are_queried_and_set_by_name_and_color() {
        let desktop = Desktop::new();
        let (result, out) = ectool(&desktop, "led power query");
        assert_eq!(result, Ok(0));
        assert_eq!(
            out,
            "Brightness range for LED 1:\n\tred\t: 0x0\n\tgreen\t: 0x0\n\tblue\t: 0x0\n\
             \tyellow\t: 0x0\n\twhite\t: 0x64\n\tamber\t: 0x0\n"
        );
        let (result, out) = ectool(&desktop, "led battery query");
        assert_eq!(
            (result, out.as_str()),
            (Err("Error: Unsupported LED.".to_string()), "Brightness range for LED 0:\n")
        );
        assert_eq!(ectool(&desktop, "led power white").0, Ok(0));
        assert_eq!(ectool(&desktop, "led SECOND_POWER amber=1").0, Ok(0));
        assert_eq!(ectool(&desktop, "led power off").0, Ok(0));
        assert_eq!(ectool(&desktop, "led power auto").0, Ok(0));
        assert_eq!(
            *desktop.calls.borrow(),
            [
                "led power [0, 0, 0, 0, 255, 0]",
                "led second_power [0, 0, 0, 0, 0, 1]",
                "led power [0, 0, 0, 0, 0, 0]",
                "led power auto"
            ]
        );
        let bad = ectool(&desktop, "led lightbar red").0.unwrap_err();
        assert!(bad.starts_with("Bad LED name: lightbar\nValid LED names: battery power"), "{bad}");
        assert_eq!(
            ectool(&desktop, "led power pink=3").0.unwrap_err().lines().next(),
            Some("Bad color name: pink")
        );
    }

    #[test]
    fn rgb_colors_go_to_the_argb_header() {
        let desktop = Desktop::new();
        assert_eq!(ectool(&desktop, "rgbkbd 4 0xff0000 0x00ff00").0, Ok(0));
        assert_eq!(ectool(&desktop, "rgbkbd clear 0x102030").0, Ok(0));
        assert_eq!(*desktop.calls.borrow(), ["rgb 4 2 [ff, 00, 00]", "rgb 0 128 [10, 20, 30]"]);
        assert_eq!(ectool(&desktop, "rgbkbd 128 0").0, Err("Invalid key ID '128'.".to_string()));
        assert_eq!(ectool(&desktop, "rgbkbd 0 red").0, Err("Invalid color 'red'.".to_string()));
    }

    #[test]
    fn a_program_without_the_capability_is_told_what_it_needs() {
        let ungiven = Desktop { given: false, ..Desktop::new() };
        let (result, _) = ectool(&ungiven, "fanduty 50");
        assert_eq!(
            result,
            Err("this program holds no platform capability: it needs admin.hardware (spark(8))"
                .to_string())
        );
        // Reading the fans' speeds needs none.
        assert_eq!(ectool(&ungiven, "pwmgetnumfans").0, Ok(0));
        let (result, out) = ectool(&ungiven, "help");
        assert_eq!(result, Ok(1));
        assert!(out.contains("\n  fanduty <percent>\n\tForces the fan PWM"), "{out}");
        assert_eq!(ectool(&ungiven, "flashread").0, Err("Unknown command 'flashread'".to_string()));
    }
}
