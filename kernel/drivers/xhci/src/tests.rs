// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated xHCI controller: its registers, its
//! command and event rings, root ports that devices are plugged into and
//! pulled from, and devices that answer control transfers on endpoint 0
//! from their descriptors, stall the requests a test names, or never
//! answer. Its layout is QEMU's `qemu-xhci`: USB 3 ports first, then USB 2
//! ones, unless a test gives other protocols.

use std::collections::HashMap;
use std::string::String;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Dma, Mmio, Registers, fake_memory, fake_memory_write};
use radix_usb::{DeviceDescriptor, Setup, Speed, descriptor};

use crate::ring::{RING_TRBS, TRB_SIZE, kind};
use crate::*;

/// Where the simulated controller's register blocks are.
const CAPLENGTH: usize = 0x40;
const RUNTIME: usize = 0x1000;
const XCAPS: usize = 0x2000;
const DOORBELLS: usize = 0x3000;
const REGS: usize = 0x4000;

/// A device of the simulated controller.
#[derive(Clone)]
struct SimDevice {
    /// Its Port Speed ID.
    speed_id: u8,
    device: Vec<u8>,
    config: Vec<u8>,
    strings: HashMap<u8, String>,
    /// The requests it stalls: bRequest and wValue's high byte.
    stalls: Vec<(u8, u8)>,
    /// It never answers.
    silent: bool,
    configuration: u8,
}

