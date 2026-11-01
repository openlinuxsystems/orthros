// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated PPM: a reset that takes a while,
//! commands and their acknowledgements, a laptop's two ports with a
//! charger on one, a change while a command runs, and failures.

use std::vec::Vec;

use crate::*;

/// A connector of the simulated PPM.
#[derive(Clone, Copy, Default)]
struct Port {
    capability: [u8; 4],
    status: [u8; 9],
    /// The partner's source objects.
    partner_pdos: [u32; 4],
    pdo_count: usize,
}

/// A simulated PPM behind its mailbox.
struct Sim {
    memory: [u8; 64],
    ports: Vec<Port>,
    /// Reads until a reset completes.
    resetting: u32,
    /// Reads the next command stays busy for.
    busy: u32,
    /// The answer the PPM gives at its next read, held while busy.
    held: Option<(u32, Vec<u8>)>,
    /// A change on this connector shows in the next CCIs until it is
    /// acknowledged.
    change: u8,
    notifications: u16,
    /// Commands it does not do, and one that fails.
    unsupported: u8,
    failing: u8,
    commands: Vec<u8>,
    /// A command was completed and not acknowledged yet.
    owed: bool,
    /// The OPM read a CCI without asking the PPM first.
    stale_reads: u32,
    synced: bool,
    waited_ms: u32,
}

impl Sim {
    fn new(ports: Vec<Port>) -> Sim {
        let mut memory = [0; 64];
        memory[..2].copy_from_slice(&0x0120u16.to_le_bytes());
        Sim {
            memory,
            ports,
            resetting: 0,
            busy: 0,
            held: None,
            change: 0,
            notifications: 0,
            unsupported: 0,
            failing: 0,
            commands: Vec::new(),
            owed: false,
            stale_reads: 0,
            synced: false,
            waited_ms: 0,
        }
    }

    fn set_cci(&mut self, cci: u32) {
        let cci = cci | u32::from(self.change) << cci::CONNECTOR_SHIFT;
        self.memory[4..8].copy_from_slice(&cci.to_le_bytes());
    }

    fn answer(&mut self, flags: u32, data: &[u8]) {
        let cci = flags | (data.len() as u32) << cci::LENGTH_SHIFT;
        if self.busy > 0 {
            self.held = Some((cci, data.to_vec()));
            self.set_cci(cci::BUSY);
            return;
        }
        self.memory[16..16 + data.len()].copy_from_slice(data);
        self.set_cci(cci);
    }

    fn run(&mut self) {
        let control = u64::from_le_bytes(self.memory[8..16].try_into().unwrap());
        let code = control as u8;
        self.commands.push(code);
        let port = (control >> 16 & 0x7f) as usize;
        if code != command::ACK_CC_CI && code != command::PPM_RESET {
            assert!(!self.owed, "a command was sent before the last was acknowledged");
        }
        if code == self.unsupported {
            self.owed = true;
            return self.answer(cci::COMMAND_COMPLETED | cci::NOT_SUPPORTED, &[]);
        }
        if code == self.failing {
            self.owed = true;
            return self.answer(cci::COMMAND_COMPLETED | cci::ERROR, &[]);
        }
        match code {
            command::PPM_RESET => {
                self.notifications = 0;
                self.change = 0;
                self.owed = false;
                self.set_cci(0);
                self.resetting = 3;
            }
            command::ACK_CC_CI => {
                if control & 1 << 16 != 0 {
                    self.change = 0;
                }
                if control & 1 << 17 != 0 {
                    self.owed = false;
                }
                self.set_cci(cci::ACK_COMMAND);
            }
            _ => {
                self.owed = true;
                let data: Vec<u8> = match code {
                    command::SET_NOTIFICATION_ENABLE => {
                        self.notifications = (control >> 16) as u16;
                        Vec::new()
                    }
                    command::GET_CAPABILITY => {
                        let mut data = [0u8; 16];
                        data[0] = 0b110; // battery charging, Power Delivery
                        data[4] = self.ports.len() as u8;
                        data[8] = 1;
                        data[12..14].copy_from_slice(&0x0300u16.to_le_bytes());
                        data[14..16].copy_from_slice(&0x0200u16.to_le_bytes());
                        data.to_vec()
                    }
                    command::GET_CONNECTOR_CAPABILITY => self.ports[port - 1].capability.to_vec(),
                    command::GET_CONNECTOR_STATUS => self.ports[port - 1].status.to_vec(),
                    command::GET_PDOS => {
                        assert_eq!(control >> 23 & 1, 1, "the partner's objects");
                        assert_eq!(control >> 29 & 1, 1, "as a source");
                        let p = self.ports[port - 1];
                        p.partner_pdos[..p.pdo_count]
                            .iter()
                            .flat_map(|pdo| pdo.to_le_bytes())
                            .collect()
                    }
                    command::GET_ERROR_STATUS => {
                        // An invalid connector number.
                        let mut data = [0u8; 16];
                        data[0] = 0x02;
                        data.to_vec()
                    }
                    _ => {
                        return self.answer(cci::COMMAND_COMPLETED | cci::NOT_SUPPORTED, &[]);
                    }
                };
                self.answer(cci::COMMAND_COMPLETED, &data);
            }
        }
    }
}

