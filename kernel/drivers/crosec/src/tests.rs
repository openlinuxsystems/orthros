// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The EC against a simulated Framework Desktop's: its memory map at
//! 0xe00, and the host commands it answers.

use std::collections::BTreeMap;
use std::string::String;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use super::*;

/// The simulated EC's state.
struct State {
    memmap: [u8; 256],
    packet: [u8; PACKET_SIZE],
    result: u8,
    /// Status reads left that say busy.
    busy_for: u32,
    /// Busy after each command for this many status reads.
    busy_after: u32,
    /// Whether it always stays busy.
    hung: bool,
    /// Whether it gets its responses' checksums wrong.
    bad_checksums: bool,
    /// Whether it is a Chromebook's, without Framework's fan curves and
    /// with no ARGB header.
    chromebook: bool,
    waited_us: u64,
    commands: Vec<(u16, u8, Vec<u8>)>,
    duty: BTreeMap<u8, u8>,
    automatic: BTreeMap<u8, bool>,
    curves: BTreeMap<u8, [u8; 7]>,
    leds: BTreeMap<u8, Option<[u8; 6]>>,
    rgb: [[u8; 3]; RGB_LEDS],
}

#[derive(Clone)]
struct Simulated(Arc<Mutex<State>>);

impl Simulated {
    fn new() -> Simulated {
        let mut memmap = [0xff; 256];
        memmap[0x20] = b'E';
        memmap[0x21] = b'C';
        memmap[0x23] = 2;
        memmap[0x27] = HOST_CMD_FLAG_VERSION_3 | 1;
        let mut curves = BTreeMap::new();
        for fan in 0..3 {
            curves.insert(fan, [100, 20, 85, 45, 0, 3, 0]);
        }
        Simulated(Arc::new(Mutex::new(State {
            memmap,
            packet: [0; PACKET_SIZE],
            result: 0,
            busy_for: 0,
            busy_after: 3,
            hung: false,
            bad_checksums: false,
            chromebook: false,
            waited_us: 0,
            commands: Vec::new(),
            duty: BTreeMap::new(),
            automatic: BTreeMap::new(),
            curves,
            leds: BTreeMap::from([(Led::Power as u8, None), (Led::SecondPower as u8, None)]),
            rgb: [[0; 3]; RGB_LEDS],
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }
}

impl State {
    /// Runs the request at 0x800 and writes its response there.
    fn run(&mut self) {
        let header = &self.packet[..HEADER];
        let command = u16::from_le_bytes([header[2], header[3]]);
        let version = header[4];
        let len = usize::from(u16::from_le_bytes([header[6], header[7]]));
        let (status, data) = if header[0] != PACKET_VERSION || HEADER + len > PACKET_SIZE {
            (12, Vec::new())
        } else if sum(&self.packet[..HEADER + len]) != 0 {
            (7, Vec::new())
        } else {
            let params = self.packet[HEADER..HEADER + len].to_vec();
            self.commands.push((command, version, params.clone()));
            self.answer(command, version, &params)
        };
        let mut response = [PACKET_VERSION, 0, 0, 0, 0, 0, 0, 0];
        response[2..4].copy_from_slice(&u16::from(status).to_le_bytes());
        response[4..6].copy_from_slice(&(data.len() as u16).to_le_bytes());
        let mut checksum = sum(&response).wrapping_add(sum(&data)).wrapping_neg();
        if self.bad_checksums {
            checksum = checksum.wrapping_add(1);
        }
        response[1] = checksum;
        self.packet[..HEADER].copy_from_slice(&response);
        self.packet[HEADER..HEADER + data.len()].copy_from_slice(&data);
        self.result = status;
        self.busy_for = self.busy_after;
    }

    fn answer(&mut self, command: u16, version: u8, params: &[u8]) -> (u8, Vec<u8>) {
        match (command, version) {
            (CMD_HELLO, 0) => {
                let given = u32::from_le_bytes(params[..4].try_into().unwrap());
                (0, given.wrapping_add(0x0102_0304).to_le_bytes().to_vec())
            }
            (CMD_GET_VERSION, 0) => {
                let mut data = vec![0; 100];
                data[..14].copy_from_slice(b"dogwood-0.0.10");
                data[32..46].copy_from_slice(b"dogwood-0.0.11");
                data[96] = 2;
                (0, data)
            }
            (CMD_GET_CMD_VERSIONS, 1) if params.len() == 2 => {
                let mask: u32 = match u16::from_le_bytes([params[0], params[1]]) {
                    CMD_HELLO | CMD_GET_VERSION | CMD_TEMP_SENSOR_GET_INFO => 1,
                    CMD_GET_CMD_VERSIONS | CMD_PWM_SET_FAN_DUTY | CMD_THERMAL_AUTO_FAN_CTRL => 3,
                    CMD_LED_CONTROL => 2,
                    CMD_RGBKBD_SET_COLOR | CMD_FAN_CONFIGURATION if !self.chromebook => 1,
                    _ => return (3, Vec::new()),
                };
                (0, mask.to_le_bytes().to_vec())
            }
            (CMD_TEMP_SENSOR_GET_INFO, 0) => {
                let names = ["power-f75303", "memory-f75303", "ambient-f75303", "amd-apu"];
                match names.get(usize::from(params[0])) {
                    Some(name) => {
                        let mut data = vec![0; 33];
                        data[..name.len()].copy_from_slice(name.as_bytes());
                        (0, data)
                    }
                    None => (3, Vec::new()),
                }
            }
            (CMD_PWM_SET_FAN_DUTY, 1) if params.len() == 5 => {
                if params[4] >= 3 {
                    return (3, Vec::new());
                }
                self.duty.insert(params[4], params[0]);
                self.automatic.insert(params[4], false);
                (0, Vec::new())
            }
            (CMD_THERMAL_AUTO_FAN_CTRL, 1) if params.len() == 1 && params[0] < 3 => {
                self.automatic.insert(params[0], true);
                (0, Vec::new())
            }
            (CMD_FAN_CONFIGURATION, 0) if params.len() == 9 => {
                let Some(curve) = self.curves.get_mut(&params[0]) else { return (3, Vec::new()) };
                if params[1] == 1 {
                    curve.copy_from_slice(&params[2..]);
                }
                (0, curve.to_vec())
            }
            (CMD_LED_CONTROL, 1) if params.len() == 8 => {
                let Some(led) = self.leds.get_mut(&params[0]) else { return (3, Vec::new()) };
                let range = if params[0] == Led::Power as u8 {
                    [0, 0, 0, 0, 100, 0]
                } else {
                    [0, 0, 0, 0, 0, 1]
                };
                if params[1] & LED_AUTO != 0 {
                    *led = None;
                } else if params[1] & LED_QUERY == 0 {
                    *led = Some(params[2..].try_into().unwrap());
                }
                (0, range.to_vec())
            }
            (CMD_RGBKBD_SET_COLOR, 0) => {
                let (first, len) = (usize::from(params[0]), usize::from(params[1]));
                if first + len > RGB_LEDS || params.len() != 2 + 3 * len {
                    return (3, Vec::new());
                }
                for at in 0..len {
                    self.rgb[first + at].copy_from_slice(&params[2 + 3 * at..5 + 3 * at]);
                }
                (0, Vec::new())
            }
            _ => (1, Vec::new()),
        }
    }
}

impl Platform for Simulated {
    fn in8(&self, port: u16) -> u8 {
        let mut state = self.state();
        match port {
            HOST_DATA => state.result,
            HOST_CMD => {
                if state.hung {
                    STATUS_PROCESSING
                } else if state.busy_for > 0 {
                    state.busy_for -= 1;
                    STATUS_PROCESSING
                } else {
                    0
                }
            }
            0x800..0x900 => state.packet[usize::from(port - 0x800)],
            0xe00..0xf00 => state.memmap[usize::from(port - 0xe00)],
            _ => 0xff,
        }
    }

    fn out8(&self, port: u16, value: u8) {
        let mut state = self.state();
        match port {
            HOST_CMD if value == PROTOCOL_3 => state.run(),
            0x800..0x900 => state.packet[usize::from(port - 0x800)] = value,
            _ => panic!("a write of {value:#x} to port {port:#x}"),
        }
    }

    fn delay_us(&self, us: u64) {
        self.state().waited_us += us;
    }
}

/// An empty bus: every port reads all ones.
struct Nothing;

impl Platform for Nothing {
    fn in8(&self, _port: u16) -> u8 {
        0xff
    }

    fn out8(&self, port: u16, _value: u8) {
        panic!("a write to port {port:#x} with no EC");
    }

    fn delay_us(&self, _us: u64) {}
}

fn desktop() -> (Ec<Simulated>, Simulated) {
    let simulated = Simulated::new();
    (Ec::probe(simulated.clone(), FRAMEWORK_MEMMAP).unwrap(), simulated)
}

#[test]
fn an_ec_is_found_only_where_its_memory_map_says_so() {
    assert!(matches!(Ec::probe(Nothing, FRAMEWORK_MEMMAP), Err(Absent::NoMap)));
    // A Framework EC looked for at Chromebooks' memory map.
    assert!(matches!(Ec::probe(Simulated::new(), MEMMAP), Err(Absent::NoMap)));
    let old = Simulated::new();
    old.state().memmap[0x27] = 1;
    assert!(matches!(Ec::probe(old, FRAMEWORK_MEMMAP), Err(Absent::OldProtocol)));
    let (ec, _) = desktop();
    assert_eq!(ec.memmap(), 0xe00);
}

#[test]
fn the_memory_map_gives_temperatures_and_fans() {
    let (ec, simulated) = desktop();
    {
        let mut state = simulated.state();
        // 296 K, 0 K over the offset, an error, not powered; then the
        // second block's first sensor at 353 K.
        state.memmap[..4].copy_from_slice(&[96, 0, 0xfe, 0xfd]);
        state.memmap[0x18] = 153;
        state.memmap[0x10..0x18].copy_from_slice(&[0xb0, 0x04, 0x00, 0x00, 0xfe, 0xff, 0xff, 0xff]);
    }
    assert_eq!(ec.temperature(0), Temperature::Millicelsius(22_850));
    assert_eq!(ec.temperature(1), Temperature::Millicelsius(-73_150));
    assert_eq!(ec.temperature(2), Temperature::Fault);
    assert_eq!(ec.temperature(3), Temperature::Fault);
    assert_eq!(ec.temperature(4), Temperature::Absent);
    assert_eq!(ec.temperature(16), Temperature::Millicelsius(79_850));
    assert_eq!(ec.temperature(TEMP_SENSORS), Temperature::Absent);
    // Without version 2 of the thermal data, no second block.
    simulated.state().memmap[0x23] = 1;
    assert_eq!(ec.temperature(16), Temperature::Absent);
    assert_eq!(ec.fan(0), Fan::Rpm(1200));
    assert_eq!(ec.fan(1), Fan::Rpm(0));
    assert_eq!(ec.fan(2), Fan::Stalled);
    assert_eq!(ec.fan(3), Fan::Absent);
    assert_eq!(ec.fan(FANS), Fan::Absent);
}

#[test]
fn host_commands_are_checksummed_and_waited_for() {
    let (mut ec, simulated) = desktop();
    assert_eq!(ec.hello(), Ok(()));
    let mut version = String::new();
    assert_eq!(ec.version(&mut version), Ok(Image::ReadWrite));
    assert_eq!(version, "dogwood-0.0.11");
    let mut name = String::new();
    assert_eq!(ec.sensor_name(2, &mut name), Ok(()));
    assert_eq!(name, "ambient-f75303");
    assert_eq!(ec.sensor_name(9, &mut String::new()), Err(Error::Status(3)));
    assert_eq!(ec.command(0x7fff, 0, &[], &mut []), Err(Error::Status(1)));
    assert_eq!(ec.command(1, 0, &[0; PACKET_SIZE], &mut []), Err(Error::TooBig));
    {
        let state = simulated.state();
        // Every request reached it whole, and it was waited for.
        assert_eq!(state.commands.len(), 5);
        assert!(state.waited_us >= 5 * 30);
    }
    simulated.state().bad_checksums = true;
    assert_eq!(ec.hello(), Err(Error::BadResponse));
}

#[test]
fn an_ec_that_stays_busy_times_out() {
    let (mut ec, simulated) = desktop();
    simulated.state().hung = true;
    assert_eq!(ec.hello(), Err(Error::Timeout));
    let waited = simulated.state().waited_us;
    assert!((TIMEOUT_US..TIMEOUT_US + 1000).contains(&waited), "waited {waited} µs");
    assert!(simulated.state().commands.is_empty());
}

#[test]
fn fans_take_a_duty_a_curve_and_go_back_to_the_ec() {
    let (mut ec, simulated) = desktop();
    assert_eq!(ec.set_fan_duty(1, 140), Ok(()));
    assert_eq!(simulated.state().duty[&1], 100, "the duty is capped at 100%");
    assert!(!simulated.state().automatic[&1]);
    assert_eq!(ec.auto_fan(1), Ok(()));
    assert!(simulated.state().automatic[&1]);
    assert_eq!(ec.set_fan_duty(5, 50), Err(Error::Status(3)));
    let curve = ec.fan_curve(0).unwrap();
    assert_eq!(
        curve,
        FanCurve {
            max_duty: 100,
            min_duty: 20,
            max_temperature: 85,
            min_temperature: 45,
            always_on: false,
            sensor: 3,
            duty: 0,
        }
    );
    let quieter = FanCurve { max_duty: 80, min_temperature: 55, always_on: true, ..curve };
    assert_eq!(ec.set_fan_curve(2, quieter), Ok(quieter));
    assert_eq!(ec.fan_curve(2), Ok(quieter));
    assert_eq!(ec.fan_curve(0), Ok(curve), "the other fans keep theirs");
    assert_eq!(ec.fan_curve(3), Err(Error::Status(3)));
}

#[test]
fn leds_are_asked_set_and_given_back() {
    let (mut ec, simulated) = desktop();
    assert_eq!(ec.led_range(Led::Power), Ok([0, 0, 0, 0, 100, 0]));
    assert_eq!(ec.led_range(Led::SecondPower), Ok([0, 0, 0, 0, 0, 1]));
    assert_eq!(ec.led_range(Led::Battery), Err(Error::Status(3)));
    assert_eq!(simulated.state().leds[&(Led::Power as u8)], None, "asking sets nothing");
    assert_eq!(ec.set_led(Led::Power, [0, 0, 0, 0, 40, 0]), Ok(()));
    assert_eq!(simulated.state().leds[&(Led::Power as u8)], Some([0, 0, 0, 0, 40, 0]));
    assert_eq!(ec.auto_led(Led::Power), Ok(()));
    assert_eq!(simulated.state().leds[&(Led::Power as u8)], None);
    let names: Vec<&str> = Led::ALL.iter().map(|led| led.name()).collect();
    assert_eq!(names, ["battery", "power", "adapter", "left", "right", "power-2"]);
}

#[test]
fn rgb_colors_go_in_as_many_requests_as_they_take() {
    let (mut ec, simulated) = desktop();
    let colors: Vec<[u8; 3]> = (0..100u8).map(|led| [led, 0x80, 255 - led]).collect();
    assert_eq!(ec.set_rgb(10, &colors), Ok(()));
    {
        let state = simulated.state();
        let starts: Vec<(u8, u8)> =
            state.commands.iter().map(|(_, _, params)| (params[0], params[1])).collect();
        assert_eq!(starts, [(10, 82), (92, 18)]);
        assert_eq!(state.rgb[10], [0, 0x80, 255]);
        assert_eq!(state.rgb[109], [99, 0x80, 156]);
        assert_eq!(state.rgb[110], [0; 3]);
    }
    assert_eq!(ec.set_rgb(100, &colors), Err(Error::TooBig));
    assert_eq!(simulated.state().commands.len(), 2, "nothing is sent past the last LED");
}

#[test]
fn the_ec_says_which_commands_it_takes() {
    let (mut ec, simulated) = desktop();
    assert_eq!(ec.takes(CMD_PWM_SET_FAN_DUTY, 1), Ok(true));
    assert_eq!(ec.takes(CMD_LED_CONTROL, 0), Ok(false), "only version 1");
    assert_eq!(ec.takes(0x7fff, 0), Ok(false), "a command it does not know");
    for control in Control::ALL {
        assert_eq!(ec.has(control), Ok(true), "{control:?}");
    }
    simulated.state().chromebook = true;
    let has: Vec<bool> = Control::ALL.iter().map(|&control| ec.has(control).unwrap()).collect();
    assert_eq!(has, [true, false, true, false]);
    simulated.state().hung = true;
    assert_eq!(ec.has(Control::Leds), Err(Error::Timeout));
    assert_eq!(Led::from_id(7), Some(Led::SecondPower));
    assert_eq!(Led::from_id(5), None);
}