/// A keyboard at `speed_id`, with a packet size of `max_packet0` (the
/// descriptor's byte).
fn keyboard(speed_id: u8, max_packet0: u8) -> SimDevice {
    let device = std::vec![
        18, 1, 0x00, 0x02, 0, 0, 0, max_packet0, 0x27, 0x06, 0x01, 0x00, 0x00, 0x00, 1, 4, 11,
        1,
    ];
    let mut config = std::vec![9, 2, 34, 0, 1, 1, 6, 0xa0, 50];
    config.extend([9, 4, 0, 0, 1, 3, 1, 1, 0]);
    config.extend([9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0]);
    config.extend([7, 5, 0x81, 3, 8, 0, 7]);
    let strings = HashMap::from([
        (1, String::from("QEMU")),
        (4, String::from("QEMU USB Keyboard")),
        (11, String::from("68284")),
    ]);
    SimDevice {
        speed_id,
        device,
        config,
        strings,
        stalls: Vec::new(),
        silent: false,
        configuration: 0,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EpState {
    Running,
    Halted,
    Stopped,
}

/// A slot of the simulated controller.
struct SimSlot {
    port: u8,
    addressed: bool,
    route: u32,
    speed_id: u8,
    max_packet: u16,
    /// Endpoint 0's ring: where it reads next, and its cycle state.
    dequeue: u64,
    cycle: bool,
    state: EpState,
    /// A transfer the device has not answered: its first TRB.
    pending: Option<u64>,
}

/// The firmware's hold on the controller.
#[derive(Clone, Copy)]
struct Legacy {
    bios: bool,
    os: bool,
    /// When the firmware lets go once asked, if ever (on the test's
    /// clock).
    lets_go_after: Option<u64>,
    asked_at: Option<u64>,
    smi: u32,
}

/// A port's protocol: its major version, first port, port count and
/// Protocol Speed IDs.
#[derive(Clone)]
struct SimProtocol {
    major: u8,
    first: u8,
    count: u8,
    psi: Vec<u32>,
}

struct State {
    slots_max: u8,
    ports: u8,
    context_size: usize,
    ac64: bool,
    port_power: bool,
    scratchpads: u32,
    protocols: Vec<SimProtocol>,
    legacy: Option<Legacy>,
    /// It never comes out of a reset.
    stuck_in_reset: bool,
    /// It never answers commands.
    deaf: bool,
    now: u64,

    usbcmd: u32,
    usbsts: u32,
    crcr_low: u32,
    crcr_high: u32,
    dcbaap: u64,
    config: u32,
    portsc: Vec<u32>,
    devices: Vec<Option<SimDevice>>,

    iman: u32,
    imod: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    er_base: u64,
    er_size: u64,
    er_enqueue: u64,
    er_cycle: bool,
    dropped: u32,

    cmd_dequeue: u64,
    cmd_cycle: bool,
    cmd_running: bool,
    slots: HashMap<u8, SimSlot>,

    resets: u32,
    commands: Vec<(u32, u8)>,
    requests: Vec<(u8, Setup)>,
    port_writes: Vec<(u8, u32)>,
}

impl State {
    fn new() -> State {
        State {
            slots_max: 64,
            ports: 8,
            context_size: 32,
            ac64: true,
            port_power: false,
            scratchpads: 0,
            protocols: std::vec![
                SimProtocol { major: 3, first: 1, count: 4, psi: Vec::new() },
                SimProtocol { major: 2, first: 5, count: 4, psi: Vec::new() },
            ],
            legacy: None,
            stuck_in_reset: false,
            deaf: false,
            now: 0,
            usbcmd: 0,
            // The firmware left it running.
            usbsts: 0,
            crcr_low: 0,
            crcr_high: 0,
            dcbaap: 0,
            config: 0,
            portsc: std::vec![portsc::POWER; 8],
            devices: std::vec![None; 8],
            iman: 0,
            imod: 0,
            erstsz: 0,
            erstba: 0,
            erdp: 0,
            er_base: 0,
            er_size: 0,
            er_enqueue: 0,
            er_cycle: true,
            dropped: 0,
            cmd_dequeue: 0,
            cmd_cycle: true,
            cmd_running: false,
            slots: HashMap::new(),
            resets: 0,
            commands: Vec::new(),
            requests: Vec::new(),
            port_writes: Vec::new(),
        }
    }

    fn running(&self) -> bool {
        self.usbsts & op::HALTED == 0
    }

    fn usb3(&self, port: u8) -> bool {
        self.protocols.iter().any(|p| p.major == 3 && (p.first..p.first + p.count).contains(&port))
    }

    /// Writes an event to the event ring, unless it is full.
    fn post(&mut self, parameter: u64, status: u32, control: u32) {
        if self.er_size == 0 || !self.running() {
            return;
        }
        let dequeue = (self.erdp & !0xf).wrapping_sub(self.er_base) / TRB_SIZE as u64;
        if (self.er_enqueue + 1) % self.er_size == dequeue {
            self.dropped += 1;
            return;
        }
        let at = self.er_base + self.er_enqueue * TRB_SIZE as u64;
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&parameter.to_le_bytes());
        bytes[8..12].copy_from_slice(&status.to_le_bytes());
        bytes[12..].copy_from_slice(&(control | u32::from(self.er_cycle)).to_le_bytes());
        fake_memory_write(at, &bytes);
        self.er_enqueue += 1;
        if self.er_enqueue == self.er_size {
            self.er_enqueue = 0;
            self.er_cycle = !self.er_cycle;
        }
        self.iman |= rt::IP;
    }

    fn port_event(&mut self, port: u8) {
        self.post(u64::from(port) << 24, u32::from(code::SUCCESS) << 24, kind::PORT_STATUS_CHANGE << 10);
    }

    /// What a port's register holds with `device` connected (or none)
    /// after a reset of the controller or a plug.
    fn plugged(&self, port: u8, device: Option<&SimDevice>) -> u32 {
        match device {
            Some(device) => {
                let enabled = if self.usb3(port) { portsc::ENABLED } else { 0 };
                portsc::POWER
                    | portsc::CONNECTED
                    | enabled
                    | u32::from(device.speed_id) << portsc::SPEED_SHIFT
                    | portsc::CONNECT_CHANGE
            }
            None => portsc::POWER,
        }
    }

    fn reset(&mut self) {
        self.resets += 1;
        self.usbcmd = 0;
        self.usbsts = op::HALTED;
        self.crcr_low = 0;
        self.crcr_high = 0;
        self.dcbaap = 0;
        self.config = 0;
        self.iman = 0;
        self.erstsz = 0;
        self.erstba = 0;
        self.erdp = 0;
        self.er_size = 0;
        self.slots.clear();
        self.cmd_running = false;
        for port in 1..=self.ports {
            let device = self.devices[usize::from(port - 1)].clone();
            // Ports powered by software come out of the reset unpowered.
            self.portsc[usize::from(port - 1)] =
                if self.port_power { 0 } else { self.plugged(port, device.as_ref()) };
        }
        if self.stuck_in_reset {
            self.usbsts |= op::NOT_READY;
        }
    }

    fn read_trb(at: u64) -> (u64, u32, u32) {
        let mut bytes = [0u8; 16];
        fake_memory(at, &mut bytes);
        (
            u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            u32::from_le_bytes(bytes[12..].try_into().unwrap()),
        )
    }

    fn read32(at: u64) -> u32 {
        let mut bytes = [0u8; 4];
        fake_memory(at, &mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn read64(at: u64) -> u64 {
        let mut bytes = [0u8; 8];
        fake_memory(at, &mut bytes);
        u64::from_le_bytes(bytes)
    }

    /// Runs the commands on the command ring.
    fn run_commands(&mut self) {
        if self.deaf {
            self.cmd_running = true;
            return;
        }
        loop {
            let at = self.cmd_dequeue;
            let (parameter, _status, control) = State::read_trb(at);
            if (control & 1 != 0) != self.cmd_cycle {
                break;
            }
            let kind = (control >> 10) & 0x3f;
            if kind == kind::LINK {
                self.cmd_dequeue = parameter;
                if control & control::TOGGLE_CYCLE != 0 {
                    self.cmd_cycle = !self.cmd_cycle;
                }
                continue;
            }
            let slot = (control >> 24) as u8;
            let endpoint = ((control >> 16) & 0x1f) as u8;
            self.commands.push((kind, slot));
            let (code, slot) = self.command(kind, slot, endpoint, parameter, control);
            self.cmd_dequeue += TRB_SIZE as u64;
            self.post(
                at,
                u32::from(code) << 24,
                kind::COMMAND_COMPLETION << 10 | u32::from(slot) << 24,
            );
        }
    }

    fn command(&mut self, kind: u32, slot: u8, endpoint: u8, parameter: u64, control: u32) -> (u8, u8) {
        let csz = self.context_size as u64;
        match kind {
            kind::NO_OP_COMMAND => (code::SUCCESS, 0),
            kind::ENABLE_SLOT => {
                let Some(free) = (1..=self.slots_max).find(|s| !self.slots.contains_key(s)) else {
                    return (code::NO_SLOTS, 0);
                };
                self.slots.insert(
                    free,
                    SimSlot {
                        port: 0,
                        addressed: false,
                        route: 0,
                        speed_id: 0,
                        max_packet: 0,
                        dequeue: 0,
                        cycle: true,
                        state: EpState::Stopped,
                        pending: None,
                    },
                );
                (code::SUCCESS, free)
            }
            kind::DISABLE_SLOT => match self.slots.remove(&slot) {
                Some(_) => (code::SUCCESS, slot),
                None => (code::SLOT_NOT_ENABLED, slot),
            },
            kind::ADDRESS_DEVICE => {
                let input = parameter;
                let add = State::read32(input + 4);
                let slot_ctx = State::read32(input + csz);
                let port = (State::read32(input + csz + 4) >> 16) as u8;
                let ep0 = State::read32(input + 2 * csz + 4);
                let dequeue = State::read64(input + 2 * csz + 8);
                let Some(entry) = self.slots.get_mut(&slot) else {
                    return (code::SLOT_NOT_ENABLED, slot);
                };
                if add != 3 || (ep0 >> 3) & 7 != 4 || dequeue & 1 == 0 {
                    return (code::PARAMETER, slot);
                }
                if self.devices.get(usize::from(port).wrapping_sub(1)).is_none_or(Option::is_none)
                {
                    return (code::TRANSACTION, slot);
                }
                entry.port = port;
                entry.route = slot_ctx & 0xf_ffff;
                entry.speed_id = ((slot_ctx >> 20) & 0xf) as u8;
                entry.max_packet = (ep0 >> 16) as u16;
                entry.dequeue = dequeue & !0xf;
                entry.cycle = true;
                entry.addressed = control & control::BSR == 0;
                entry.state = EpState::Running;
                // The output slot context: the address, and the addressed
                // state.
                let output = State::read64(self.dcbaap + 8 * u64::from(slot));
                fake_memory_write(output + 12, &(u32::from(slot) | 2 << 27).to_le_bytes());
                (code::SUCCESS, slot)
            }
            kind::EVALUATE_CONTEXT => {
                let add = State::read32(parameter + 4);
                let ep0 = State::read32(parameter + 2 * csz + 4);
                let Some(entry) = self.slots.get_mut(&slot) else {
                    return (code::SLOT_NOT_ENABLED, slot);
                };
                if add & 2 != 0 {
                    entry.max_packet = (ep0 >> 16) as u16;
                }
                (code::SUCCESS, slot)
            }
            kind::RESET_ENDPOINT | kind::STOP_ENDPOINT | kind::SET_TR_DEQUEUE => {
                let Some(entry) = self.slots.get_mut(&slot) else {
                    return (code::SLOT_NOT_ENABLED, slot);
                };
                if endpoint != 1 {
                    return (code::ENDPOINT_NOT_ENABLED, slot);
                }
                match (kind, entry.state) {
                    (kind::RESET_ENDPOINT, EpState::Halted) => entry.state = EpState::Stopped,
                    (kind::STOP_ENDPOINT, EpState::Running) => {
                        entry.state = EpState::Stopped;
                        if let Some(trb) = entry.pending.take() {
                            let stopped = trb;
                            self.post(
                                stopped,
                                u32::from(code::STOPPED) << 24,
                                kind::TRANSFER_EVENT << 10 | 1 << 16 | u32::from(slot) << 24,
                            );
                        }
                    }
                    (kind::SET_TR_DEQUEUE, EpState::Stopped | EpState::Halted) => {
                        entry.dequeue = parameter & !0xf;
                        entry.cycle = parameter & 1 != 0;
                    }
                    _ => return (code::CONTEXT_STATE, slot),
                }
                (code::SUCCESS, slot)
            }
            _ => (code::TRB, slot),
        }
    }

    /// Runs the transfers on slot `slot`'s endpoint 0.
    fn run_transfers(&mut self, slot: u8) {
        let Some(entry) = self.slots.get_mut(&slot) else { return };
        if entry.state == EpState::Halted || entry.pending.is_some() {
            return;
        }
        entry.state = EpState::Running;
        loop {
            let entry = self.slots.get_mut(&slot).unwrap();
            let (mut dequeue, mut cycle) = (entry.dequeue, entry.cycle);
            // A TD: TRBs up to and including one that interrupts.
            let mut trbs = Vec::new();
            loop {
                let (parameter, status, control) = State::read_trb(dequeue);
                if (control & 1 != 0) != cycle {
                    return;
                }
                if (control >> 10) & 0x3f == kind::LINK {
                    dequeue = parameter;
                    if control & control::TOGGLE_CYCLE != 0 {
                        cycle = !cycle;
                    }
                    continue;
                }
                trbs.push((dequeue, parameter, status, control));
                dequeue += TRB_SIZE as u64;
                if control & control::IOC != 0 {
                    break;
                }
            }
            let entry = self.slots.get_mut(&slot).unwrap();
            let port = entry.port;
            let first = trbs[0].0;
            let device = self.devices.get(usize::from(port).wrapping_sub(1)).cloned().flatten();
            let event = |code: u8, residual: u32| {
                (u32::from(code) << 24 | residual, kind::TRANSFER_EVENT << 10 | 1 << 16 | u32::from(slot) << 24)
            };
            let Some(mut device) = device else {
                // Pulled out: no answer, after the retries.
                let entry = self.slots.get_mut(&slot).unwrap();
                entry.state = EpState::Halted;
                let (status, control) = event(code::TRANSACTION, 0);
                self.post(first, status, control);
                return;
            };
            if device.silent {
                self.slots.get_mut(&slot).unwrap().pending = Some(first);
                return;
            }
            let (_, setup, _, _) = trbs[0];
            let setup = Setup {
                request_type: setup as u8,
                request: (setup >> 8) as u8,
                value: (setup >> 16) as u16,
                index: (setup >> 32) as u16,
                length: (setup >> 48) as u16,
            };
            self.requests.push((slot, setup));
            let data = trbs.iter().find(|t| (t.3 >> 10) & 0x3f == kind::DATA_STAGE).copied();
            let status_trb = *trbs.last().unwrap();
            let answer: Option<Vec<u8>> = if device.stalls.contains(&(setup.request, (setup.value >> 8) as u8)) {
                None
            } else {
                match (setup.request, (setup.value >> 8) as u8) {
                    (6, descriptor::DEVICE) => Some(device.device.clone()),
                    (6, descriptor::CONFIGURATION) => Some(device.config.clone()),
                    (6, descriptor::STRING) if setup.value & 0xff == 0 => Some(std::vec![4, 3, 0x09, 0x04]),
                    (6, descriptor::STRING) => device.strings.get(&(setup.value as u8)).map(|text| {
                        let units: Vec<u16> = text.encode_utf16().collect();
                        let mut bytes = std::vec![(2 + 2 * units.len()) as u8, 3];
                        for unit in units {
                            bytes.extend(unit.to_le_bytes());
                        }
                        bytes
                    }),
                    (9, _) => {
                        device.configuration = setup.value as u8;
                        Some(Vec::new())
                    }
                    _ => None,
                }
            };
            let entry = self.slots.get_mut(&slot).unwrap();
            let Some(answer) = answer else {
                entry.state = EpState::Halted;
                let at = data.map_or(status_trb.0, |d| d.0);
                let (status, control) = event(code::STALL, 0);
                self.post(at, status, control);
                return;
            };
            entry.dequeue = dequeue;
            entry.cycle = cycle;
            self.devices[usize::from(port - 1)] = Some(device);
            if let Some((at, buffer, length, control)) = data {
                let length = length & 0x1_ffff;
                if control & control::DIR_IN != 0 {
                    let moved = answer.len().min(length as usize);
                    fake_memory_write(buffer, &answer[..moved]);
                    let residual = length - moved as u32;
                    if residual > 0 && control & control::ISP != 0 {
                        let (status, control) = event(code::SHORT_PACKET, residual);
                        self.post(at, status, control);
                    }
                }
            }
            let (status, control) = event(code::SUCCESS, 0);
            self.post(status_trb.0, status, control);
        }
    }

    fn write_port(&mut self, port: u8, value: u32) {
        self.port_writes.push((port, value));
        let index = usize::from(port - 1);
        let mut current = self.portsc[index];
        if value & portsc::RESET != 0 || value & portsc::WARM_RESET != 0 {
            if current & portsc::CONNECTED != 0 {
                current |= portsc::ENABLED;
            }
            current |= if value & portsc::WARM_RESET != 0 {
                portsc::WARM_RESET_CHANGE | portsc::RESET_CHANGE
            } else {
                portsc::RESET_CHANGE
            };
            self.portsc[index] = current;
            self.port_event(port);
            return;
        }
        current &= !(value & portsc::CHANGES);
        if value & portsc::ENABLED != 0 {
            current &= !portsc::ENABLED;
        }
        current = (current & !portsc::POWER) | (value & portsc::POWER);
        self.portsc[index] = current;
    }

    /// Plugs `device` into `port`.
    fn plug(&mut self, port: u8, device: Option<SimDevice>) {
        let index = usize::from(port - 1);
        self.portsc[index] = self.plugged(port, device.as_ref());
        if device.is_none() {
            self.portsc[index] |= portsc::CONNECT_CHANGE;
        }
        self.devices[index] = device;
        self.port_event(port);
    }
}

/// The simulated controller.
struct Sim(Mutex<State>);

impl Sim {
    fn new(state: State) -> Arc<Sim> {
        Arc::new(Sim(Mutex::new(state)))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    fn mmio(self: &Arc<Self>) -> Mmio {
        Mmio::fake(self.clone(), REGS)
    }
}

impl Registers for Sim {
    fn read(&self, offset: usize, size: usize) -> u64 {
        assert_eq!(size, 4, "a register read of {size} bytes at {offset:#x}");
        let mut s = self.state();
        let protocols_size: usize =
            s.protocols.iter().map(|p| 16 + 4 * p.psi.len()).sum::<usize>();
        // The protocols follow the legacy capability, or come first.
        let protocols_at = if s.legacy.is_some() { XCAPS + 8 } else { XCAPS };
        let value = match offset {
            0x00 => (0x0100 << 16) | CAPLENGTH as u32,
            0x04 => u32::from(s.ports) << 24 | 1 << 8 | u32::from(s.slots_max),
            0x08 => {
                let sp = s.scratchpads;
                ((sp >> 5) & 0x1f) << 21 | (sp & 0x1f) << 27
            }
            0x10 => {
                let csz = if s.context_size == 64 { cap::CSZ } else { 0 };
                let ac64 = if s.ac64 { cap::AC64 } else { 0 };
                let ppc = if s.port_power { cap::PPC } else { 0 };
                ((XCAPS / 4) as u32) << 16 | csz | ac64 | ppc
            }
            0x14 => DOORBELLS as u32,
            0x18 => RUNTIME as u32,
            o if (CAPLENGTH..CAPLENGTH + 0x400).contains(&o) => match o - CAPLENGTH {
                op::USBCMD => s.usbcmd,
                op::USBSTS => s.usbsts,
                op::PAGESIZE => 1,
                op::CRCR => if s.cmd_running { op::CRCR_RUNNING } else { 0 },
                op::CONFIG => s.config,
                _ => 0,
            },
            o if (CAPLENGTH + op::PORTS..CAPLENGTH + op::PORTS + 0x10 * usize::from(s.ports))
                .contains(&o) =>
            {
                let port = (o - CAPLENGTH - op::PORTS) / 0x10;
                if (o - CAPLENGTH - op::PORTS) % 0x10 == 0 { s.portsc[port] } else { 0 }
            }
            o if (RUNTIME + 0x20..RUNTIME + 0x40).contains(&o) => match o - RUNTIME - 0x20 {
                rt::IMAN => s.iman,
                rt::IMOD => s.imod,
                rt::ERSTSZ => s.erstsz,
                rt::ERDP => s.erdp as u32,
                _ => 0,
            },
            o if (XCAPS..XCAPS + 8).contains(&o) && s.legacy.is_some() => {
                let now = s.now;
                let legacy = s.legacy.as_mut().unwrap();
                if let (Some(after), Some(asked)) = (legacy.lets_go_after, legacy.asked_at)
                    && now >= asked + after
                {
                    legacy.bios = false;
                }
                if o == XCAPS {
                    let next = 2u32 << 8;
                    xcap::LEGACY
                        | next
                        | if legacy.bios { xcap::BIOS_OWNED } else { 0 }
                        | if legacy.os { xcap::OS_OWNED } else { 0 }
                } else {
                    legacy.smi
                }
            }
            o if (protocols_at..protocols_at + protocols_size).contains(&o) => {
                let mut at = protocols_at;
                let mut value = 0;
                let count = s.protocols.len();
                for (i, p) in s.protocols.iter().enumerate() {
                    let size = 16 + 4 * p.psi.len();
                    if (at..at + size).contains(&o) {
                        let next = if i + 1 < count { (size / 4) as u32 } else { 0 };
                        value = match o - at {
                            0 => xcap::PROTOCOL | next << 8 | u32::from(p.major) << 24,
                            4 => u32::from_le_bytes(*b"USB "),
                            8 => {
                                u32::from(p.first)
                                    | u32::from(p.count) << 8
                                    | (p.psi.len() as u32) << 28
                            }
                            12 => u32::from(p.major),
                            k => p.psi[(k - 16) / 4],
                        };
                    }
                    at += size;
                }
                value
            }
            _ => 0,
        };
        u64::from(value)
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 4, "a register write of {size} bytes at {offset:#x}");
        let value = value as u32;
        let mut s = self.state();
        match offset {
            o if (CAPLENGTH..CAPLENGTH + 0x400).contains(&o) => match o - CAPLENGTH {
                op::USBCMD => {
                    if value & op::RESET != 0 {
                        assert!(s.usbsts & op::HALTED != 0, "a reset of a running controller");
                        s.reset();
                        return;
                    }
                    s.usbcmd = value;
                    if value & op::RUN != 0 {
                        assert!(s.usbsts & op::NOT_READY == 0);
                        s.usbsts &= !op::HALTED;
                    } else {
                        s.usbsts |= op::HALTED;
                    }
                }
                op::USBSTS => s.usbsts &= !(value & (op::HSE | op::EINT | op::PCD)),
                op::CRCR => s.crcr_low = value,
                0x1c => {
                    s.crcr_high = value;
                    if s.crcr_low & op::CRCR_ABORT != 0 && s.cmd_running {
                        s.cmd_running = false;
                        let at = s.cmd_dequeue;
                        s.post(at, u32::from(code::COMMAND_RING_STOPPED) << 24, kind::COMMAND_COMPLETION << 10);
                    } else if !s.cmd_running {
                        s.cmd_dequeue = u64::from(s.crcr_low & !0x3f) | u64::from(value) << 32;
                        s.cmd_cycle = s.crcr_low & 1 != 0;
                    }
                }
                op::DCBAAP => s.dcbaap = (s.dcbaap & !0xffff_ffff) | u64::from(value),
                0x34 => s.dcbaap = (s.dcbaap & 0xffff_ffff) | u64::from(value) << 32,
                op::CONFIG => s.config = value,
                _ => {}
            },
            o if (CAPLENGTH + op::PORTS..CAPLENGTH + op::PORTS + 0x10 * usize::from(s.ports))
                .contains(&o) =>
            {
                if (o - CAPLENGTH - op::PORTS) % 0x10 == 0 {
                    let port = ((o - CAPLENGTH - op::PORTS) / 0x10 + 1) as u8;
                    s.write_port(port, value);
                }
            }
            o if (RUNTIME + 0x20..RUNTIME + 0x40).contains(&o) => match o - RUNTIME - 0x20 {
                rt::IMAN => {
                    if value & rt::IP != 0 {
                        s.iman &= !rt::IP;
                    }
                    s.iman = (s.iman & !rt::IE) | (value & rt::IE);
                }
                rt::IMOD => s.imod = value,
                rt::ERSTSZ => s.erstsz = value,
                rt::ERSTBA => s.erstba = (s.erstba & !0xffff_ffff) | u64::from(value),
                0x14 => {
                    s.erstba = (s.erstba & 0xffff_ffff) | u64::from(value) << 32;
                    let table = s.erstba;
                    s.er_base = State::read64(table);
                    s.er_size = u64::from(State::read32(table + 8));
                    s.er_enqueue = 0;
                    s.er_cycle = true;
                }
                rt::ERDP => s.erdp = (s.erdp & !0xffff_ffff) | u64::from(value & !rt::EHB),
                0x1c => s.erdp = (s.erdp & 0xffff_ffff) | u64::from(value) << 32,
                _ => {}
            },
            o if o == XCAPS && s.legacy.is_some() => {
                let now = s.now;
                let legacy = s.legacy.as_mut().unwrap();
                legacy.os = value & xcap::OS_OWNED != 0;
                if legacy.os && legacy.asked_at.is_none() {
                    legacy.asked_at = Some(now);
                }
                if value & xcap::BIOS_OWNED == 0 && legacy.lets_go_after.is_none() {
                    // Overruled.
                    legacy.bios = false;
                }
            }
            o if o == XCAPS + 4 && s.legacy.is_some() => {
                let legacy = s.legacy.as_mut().unwrap();
                legacy.smi = (value & !xcap::SMI_EVENTS) & !(legacy.smi & value & xcap::SMI_EVENTS);
            }
            o if (DOORBELLS..DOORBELLS + 4 * 256).contains(&o) => {
                let index = ((o - DOORBELLS) / 4) as u8;
                if !s.running() {
                    return;
                }
                if index == 0 {
                    s.run_commands();
                } else if value == 1 {
                    s.run_transfers(index);
                }
            }
            _ => {}
        }
    }
}

/// The kernel's side, for the tests: a clock that moves only when the
/// driver waits, and the events handed on.
struct TestHost {
    sim: Arc<Sim>,
    now: u64,
    others: Vec<Event>,
}

impl Host for TestHost {
    fn wait(&mut self, us: u32) {
        self.now += u64::from(us);
        self.sim.state().now = self.now;
    }

    fn now_us(&mut self) -> u64 {
        self.now
    }

    fn other(&mut self, event: Event) {
        self.others.push(event);
    }
}

/// Memory for the driver, recording whether it was asked for below 4 GiB.
fn alloc(below: &mut Vec<bool>) -> impl FnMut(usize, bool) -> Option<Dma> + '_ {
    move |pages, below_4g| {
        below.push(below_4g);
        Some(Dma::fake(pages * 4096))
    }
}

fn host(sim: &Arc<Sim>) -> TestHost {
    TestHost { sim: sim.clone(), now: 0, others: Vec::new() }
}

/// Starts a driver on `sim`.
fn start(sim: &Arc<Sim>) -> (Xhci, TestHost) {
    let mut host = host(sim);
    let mut below = Vec::new();
    let xhci = Xhci::new(sim.mmio(), &mut host, &mut alloc(&mut below)).unwrap();
    (xhci, host)
}

/// What the kernel does for a device on root port `port`: resets the
/// port if it is USB 2, waits for it, gives the device a slot, reads the
/// first eight bytes of its descriptor and gives the controller its
/// packet size.
fn attach(xhci: &mut Xhci, host: &mut TestHost, port: u8) -> Result<Device, Error> {
    let status = xhci.port_status(port);
    xhci.acknowledge_port(port, status);
    if !status.enabled() {
        xhci.reset_port(port, false);
        let mut status = xhci.port_status(port);
        while !status.reset_changed() {
            host.wait(1000);
            status = xhci.port_status(port);
        }
        xhci.acknowledge_port(port, status);
    }
    let status = xhci.port_status(port);
    assert!(status.enabled());
    let speed = xhci.port_speed(port, status).unwrap();
    let place = Place { root_port: port, route: 0, speed, speed_id: status.speed_id() };
    let mut below = Vec::new();
    let mut device = Device::address(xhci, host, &mut alloc(&mut below), place)?;
    let mut first = [0u8; 8];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 8);
    device.control_in(xhci, host, setup, &mut first)?;
    let max = DeviceDescriptor::parse(&first).and_then(|d| d.max_packet0(speed)).unwrap();
    device.set_max_packet(xhci, host, max)?;
    Ok(device)
}

#[test]
fn the_controller_is_taken_reset_and_started() {
    let sim = Sim::new(State::new());
    let (xhci, host) = start(&sim);
    {
        let s = sim.state();
        assert_eq!(s.resets, 1);
        assert_eq!(s.usbcmd, op::RUN | op::INTE | op::HSEE);
        assert_eq!(s.config, 64);
        assert_ne!(s.dcbaap, 0);
        assert_eq!(s.erstsz, 1);
        assert_eq!(s.er_size, 256);
        assert_eq!(s.erdp & !0xf, s.er_base);
        assert_eq!(s.iman & rt::IE, rt::IE);
        assert_eq!(s.imod, rt::MODERATION);
        assert!(s.cmd_cycle);
    }
    let info = xhci.info();
    assert_eq!((info.version, info.ports, info.slots, info.context_size), (0x0100, 8, 64, 32));
    assert_eq!(info.handoff, Handoff::None);
    assert!(info.addresses_64 && !xhci.below_4g());
    let protocols = xhci.protocols();
    assert_eq!(protocols.len(), 2);
    assert_eq!((protocols[0].major, protocols[0].first_port, protocols[0].ports), (3, 1, 4));
    assert_eq!((protocols[1].major, protocols[1].first_port, protocols[1].ports), (2, 5, 4));
    assert_eq!(xhci.port_protocol(6).map(|p| p.major), Some(2));
    assert!(xhci.port_protocol(9).is_none());
    assert!(!xhci.failed());
    assert!(host.others.is_empty());
}

#[test]
fn the_firmware_lets_go_when_asked_and_its_smis_are_turned_off() {
    let mut state = State::new();
    state.legacy = Some(Legacy {
        bios: true,
        os: false,
        lets_go_after: Some(30_000),
        asked_at: None,
        smi: 0xe011_0011,
    });
    let sim = Sim::new(state);
    let (xhci, host) = start(&sim);
    assert_eq!(xhci.info().handoff, Handoff::Given);
    let legacy = sim.state().legacy.unwrap();
    assert!(legacy.os && !legacy.bios);
    // Every SMI enable off, the events cleared, the reserved bits kept.
    assert_eq!(legacy.smi & !xcap::SMI_KEEP, 0);
    assert_eq!(legacy.smi & xcap::SMI_KEEP, 0xe011_0011 & xcap::SMI_KEEP);
    assert!(host.now >= 30_000);
}

#[test]
fn a_firmware_that_will_not_let_go_is_overruled() {
    let mut state = State::new();
    state.legacy =
        Some(Legacy { bios: true, os: false, lets_go_after: None, asked_at: None, smi: 1 });
    let sim = Sim::new(state);
    let (xhci, host) = start(&sim);
    assert_eq!(xhci.info().handoff, Handoff::Taken);
    assert!(!sim.state().legacy.unwrap().bios);
    assert!(host.now >= HANDOFF_US);
    // And with nothing held, nothing to wait for.
    let mut state = State::new();
    state.legacy =
        Some(Legacy { bios: false, os: false, lets_go_after: None, asked_at: None, smi: 0 });
    let sim = Sim::new(state);
    let (xhci, host) = start(&sim);
    assert_eq!(xhci.info().handoff, Handoff::Free);
    assert!(sim.state().legacy.unwrap().os);
    assert!(host.now < HANDOFF_US);
}

#[test]
fn a_controller_that_stays_in_reset_is_refused() {
    let mut state = State::new();
    state.stuck_in_reset = true;
    let sim = Sim::new(state);
    let mut host = host(&sim);
    let mut below = Vec::new();
    let err = Xhci::new(sim.mmio(), &mut host, &mut alloc(&mut below)).err();
    assert_eq!(err, Some(Error::Unresponsive("it did not come out of its reset")));
}

#[test]
fn scratchpads_and_32_bit_memory_are_given_as_asked() {
    let mut state = State::new();
    state.scratchpads = 40;
    state.ac64 = false;
    let sim = Sim::new(state);
    let mut host = host(&sim);
    let mut below = Vec::new();
    let xhci = Xhci::new(sim.mmio(), &mut host, &mut alloc(&mut below)).unwrap();
    assert_eq!(xhci.info().scratchpads, 40);
    assert!(xhci.below_4g());
    assert!(below.iter().all(|&b| b));
    let dcbaa = sim.state().dcbaap;
    let array = State::read64(dcbaa);
    assert_ne!(array, 0);
    let pages: Vec<u64> = (0..40).map(|i| State::read64(array + 8 * i)).collect();
    assert!(pages.windows(2).all(|w| w[1] == w[0] + 4096));
}

#[test]
fn ports_powered_by_software_are_powered() {
    let mut state = State::new();
    state.port_power = true;
    state.portsc = std::vec![0; 8];
    let sim = Sim::new(state);
    let (xhci, host) = start(&sim);
    assert!((1..=8).all(|port| xhci.port_status(port).powered()));
    assert!(host.now >= POWER_US);
}

#[test]
fn speeds_come_from_the_protocols() {
    let mut state = State::new();
    state.protocols = std::vec![
        SimProtocol { major: 2, first: 1, count: 2, psi: Vec::new() },
        // A port with its own speed IDs: 5 Gb/s as ID 1, 10 Gb/s as ID 2.
        SimProtocol { major: 3, first: 3, count: 1, psi: std::vec![5 << 16 | 3 << 4 | 1, 10 << 16 | 3 << 4 | 2] },
    ];
    let sim = Sim::new(state);
    let (xhci, _) = start(&sim);
    let usb2 = xhci.port_protocol(1).unwrap();
    assert_eq!(usb2.speed(1), Some(Speed::Full));
    assert_eq!(usb2.speed(2), Some(Speed::Low));
    assert_eq!(usb2.speed(3), Some(Speed::High));
    assert_eq!(usb2.speed(4), None);
    assert_eq!(usb2.speed_id(Speed::Low), 2);
    let usb3 = xhci.port_protocol(3).unwrap();
    assert_eq!(usb3.speed(1), Some(Speed::Super));
    assert_eq!(usb3.speed(2), Some(Speed::SuperPlus));
    assert_eq!(usb3.speed(4), None);
    assert_eq!(usb3.speed_id(Speed::SuperPlus), 2);
}

#[test]
fn a_device_on_a_usb2_port_is_reset_addressed_and_read() {
    let mut state = State::new();
    state.devices[5] = Some(keyboard(3, 64));
    let sim = Sim::new(state);
    let (mut xhci, mut host) = start(&sim);
    let status = xhci.port_status(6);
    assert!(status.connected() && status.connect_changed() && !status.enabled());
    let mut device = attach(&mut xhci, &mut host, 6).unwrap();
    // The port was reset with its power kept, and its changes acknowledged.
    let writes = sim.state().port_writes.clone();
    assert!(writes.iter().any(|&(p, v)| p == 6 && v & portsc::RESET != 0 && v & portsc::POWER != 0));
    assert!(writes.iter().all(|&(_, v)| v & portsc::ENABLED == 0));
    assert_eq!(xhci.port_status(6).changes(), 0);
    let slot = device.slot();
    assert_eq!(slot, 1);
    assert_eq!(device.usb_address(), 1);
    {
        let s = sim.state();
        let sim_slot = &s.slots[&slot];
        assert_eq!((sim_slot.port, sim_slot.route, sim_slot.speed_id), (6, 0, 3));
        assert!(sim_slot.addressed);
        // High speed: 64 from the start, and no Evaluate Context.
        assert_eq!(sim_slot.max_packet, 64);
        assert!(!s.commands.iter().any(|&(k, _)| k == kind::EVALUATE_CONTEXT));
    }
    let mut bytes = [0u8; 18];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(device.control_in(&mut xhci, &mut host, setup, &mut bytes), Ok(18));
    let descriptor = DeviceDescriptor::parse(&bytes).unwrap();
    assert_eq!((descriptor.vendor, descriptor.product), (0x0627, 1));
    // A string, longer than asked for is cut; shorter is said.
    let mut text = [0u8; 255];
    let setup = Setup::get_descriptor(descriptor::STRING, 4, 0x0409, 255);
    let came = device.control_in(&mut xhci, &mut host, setup, &mut text).unwrap();
    assert_eq!(came, 2 + 2 * "QEMU USB Keyboard".len());
    let mut name = String::new();
    radix_usb::string(&text[..came], &mut name).unwrap();
    assert_eq!(name, "QEMU USB Keyboard");
    let mut config = [0u8; 9];
    let setup = Setup::get_descriptor(descriptor::CONFIGURATION, 0, 0, 9);
    assert_eq!(device.control_in(&mut xhci, &mut host, setup, &mut config), Ok(9));
    device.control_out(&mut xhci, &mut host, Setup::set_configuration(1), &[]).unwrap();
    assert_eq!(sim.state().devices[5].as_ref().unwrap().configuration, 1);
    device.disable(&mut xhci, &mut host).unwrap();
    assert!(sim.state().slots.is_empty());
    // The port's reset raised an event, which came while the driver
    // waited for its first command.
    assert_eq!(host.others, [Event::Port { port: 6 }]);
}

#[test]
fn a_usb3_device_needs_no_reset() {
    let mut state = State::new();
    let mut disk = keyboard(4, 9);
    disk.device[2..4].copy_from_slice(&0x0320u16.to_le_bytes());
    state.devices[1] = Some(disk);
    let sim = Sim::new(state);
    let (mut xhci, mut host) = start(&sim);
    let status = xhci.port_status(2);
    assert!(status.enabled());
    assert_eq!(xhci.port_speed(2, status), Some(Speed::Super));
    let device = attach(&mut xhci, &mut host, 2).unwrap();
    assert_eq!(device.max_packet(), 512);
    assert!(sim.state().port_writes.iter().all(|&(_, v)| v & portsc::RESET == 0));
}

#[test]
fn a_full_speed_device_gets_its_packet_size_evaluated() {
    let mut state = State::new();
    state.devices[4] = Some(keyboard(1, 64));
    let sim = Sim::new(state);
    let (mut xhci, mut host) = start(&sim);
    let device = attach(&mut xhci, &mut host, 5).unwrap();
    assert_eq!(device.max_packet(), 64);
    let s = sim.state();
    assert_eq!(s.slots[&device.slot()].max_packet, 64);
    assert_eq!(s.commands.iter().filter(|&&(k, _)| k == kind::EVALUATE_CONTEXT).count(), 1);
}

#[test]
fn a_stalled_request_fails_and_the_pipe_works_after() {
    let mut state = State::new();
    let mut device = keyboard(3, 64);
    device.stalls.push((6, descriptor::BOS));
    state.devices[4] = Some(device);
    let sim = Sim::new(state);
    let (mut xhci, mut host) = start(&sim);
    let mut device = attach(&mut xhci, &mut host, 5).unwrap();
    let mut bytes = [0u8; 5];
    let setup = Setup::get_descriptor(descriptor::BOS, 0, 0, 5);
    assert_eq!(
        device.control_in(&mut xhci, &mut host, setup, &mut bytes),
        Err(Error::Completion(code::STALL))
    );
    {
        let s = sim.state();
        let kinds: Vec<u32> = s.commands.iter().map(|&(k, _)| k).rev().take(2).collect();
        assert_eq!(kinds, [kind::SET_TR_DEQUEUE, kind::RESET_ENDPOINT]);
    }
    // A request with no data stage stalls on its status stage.
    let mut nothing = [0u8; 0];
    let setup = Setup { length: 0, ..Setup::get_descriptor(descriptor::BOS, 0, 0, 0) };
    assert_eq!(
        device.control_in(&mut xhci, &mut host, setup, &mut nothing),
        Err(Error::Completion(code::STALL))
    );
    let mut bytes = [0u8; 18];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(device.control_in(&mut xhci, &mut host, setup, &mut bytes), Ok(18));
}

#[test]
fn a_device_that_never_answers_is_stopped_and_skipped() {
    let mut state = State::new();
    state.devices[4] = Some(keyboard(3, 64));
    let sim = Sim::new(state);
    let (mut xhci, mut host) = start(&sim);
    let mut device = attach(&mut xhci, &mut host, 5).unwrap();
    sim.state().devices[4].as_mut().unwrap().silent = true;
    let mut bytes = [0u8; 18];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    let before = host.now;
    assert_eq!(device.control_in(&mut xhci, &mut host, setup, &mut bytes), Err(Error::TimedOut));
    assert!(host.now - before >= 5_000_000);
    {
        let s = sim.state();
        let kinds: Vec<u32> = s.commands.iter().map(|&(k, _)| k).rev().take(2).collect();
        assert_eq!(kinds, [kind::SET_TR_DEQUEUE, kind::STOP_ENDPOINT]);
    }
    sim.state().devices[4].as_mut().unwrap().silent = false;
    assert_eq!(device.control_in(&mut xhci, &mut host, setup, &mut bytes), Ok(18));
}

#[test]
fn a_device_pulled_out_fails_its_transfers_and_its_port_says_so() {
    let mut state = State::new();
    state.devices[4] = Some(keyboard(3, 64));
    let sim = Sim::new(state);
    let (mut xhci, mut host) = start(&sim);
    let mut device = attach(&mut xhci, &mut host, 5).unwrap();
    sim.state().plug(5, None);
    let mut bytes = [0u8; 18];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(
        device.control_in(&mut xhci, &mut host, setup, &mut bytes),
        Err(Error::Completion(code::TRANSACTION))
    );
    // The port's change came while the driver waited (after its reset's).
    assert_eq!(host.others, [Event::Port { port: 5 }, Event::Port { port: 5 }]);
    let status = xhci.port_status(5);
    assert!(!status.connected() && status.connect_changed());
    device.disable(&mut xhci, &mut host).unwrap();
    // A slot for a port with nothing on it fails, and is given back.
    assert_eq!(attach_error(&mut xhci, &mut host, 5), Some(Error::Completion(code::TRANSACTION)));
    assert!(sim.state().slots.is_empty());
}

/// Addresses a device on `port` whatever its status says, for a port with
/// nothing on it.
fn attach_error(xhci: &mut Xhci, host: &mut TestHost, port: u8) -> Option<Error> {
    let place = Place { root_port: port, route: 0, speed: Speed::High, speed_id: 3 };
    let mut below = Vec::new();
    Device::address(xhci, host, &mut alloc(&mut below), place).err()
}

#[test]
fn rings_wrap_with_their_cycle_state() {
    let mut state = State::new();
    state.devices[4] = Some(keyboard(3, 64));
    let sim = Sim::new(state);
    let (mut xhci, mut host) = start(&sim);
    let mut device = attach(&mut xhci, &mut host, 5).unwrap();
    // Three TRBs a transfer, an event each (and a short packet): around
    // endpoint 0's ring and the event ring several times.
    for i in 0..(3 * RING_TRBS) {
        let mut bytes = [0u8; 64];
        let length = if i % 2 == 0 { 18 } else { 64 };
        let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, length);
        assert_eq!(device.control_in(&mut xhci, &mut host, setup, &mut bytes), Ok(18), "transfer {i}");
    }
    // And the command ring.
    for i in 0..(2 * RING_TRBS) {
        assert!(xhci.command(Command::Noop, &mut host).is_ok(), "command {i}");
    }
    let s = sim.state();
    assert_eq!(s.dropped, 0);
    assert!(s.er_enqueue < 256);
}