impl Mailbox for Sim {
    fn read(&mut self, offset: usize, out: &mut [u8]) {
        if offset == offset::CCI && !self.synced {
            self.stale_reads += 1;
        }
        self.synced = false;
        out.copy_from_slice(&self.memory[offset..offset + out.len()]);
    }

    fn write(&mut self, offset: usize, data: &[u8]) {
        self.memory[offset..offset + data.len()].copy_from_slice(data);
    }

    fn after_write(&mut self) -> Result<(), Error> {
        self.run();
        Ok(())
    }

    fn before_read(&mut self) -> Result<(), Error> {
        self.synced = true;
        if self.resetting > 0 {
            self.resetting -= 1;
            if self.resetting == 0 {
                self.set_cci(cci::RESET_COMPLETED);
            }
        }
        if self.busy > 0 {
            self.busy -= 1;
            if self.busy == 0
                && let Some((cci, data)) = self.held.take()
            {
                self.memory[16..16 + data.len()].copy_from_slice(&data);
                self.set_cci(cci);
            }
        }
        Ok(())
    }

    fn delay_ms(&mut self, ms: u32) {
        self.waited_ms += ms;
    }
}

/// A word of `fields`, each a value and the bit it starts at.
const fn pack(fields: &[(u32, u32)]) -> u32 {
    let mut word = 0;
    let mut i = 0;
    while i < fields.len() {
        word |= fields[i].0 << fields[i].1;
        i += 1;
    }
    word
}

/// A dual-role port of USB 2 and 3 and alternate modes, that gives and
/// takes power.
const DUAL_ROLE: [u8; 4] = [0b1110_0100, 0b11, 0, 0];

/// Connector status bits: a charger by Power Delivery, the machine a
/// sink, charging, its RDO for object 2 at 3 A.
fn charger_status() -> [u8; 9] {
    let flags: u32 = 1 << 14 | 3 << 16 | 1 << 19 | 1 << 29;
    let rdo = pack(&[(2, 28), (300, 10), (300, 0)]);
    let mut status = [0; 9];
    status[..4].copy_from_slice(&flags.to_le_bytes());
    status[4..8].copy_from_slice(&rdo.to_le_bytes());
    status[8] = 1;
    status
}

/// A 65 W charger's source objects: 5 V 3 A, 20 V 3.25 A, 9 V 3 A, and
/// a programmable 3.3 to 21 V at 3 A.
const CHARGER_PDOS: [u32; 4] = [
    pack(&[(100, 10), (300, 0)]),
    pack(&[(400, 10), (325, 0)]),
    pack(&[(180, 10), (300, 0)]),
    pack(&[(0b11, 30), (210, 17), (33, 8), (60, 0)]),
];

fn laptop() -> Sim {
    let charger = Port {
        capability: DUAL_ROLE,
        status: charger_status(),
        partner_pdos: CHARGER_PDOS,
        pdo_count: 4,
    };
    Sim::new(std::vec![charger, Port { capability: DUAL_ROLE, ..Port::default() }])
}

#[test]
fn a_laptop_s_ports_are_read() {
    let mut ppm = Ppm::reset(laptop()).unwrap();
    assert_eq!(ppm.version(), 0x0120);
    // The reset was waited for, a millisecond at a time.
    assert_eq!(ppm.mailbox().waited_ms, 2);
    ppm.enable_notifications(NOTIFICATIONS).unwrap();
    assert_eq!(ppm.mailbox().notifications, 0xdbe7);
    let capability = ppm.capability().unwrap();
    assert_eq!(capability.connectors, 2);
    assert!(capability.power_delivery());
    assert_eq!((capability.pd_version, capability.typec_version), (0x0300, 0x0200));
    let port = ppm.connector_capability(1).unwrap();
    assert_eq!((port.port_type(), port.provider, port.consumer), ("dual", true, true));
    let status = ppm.connector_status(1).unwrap();
    assert!(status.connected && !status.provider);
    assert_eq!(status.power, PowerMode::PowerDelivery);
    assert_eq!(status.power.as_str(), "usb_power_delivery");
    assert_eq!((status.partner, status.charging), (Partner::Host, Charging::Nominal));
    assert_eq!((status.data_role(), status.power_role()), ("device", "sink"));
    assert_eq!(status.object_position(), 2);
    let contract = ppm.contract(1, &status).unwrap();
    assert_eq!(contract, Contract { mv: 20_000, ma: 3000 });
    let text = std::format!("{}", Describe { status: &status, contract: Some(contract) });
    assert_eq!(
        text,
        "a charger; the machine takes 20 V at 3 A (60 W) by USB Power Delivery; charging"
    );
    let empty = ppm.connector_status(2).unwrap();
    assert!(!empty.connected);
    assert_eq!(empty.data_role(), "host");
    assert_eq!(
        std::format!("{}", Describe { status: &empty, contract: None }),
        "nothing is plugged in"
    );
    // Every command was acknowledged; every CCI was asked for first.
    let sim = ppm.mailbox();
    assert!(!sim.owed);
    assert_eq!(sim.stale_reads, 0);
    let acks = sim.commands.iter().filter(|&&c| c == command::ACK_CC_CI).count();
    assert_eq!(acks, sim.commands.len() - 1 - acks);
}