#[test]
fn events_for_others_are_handed_on_while_waiting() {
    let mut state = State::new();
    state.devices[4] = Some(keyboard(3, 64));
    let sim = Sim::new(state);
    let (mut xhci, mut host) = start(&sim);
    sim.state().plug(6, Some(keyboard(3, 64)));
    xhci.command(Command::Noop, &mut host).unwrap();
    assert_eq!(host.others, [Event::Port { port: 6 }]);
    // With nothing waited for, they are read as they come.
    sim.state().plug(7, Some(keyboard(3, 64)));
    assert_eq!(xhci.next_event(), Some(Event::Port { port: 7 }));
    assert_eq!(xhci.next_event(), None);
    xhci.events_read();
}

#[test]
fn a_command_that_never_finishes_is_aborted() {
    let sim = Sim::new(State::new());
    let (mut xhci, mut host) = start(&sim);
    sim.state().deaf = true;
    assert_eq!(xhci.command(Command::Noop, &mut host), Err(Error::TimedOut));
    assert!(!sim.state().cmd_running);
    assert!(!xhci.failed());
    // The ring goes on where the driver left it.
    sim.state().deaf = false;
    assert!(xhci.command(Command::Noop, &mut host).is_ok());
}

#[test]
fn slots_run_out() {
    let mut state = State::new();
    state.slots_max = 1;
    state.devices[4] = Some(keyboard(3, 64));
    state.devices[5] = Some(keyboard(3, 64));
    let sim = Sim::new(state);
    let (mut xhci, mut host) = start(&sim);
    let _first = attach(&mut xhci, &mut host, 5).unwrap();
    assert_eq!(attach(&mut xhci, &mut host, 6).err(), Some(Error::Completion(code::NO_SLOTS)));
}

#[test]
fn big_contexts_are_laid_out_at_64_bytes() {
    let mut state = State::new();
    state.context_size = 64;
    state.devices[4] = Some(keyboard(1, 64));
    let sim = Sim::new(state);
    let (mut xhci, mut host) = start(&sim);
    let device = attach(&mut xhci, &mut host, 5).unwrap();
    let s = sim.state();
    let slot = &s.slots[&device.slot()];
    assert_eq!((slot.port, slot.speed_id, slot.max_packet), (5, 1, 64));
}

#[test]
fn the_controller_halts_for_the_power_off() {
    let sim = Sim::new(State::new());
    let (mut xhci, mut host) = start(&sim);
    xhci.halt(&mut host).unwrap();
    assert!(sim.state().usbsts & op::HALTED != 0);
    assert!(xhci.failed());
}

#[test]
fn the_controller_is_stopped_from_outside() {
    let sim = Sim::new(State::new());
    let (_xhci, _host) = start(&sim);
    let mut waited = 0;
    assert!(stop(&sim.mmio(), &mut |us| waited += us));
    assert!(sim.state().usbsts & op::HALTED != 0);
    assert_eq!(sim.state().usbcmd & (op::RUN | op::INTE), 0);
    assert_eq!(waited, 0);
}

#[test]
fn requests_too_long_or_the_wrong_way_are_refused() {
    let mut state = State::new();
    state.devices[4] = Some(keyboard(3, 64));
    let sim = Sim::new(state);
    let (mut xhci, mut host) = start(&sim);
    let mut device = attach(&mut xhci, &mut host, 5).unwrap();
    let mut small = [0u8; 4];
    let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(device.control_in(&mut xhci, &mut host, setup, &mut small), Err(Error::BadRequest));
    assert_eq!(device.control_out(&mut xhci, &mut host, setup, &[]), Err(Error::BadRequest));
    let mut big = std::vec![0u8; 8192];
    let setup = Setup::get_descriptor(descriptor::CONFIGURATION, 0, 0, 5000);
    assert_eq!(device.control_in(&mut xhci, &mut host, setup, &mut big), Err(Error::BadRequest));
}