#[test]
fn changes_are_found_and_acknowledged() {
    let mut ppm = Ppm::reset(laptop()).unwrap();
    assert_eq!(ppm.changed(), Ok(None));
    // A device is plugged into port 2 while a command runs.
    {
        let sim = ppm.mailbox();
        let flags: u32 = 1 << 14 | 5 << 16 | 1 << 19 | 1 << 20 | 1 << 21 | 2 << 29;
        sim.ports[1].status[..4].copy_from_slice(&flags.to_le_bytes());
        sim.change = 2;
    }
    ppm.capability().unwrap();
    assert_eq!(ppm.changed(), Ok(Some(2)));
    let status = ppm.connector_status(2).unwrap();
    assert_eq!(status.change & 1 << 14, 1 << 14);
    assert_eq!((status.partner, status.provider), (Partner::Device, true));
    assert_eq!(
        std::format!("{}", Describe { status: &status, contract: ppm.contract(2, &status) }),
        "a device; the machine gives 5 V at 3 A"
    );
    ppm.ack_change(2).unwrap();
    assert_eq!(ppm.mailbox().change, 0);
    assert_eq!(ppm.changed(), Ok(None));
    // One the CCI shows by itself, as after a notify.
    ppm.mailbox().change = 1;
    ppm.mailbox().set_cci(0);
    assert_eq!(ppm.changed(), Ok(Some(1)));
}

#[test]
fn busy_unsupported_and_failed_commands() {
    let mut ppm = Ppm::reset(laptop()).unwrap();
    ppm.mailbox().busy = 3;
    assert_eq!(ppm.capability().unwrap().connectors, 2);
    ppm.mailbox().unsupported = command::GET_PDOS;
    let status = ppm.connector_status(1).unwrap();
    assert_eq!(ppm.contract(1, &status), None);
    assert!(!ppm.mailbox().owed);
    ppm.mailbox().failing = command::GET_CONNECTOR_STATUS;
    assert_eq!(ppm.connector_status(7), Err(Error::Failed(2)));
    assert_eq!(std::format!("{}", Error::Failed(2)), "the firmware said it failed (error 0x0002)");
    // A PPM that never answers.
    let mut stuck = Sim::new(Vec::new());
    stuck.unsupported = command::PPM_RESET;
    assert!(matches!(Ppm::reset(stuck), Err(Error::Timeout)));
}

#[test]
fn power_objects_are_read() {
    assert_eq!(Pdo::parse(CHARGER_PDOS[1]), Pdo::Fixed { mv: 20_000, ma: 3250 });
    assert_eq!(
        Pdo::parse(CHARGER_PDOS[3]),
        Pdo::Programmable { min_mv: 3300, max_mv: 21_000, ma: 3000 }
    );
    let battery = pack(&[(0b01, 30), (400, 20), (100, 10), (240, 0)]);
    assert_eq!(Pdo::parse(battery), Pdo::Battery { min_mv: 5000, max_mv: 20_000, mw: 60_000 });
    let variable = pack(&[(0b10, 30), (300, 20), (180, 10), (200, 0)]);
    assert_eq!(Pdo::parse(variable), Pdo::Variable { min_mv: 9000, max_mv: 15_000, ma: 2000 });
    // EPR's adjustable voltage supply.
    let avs = 0b11 << 30 | 0b01 << 28 | 1;
    assert_eq!(Pdo::parse(avs), Pdo::Other(avs));
    // A PPS request for 9.2 V at 2.5 A.
    let rdo = pack(&[(4, 28), (460, 9), (50, 0)]);
    let pps = Pdo::parse(CHARGER_PDOS[3]);
    assert_eq!(Contract::of(pps, rdo), Some(Contract { mv: 9200, ma: 2500 }));
    assert_eq!(std::format!("{}", Volts(9200)), "9.2 V");
    assert_eq!(std::format!("{} {}", Amperes(1500), Amperes(3250)), "1.5 A 3.25 A");
}

#[test]
fn answers_that_are_too_short_are_refused() {
    assert_eq!(Capability::parse(&[0; 15]), Err(Error::Malformed));
    assert_eq!(ConnectorStatus::parse(&[0; 8]), Err(Error::Malformed));
    assert_eq!(ConnectorCapability::parse(&[0]), Err(Error::Malformed));
    let only_host = ConnectorCapability { modes: 0b0010_0001, provider: true, consumer: false };
    assert_eq!(only_host.port_type(), "source");
}
