// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! USB Type-C ports for the Radix kernel (Phase 11g, ADR 0248), through
//! the firmware's *USB Type-C Connector System Software Interface* (UCSI,
//! Intel's specification, revisions 1.2 to 3.0).
//!
//! On a laptop with USB-C ports, a Platform Policy Manager (PPM; the
//! embedded controller's or a PD controller's firmware) runs the ports:
//! it negotiates power with a charger or a device by USB Power Delivery,
//! picks the data role and switches alternate modes. The operating system
//! (the OPM) asks it what is plugged in and is told when that changes,
//! through a mailbox in memory: the version (offset 0), the Command Status
//! and Connector Change Indication (CCI, offset 4), the CONTROL word that
//! carries a command (offset 8) and MESSAGE IN, its answer (offset 16).
//! On ACPI machines the mailbox is the memory a `USBC000` device's `_CRS`
//! names, and its `_DSM` (function 1 after the OPM writes, 2 before it
//! reads) passes it to and from the PPM ([`Mailbox`]).
//!
//! [`Ppm`] sends one command at a time and waits for the CCI to say it
//! is done, then acknowledges it (ACK_CC_CI), as section 4 of the
//! specification asks: PPM_RESET, SET_NOTIFICATION_ENABLE, GET_CAPABILITY,
//! GET_CONNECTOR_CAPABILITY, GET_CONNECTOR_STATUS and GET_PDOS. What they
//! answer is read into [`Capability`], [`ConnectorCapability`],
//! [`ConnectorStatus`] and [`Pdo`], whose bits are those of the
//! specification's tables and of the *USB Power Delivery Specification*
//! (revision 3.1) for power data objects and request data objects. A
//! change on a connector shows in the CCI's Connector Change Indicator,
//! which [`Ppm::changed`] reads and [`Ppm::ack_change`] acknowledges.
//!
//! Nothing is set: roles are not swapped and alternate modes are left to
//! the PPM, which picks them by itself. The crate is a compartment with no
//! `unsafe` code; its tests run it against a simulated PPM.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use core::fmt;

/// The `_DSM` interface of a UCSI device, 6F8398C2-7CA4-11E4-AD36-
/// 631042B5008F, as the 16 bytes of its buffer (its first three fields
/// little-endian).
pub const DSM_UUID: [u8; 16] = [
    0xc2, 0x98, 0x83, 0x6f, 0xa4, 0x7c, 0xe4, 0x11, 0xad, 0x36, 0x63, 0x10, 0x42, 0xb5, 0x00, 0x8f,
];
/// The `_DSM`'s revision.
pub const DSM_REVISION: u64 = 1;
/// The `_DSM` function that hands the PPM what the OPM wrote.
pub const DSM_WRITE: u64 = 1;
/// The `_DSM` function that has the PPM fill the mailbox for a read.
pub const DSM_READ: u64 = 2;

/// The `_HID` and `_CID` of a UCSI device.
pub const IDS: [&core::ffi::CStr; 2] = [c"USBC000", c"PNP0CA0"];

/// Where each field of the mailbox is.
pub mod offset {
    /// The version of the specification the PPM follows, in BCD.
    pub const VERSION: usize = 0;
    /// The Command Status and Connector Change Indication.
    pub const CCI: usize = 4;
    /// The command.
    pub const CONTROL: usize = 8;
    /// The command's answer: 16 bytes, or 256 from version 2.0.
    pub const MESSAGE_IN: usize = 16;
}

/// The least a mailbox has: version 1's 48 bytes.
pub const MAILBOX_LEN: usize = 48;

/// The CCI's bits.
pub mod cci {
    /// The connector that changed, if any (bits 1 to 7).
    pub const CONNECTOR_SHIFT: u32 = 1;
    /// The bytes of MESSAGE IN the answer has (bits 8 to 15).
    pub const LENGTH_SHIFT: u32 = 8;
    /// The PPM does not do the command.
    pub const NOT_SUPPORTED: u32 = 1 << 25;
    /// A CANCEL took the command back.
    pub const CANCEL_COMPLETED: u32 = 1 << 26;
    /// PPM_RESET is done.
    pub const RESET_COMPLETED: u32 = 1 << 27;
    /// The PPM is still at the command.
    pub const BUSY: u32 = 1 << 28;
    /// ACK_CC_CI is done.
    pub const ACK_COMMAND: u32 = 1 << 29;
    /// The command failed: GET_ERROR_STATUS says why.
    pub const ERROR: u32 = 1 << 30;
    /// The command is done, its answer in MESSAGE IN.
    pub const COMMAND_COMPLETED: u32 = 1 << 31;
}

/// The commands (Table 4-6).
pub mod command {
    /// Resets the PPM, which forgets every change and notification.
    pub const PPM_RESET: u8 = 0x01;
    /// Takes back the command in progress.
    pub const CANCEL: u8 = 0x02;
    /// Resets a connector, which drops what is plugged in.
    pub const CONNECTOR_RESET: u8 = 0x03;
    /// Acknowledges a command's completion or a connector's change.
    pub const ACK_CC_CI: u8 = 0x04;
    /// Says which changes the PPM tells of.
    pub const SET_NOTIFICATION_ENABLE: u8 = 0x05;
    /// What the PPM can do.
    pub const GET_CAPABILITY: u8 = 0x06;
    /// What a connector can do.
    pub const GET_CONNECTOR_CAPABILITY: u8 = 0x07;
    /// A connector's or its partner's power data objects.
    pub const GET_PDOS: u8 = 0x10;
    /// What is on a connector now.
    pub const GET_CONNECTOR_STATUS: u8 = 0x12;
    /// Why the last command failed.
    pub const GET_ERROR_STATUS: u8 = 0x13;
}

/// ACK_CC_CI's bits: a connector change, and a command, acknowledged.
const ACK_CHANGE: u64 = 1 << 16;
const ACK_COMMAND: u64 = 1 << 17;

/// The changes the PPM is asked to tell of (SET_NOTIFICATION_ENABLE), as
/// the Connector Status Change bits name them: a command completed (0),
/// an external supply (1), the power operation mode (2), the provider's
/// capabilities (5), the negotiated power level (6), a PD reset (7), the
/// alternate modes (8), the battery's charging (9), the partner (11), the
/// power direction (12), a connect or a disconnect (14) and an error
/// (15).
pub const NOTIFICATIONS: u16 = 0xdbe7;

/// How long a command may take, and how often the CCI is looked at while
/// waiting.
const TIMEOUT_MS: u32 = 5000;
const POLL_MS: u32 = 1;

/// What goes wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The PPM did not answer in time.
    Timeout,
    /// The PPM does not do the command.
    NotSupported,
    /// The command failed; GET_ERROR_STATUS's first word says why, or 0.
    Failed(u16),
    /// The `_DSM` could not be run.
    Firmware,
    /// The PPM said something the specification does not allow.
    Malformed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Timeout => f.write_str("the firmware did not answer in 5 s"),
            Error::NotSupported => f.write_str("the firmware does not do that"),
            Error::Failed(0) => f.write_str("the firmware said it failed"),
            Error::Failed(why) => write!(f, "the firmware said it failed (error {why:#06x})"),
            Error::Firmware => f.write_str("the firmware's _DSM could not be run"),
            Error::Malformed => f.write_str("the firmware's answer is malformed"),
        }
    }
}

/// The mailbox, and the firmware that passes it to the PPM.
pub trait Mailbox {
    /// Reads `out.len()` bytes of the mailbox from `offset`.
    fn read(&mut self, offset: usize, out: &mut [u8]);
    /// Writes `data` to the mailbox at `offset`.
    fn write(&mut self, offset: usize, data: &[u8]);
    /// Hands the PPM what was written (`_DSM` function 1).
    fn after_write(&mut self) -> Result<(), Error>;
    /// Has the PPM fill the mailbox before a read (`_DSM` function 2).
    fn before_read(&mut self) -> Result<(), Error>;
    /// Waits `ms` milliseconds.
    fn delay_ms(&mut self, ms: u32);
}

/// What the PPM can do (GET_CAPABILITY, Table 4-14).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capability {
    /// bmAttributes: bit 1 battery charging, 2 USB Power Delivery, 6
    /// USB Type-C current; bits 8 to 15 the machine's power sources.
    pub attributes: u32,
    /// bNumConnectors.
    pub connectors: u8,
    /// bmOptionalFeatures.
    pub features: u32,
    /// bNumAltModes.
    pub alt_modes: u8,
    /// The version of Battery Charging it follows, in BCD.
    pub bc_version: u16,
    /// The version of USB Power Delivery it follows, in BCD.
    pub pd_version: u16,
    /// The version of USB Type-C it follows, in BCD.
    pub typec_version: u16,
}

impl Capability {
    /// Reads GET_CAPABILITY's 16 bytes.
    pub fn parse(data: &[u8]) -> Result<Capability, Error> {
        let data: &[u8; 16] =
            data.get(..16).and_then(|d| d.try_into().ok()).ok_or(Error::Malformed)?;
        let word = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]);
        Ok(Capability {
            attributes: u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            connectors: data[4] & 0x7f,
            features: u32::from_le_bytes([data[5], data[6], data[7], 0]),
            alt_modes: data[8],
            bc_version: word(10),
            pd_version: word(12),
            typec_version: word(14),
        })
    }

    /// It does USB Power Delivery.
    pub fn power_delivery(&self) -> bool {
        self.attributes & 1 << 2 != 0
    }
}

/// What a connector can do (GET_CONNECTOR_CAPABILITY, Table 4-17).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnectorCapability {
    /// The operation modes: bit 0 DFP only, 1 UFP only, 2 dual role,
    /// 3 audio accessories, 4 debug accessories, 5 USB 2, 6 USB 3, 7
    /// alternate modes.
    pub modes: u8,
    /// It can give power.
    pub provider: bool,
    /// It can take power.
    pub consumer: bool,
}

impl ConnectorCapability {
    /// Reads GET_CONNECTOR_CAPABILITY's answer.
    pub fn parse(data: &[u8]) -> Result<ConnectorCapability, Error> {
        let &[modes, flags, ..] = data else { return Err(Error::Malformed) };
        Ok(ConnectorCapability { modes, provider: flags & 1 != 0, consumer: flags & 2 != 0 })
    }

    /// Its data roles, as Linux's `port_type` names them: `dual` for a
    /// dual-role port, `source` for one that is only a host (a DFP),
    /// `sink` for one that is only a device.
    pub fn port_type(&self) -> &'static str {
        match self.modes & 0b111 {
            0b001 => "source",
            0b010 => "sink",
            _ => "dual",
        }
    }
}

/// How power goes over a connection (Power Operation Mode).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PowerMode {
    /// Nothing is said.
    #[default]
    None,
    /// USB's default: 500 mA on USB 2, 900 mA on USB 3.
    Default,
    /// Battery Charging 1.2.
    BatteryCharging,
    /// A USB Power Delivery contract.
    PowerDelivery,
    /// USB Type-C current at 1.5 A.
    TypeC1_5,
    /// USB Type-C current at 3 A.
    TypeC3_0,
    /// USB Type-C current at 5 A (version 3.0).
    TypeC5_0,
}

impl PowerMode {
    /// As Linux's `power_operation_mode` names it.
    pub fn as_str(self) -> &'static str {
        match self {
            PowerMode::None | PowerMode::Default | PowerMode::BatteryCharging => "default",
            PowerMode::PowerDelivery => "usb_power_delivery",
            PowerMode::TypeC1_5 => "1.5A",
            PowerMode::TypeC3_0 => "3.0A",
            PowerMode::TypeC5_0 => "5.0A",
        }
    }
}

/// What is at the other end (Connector Partner Type).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Partner {
    /// Nothing, or what the specification does not name.
    #[default]
    None,
    /// A host: this port is the device.
    Host,
    /// A device: this port is the host.
    Device,
    /// A cable that draws power, with nothing at its end.
    Cable,
    /// A cable that draws power, with a device at its end.
    CableAndDevice,
    /// A debug accessory.
    Debug,
    /// An analog audio adapter.
    Audio,
}

/// How a battery charges from the port (Battery Charging Capability
/// Status).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Charging {
    /// It is not charging.
    #[default]
    No,
    /// As fast as it can.
    Nominal,
    /// Slowly: the supply gives less than the machine takes.
    Slow,
    /// Very slowly.
    VerySlow,
}

/// What is on a connector now (GET_CONNECTOR_STATUS, Table 4-46).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnectorStatus {
    /// What changed since the last acknowledged change (the bits of
    /// [`NOTIFICATIONS`]).
    pub change: u16,
    /// How power goes.
    pub power: PowerMode,
    /// Something is plugged in.
    pub connected: bool,
    /// The machine gives power; else it takes it.
    pub provider: bool,
    /// Connector Partner Flags: bit 0 USB, bit 1 an alternate mode.
    pub partner_flags: u8,
    /// What is at the other end.
    pub partner: Partner,
    /// The Request Data Object of a Power Delivery contract.
    pub rdo: u32,
    /// How the battery charges from the port.
    pub charging: Charging,
}

impl ConnectorStatus {
    /// Reads GET_CONNECTOR_STATUS's answer: its first 9 bytes, which
    /// every version has.
    pub fn parse(data: &[u8]) -> Result<ConnectorStatus, Error> {
        let data: &[u8; 9] =
            data.get(..9).and_then(|d| d.try_into().ok()).ok_or(Error::Malformed)?;
        let flags = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let power = match flags >> 16 & 0b111 {
            1 => PowerMode::Default,
            2 => PowerMode::BatteryCharging,
            3 => PowerMode::PowerDelivery,
            4 => PowerMode::TypeC1_5,
            5 => PowerMode::TypeC3_0,
            6 => PowerMode::TypeC5_0,
            _ => PowerMode::None,
        };
        let partner = match flags >> 29 {
            1 => Partner::Host,
            2 => Partner::Device,
            3 => Partner::Cable,
            4 => Partner::CableAndDevice,
            5 => Partner::Debug,
            6 => Partner::Audio,
            _ => Partner::None,
        };
        let charging = match data[8] & 0b11 {
            1 => Charging::Nominal,
            2 => Charging::Slow,
            3 => Charging::VerySlow,
            _ => Charging::No,
        };
        Ok(ConnectorStatus {
            change: flags as u16,
            power,
            connected: flags & 1 << 19 != 0,
            provider: flags & 1 << 20 != 0,
            partner_flags: (flags >> 21) as u8,
            partner,
            rdo: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
            charging,
        })
    }

    /// The power data object the contract is for: its position in the
    /// source's list, from 1 (the RDO's bits 28 to 31); 0 for none.
    pub fn object_position(&self) -> u8 {
        if self.power == PowerMode::PowerDelivery { (self.rdo >> 28) as u8 } else { 0 }
    }

    /// Linux's `data_role`: `host` or `device`.
    pub fn data_role(&self) -> &'static str {
        match self.partner {
            Partner::Host => "device",
            _ => "host",
        }
    }

    /// Linux's `power_role`: `source` or `sink`.
    pub fn power_role(&self) -> &'static str {
        if self.provider { "source" } else { "sink" }
    }
}

/// A power data object (USB Power Delivery 3.1, 6.4.1): what a source
/// offers or a sink asks, in mV, mA and mW.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)] // Each variant's fields are named for what they are.
pub enum Pdo {
    /// A fixed supply: its voltage and its most current.
    Fixed { mv: u32, ma: u32 },
    /// A battery: a range of voltages, and the most power.
    Battery { min_mv: u32, max_mv: u32, mw: u32 },
    /// A supply of variable voltage: its range, and its most current.
    Variable { min_mv: u32, max_mv: u32, ma: u32 },
    /// A programmable power supply (PPS): its range, and its most
    /// current.
    Programmable { min_mv: u32, max_mv: u32, ma: u32 },
    /// Another augmented object (EPR's adjustable voltage supply, and
    /// later kinds).
    Other(u32),
}

impl Pdo {
    /// The object `pdo`.
    pub fn parse(pdo: u32) -> Pdo {
        let field = |high: u32, low: u32| pdo >> low & ((1 << (high - low + 1)) - 1);
        match pdo >> 30 {
            0 => Pdo::Fixed { mv: field(19, 10) * 50, ma: field(9, 0) * 10 },
            1 => Pdo::Battery {
                max_mv: field(29, 20) * 50,
                min_mv: field(19, 10) * 50,
                mw: field(9, 0) * 250,
            },
            2 => Pdo::Variable {
                max_mv: field(29, 20) * 50,
                min_mv: field(19, 10) * 50,
                ma: field(9, 0) * 10,
            },
            _ if field(29, 28) == 0 => Pdo::Programmable {
                max_mv: field(24, 17) * 100,
                min_mv: field(15, 8) * 100,
                ma: field(6, 0) * 50,
            },
            _ => Pdo::Other(pdo),
        }
    }
}

/// A Power Delivery contract: the voltage and current agreed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contract {
    /// The voltage, in mV.
    pub mv: u32,
    /// The current, in mA.
    pub ma: u32,
}

impl Contract {
    /// The contract that `rdo` asks for of the object `pdo` it names
    /// (USB Power Delivery 3.1, 6.4.2): a fixed or variable supply's
    /// voltage, a programmable one's asked voltage, and the operating
    /// current; a battery's operating power at its lowest voltage.
    pub fn of(pdo: Pdo, rdo: u32) -> Option<Contract> {
        let field = |high: u32, low: u32| rdo >> low & ((1 << (high - low + 1)) - 1);
        match pdo {
            Pdo::Fixed { mv, .. } => Some(Contract { mv, ma: field(19, 10) * 10 }),
            Pdo::Variable { min_mv, .. } => Some(Contract { mv: min_mv, ma: field(19, 10) * 10 }),
            Pdo::Battery { min_mv, .. } if min_mv > 0 => {
                Some(Contract { mv: min_mv, ma: field(19, 10) * 250 * 1000 / min_mv })
            }
            Pdo::Programmable { .. } => {
                Some(Contract { mv: field(20, 9) * 20, ma: field(6, 0) * 50 })
            }
            _ => None,
        }
    }

    /// Its power, in mW.
    pub fn mw(&self) -> u32 {
        self.mv * self.ma / 1000
    }
}

/// Writes `mv` as volts, as 20 V or 5.5 V.
pub struct Volts(pub u32);

impl fmt::Display for Volts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, tenths) = (self.0 / 1000, self.0 % 1000 / 100);
        if tenths == 0 { write!(f, "{whole} V") } else { write!(f, "{whole}.{tenths} V") }
    }
}

/// Writes `ma` as amperes, as 3 A or 1.5 A.
pub struct Amperes(pub u32);

impl fmt::Display for Amperes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, rest) = (self.0 / 1000, self.0 % 1000);
        if rest == 0 {
            write!(f, "{whole} A")
        } else if rest % 100 == 0 {
            write!(f, "{whole}.{} A", rest / 100)
        } else {
            write!(f, "{whole}.{:02} A", rest / 10)
        }
    }
}

/// What a connector has, in words, for the log: what is plugged in, and
/// the power it gives or takes.
pub struct Describe<'a> {
    /// What is on the connector.
    pub status: &'a ConnectorStatus,
    /// Its Power Delivery contract, if it has one and it is known.
    pub contract: Option<Contract>,
}

impl fmt::Display for Describe<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = self.status;
        if !status.connected {
            return f.write_str("nothing is plugged in");
        }
        // A charger is a host without USB's data; a dock or a display
        // that charges the machine is a device that gives it power.
        let usb = status.partner_flags & 1 != 0;
        let what = match (status.partner, status.provider) {
            (Partner::Host | Partner::None, false) if !usb => "a charger",
            (Partner::Host, false) => "a host that powers this machine",
            (Partner::Host, true) => "a host",
            (Partner::Device | Partner::CableAndDevice, true) => "a device",
            (Partner::Device | Partner::CableAndDevice, false) => {
                "a dock or a display that powers this machine"
            }
            (Partner::Cable, _) => "a cable with nothing at its end",
            (Partner::Debug, _) => "a debug accessory",
            (Partner::Audio, _) => "an audio adapter",
            (Partner::None, _) => "something",
        };
        f.write_str(what)?;
        if status.partner_flags & 2 != 0 {
            f.write_str(" in an alternate mode")?;
        }
        let direction = if status.provider { "gives" } else { "takes" };
        match (status.power, self.contract) {
            (PowerMode::PowerDelivery, Some(contract)) => write!(
                f,
                "; the machine {direction} {} at {} ({} W) by USB Power Delivery",
                Volts(contract.mv),
                Amperes(contract.ma),
                contract.mw() / 1000
            )?,
            (PowerMode::PowerDelivery, None) => {
                write!(f, "; the machine {direction} power by USB Power Delivery")?;
            }
            (PowerMode::TypeC1_5, _) => write!(f, "; the machine {direction} 5 V at 1.5 A")?,
            (PowerMode::TypeC3_0, _) => write!(f, "; the machine {direction} 5 V at 3 A")?,
            (PowerMode::TypeC5_0, _) => write!(f, "; the machine {direction} 5 V at 5 A")?,
            (PowerMode::BatteryCharging, _) => {
                write!(f, "; the machine {direction} power by Battery Charging 1.2")?;
            }
            (PowerMode::Default | PowerMode::None, _) => {
                write!(f, "; the machine {direction} USB's default power")?;
            }
        }
        match (status.provider, status.charging) {
            (true, _) | (false, Charging::No) => Ok(()),
            (false, Charging::Nominal) => f.write_str("; charging"),
            (false, Charging::Slow) => f.write_str("; charging slowly"),
            (false, Charging::VerySlow) => f.write_str("; charging very slowly"),
        }
    }
}

/// The PPM, through its mailbox.
pub struct Ppm<M: Mailbox> {
    mailbox: M,
    /// The version it follows, in BCD.
    version: u16,
    /// The connectors that changed while a command ran, by bit.
    pending: u128,
}

impl<M: Mailbox> Ppm<M> {
    /// The PPM behind `mailbox`, reset: its version read, and every
    /// change it remembers forgotten (PPM_RESET).
    pub fn reset(mailbox: M) -> Result<Ppm<M>, Error> {
        let mut ppm = Ppm { mailbox, version: 0, pending: 0 };
        ppm.control(u64::from(command::PPM_RESET))?;
        // The reset may take a while; the PPM says when it is done.
        ppm.wait(|cci| cci & cci::RESET_COMPLETED != 0)?;
        let mut version = [0; 2];
        ppm.mailbox.read(offset::VERSION, &mut version);
        ppm.version = u16::from_le_bytes(version);
        Ok(ppm)
    }

    /// The version of the specification it follows, in BCD: 0x0120 for
    /// 1.2.
    pub fn version(&self) -> u16 {
        self.version
    }

    /// The mailbox.
    pub fn mailbox(&mut self) -> &mut M {
        &mut self.mailbox
    }

    /// Writes `control` and hands it to the PPM.
    fn control(&mut self, control: u64) -> Result<(), Error> {
        self.mailbox.write(offset::CONTROL, &control.to_le_bytes());
        self.mailbox.after_write()
    }

    /// The CCI, once the PPM filled the mailbox.
    fn cci(&mut self) -> Result<u32, Error> {
        self.mailbox.before_read()?;
        let mut cci = [0; 4];
        self.mailbox.read(offset::CCI, &mut cci);
        Ok(u32::from_le_bytes(cci))
    }

    /// Waits until the CCI shows `done`; returns it. A connector change
    /// it shows meanwhile is kept for [`Ppm::changed`].
    fn wait(&mut self, done: impl Fn(u32) -> bool) -> Result<u32, Error> {
        let mut waited = 0;
        loop {
            let cci = self.cci()?;
            let connector = cci >> cci::CONNECTOR_SHIFT & 0x7f;
            if connector != 0 {
                self.pending |= 1 << connector;
            }
            if done(cci) {
                return Ok(cci);
            }
            if waited >= TIMEOUT_MS {
                return Err(Error::Timeout);
            }
            self.mailbox.delay_ms(POLL_MS);
            waited += POLL_MS;
        }
    }

    /// Runs the command `control`, puts its answer in `out` and
    /// acknowledges it; returns how many bytes the answer has.
    pub fn command(&mut self, control: u64, out: &mut [u8]) -> Result<usize, Error> {
        self.control(control)?;
        let cci = self.wait(|cci| cci & (cci::COMMAND_COMPLETED | cci::ERROR) != 0)?;
        let length = (cci >> cci::LENGTH_SHIFT & 0xff) as usize;
        let failed = cci & cci::ERROR != 0;
        let unsupported = cci & cci::NOT_SUPPORTED != 0;
        let n = length.min(out.len());
        if !failed {
            self.mailbox.read(offset::MESSAGE_IN, &mut out[..n]);
        }
        self.acknowledge(ACK_COMMAND)?;
        if unsupported {
            return Err(Error::NotSupported);
        }
        if failed {
            return Err(Error::Failed(self.error_status()));
        }
        Ok(n)
    }

    /// Acknowledges a command or a change (ACK_CC_CI), and waits for the
    /// PPM to say it has seen it.
    fn acknowledge(&mut self, what: u64) -> Result<(), Error> {
        self.control(u64::from(command::ACK_CC_CI) | what)?;
        self.wait(|cci| cci & cci::ACK_COMMAND != 0).map(drop)
    }

    /// Why the last command failed: GET_ERROR_STATUS's first word, or 0
    /// if that fails too.
    fn error_status(&mut self) -> u16 {
        let mut status = [0; 16];
        let control = u64::from(command::GET_ERROR_STATUS);
        if self.control(control).is_err() {
            return 0;
        }
        let Ok(cci) = self.wait(|cci| cci & (cci::COMMAND_COMPLETED | cci::ERROR) != 0) else {
            return 0;
        };
        if cci & cci::ERROR == 0 {
            self.mailbox.read(offset::MESSAGE_IN, &mut status);
        }
        let _ = self.acknowledge(ACK_COMMAND);
        u16::from_le_bytes([status[0], status[1]])
    }

    /// Asks the PPM to tell of the changes `mask` names
    /// (SET_NOTIFICATION_ENABLE).
    pub fn enable_notifications(&mut self, mask: u16) -> Result<(), Error> {
        let control = u64::from(command::SET_NOTIFICATION_ENABLE) | u64::from(mask) << 16;
        self.command(control, &mut []).map(drop)
    }

    /// What it can do (GET_CAPABILITY).
    pub fn capability(&mut self) -> Result<Capability, Error> {
        let mut data = [0; 16];
        let n = self.command(u64::from(command::GET_CAPABILITY), &mut data)?;
        Capability::parse(&data[..n])
    }

    /// What connector `number` (from 1) can do (GET_CONNECTOR_CAPABILITY).
    pub fn connector_capability(&mut self, number: u8) -> Result<ConnectorCapability, Error> {
        let mut data = [0; 16];
        let control = u64::from(command::GET_CONNECTOR_CAPABILITY) | connector(number);
        let n = self.command(control, &mut data)?;
        ConnectorCapability::parse(&data[..n])
    }

    /// What is on connector `number` now (GET_CONNECTOR_STATUS).
    pub fn connector_status(&mut self, number: u8) -> Result<ConnectorStatus, Error> {
        let mut data = [0; 16];
        let control = u64::from(command::GET_CONNECTOR_STATUS) | connector(number);
        let n = self.command(control, &mut data)?;
        ConnectorStatus::parse(&data[..n])
    }

    /// The first four power data objects of connector `number`'s partner
    /// (or its own), as a source (or a sink): GET_PDOS. Returns how many
    /// there are.
    pub fn pdos(
        &mut self,
        number: u8,
        partner: bool,
        source: bool,
        out: &mut [u32; 4],
    ) -> Result<usize, Error> {
        // From the first, four of them, the current capabilities.
        let control = u64::from(command::GET_PDOS)
            | connector(number)
            | u64::from(partner) << 23
            | 3 << 27
            | u64::from(source) << 29;
        let mut data = [0; 16];
        let n = self.command(control, &mut data)? / 4;
        for (pdo, bytes) in out.iter_mut().zip(data.as_chunks::<4>().0).take(n) {
            *pdo = u32::from_le_bytes(*bytes);
        }
        Ok(n)
    }

    /// The contract a sink connector has with its partner, from the
    /// partner's source objects and the connector's request.
    pub fn contract(&mut self, number: u8, status: &ConnectorStatus) -> Option<Contract> {
        let position = usize::from(status.object_position());
        if position == 0 || position > 4 {
            return None;
        }
        let mut pdos = [0; 4];
        // A sink asked for one of its partner's source objects; a source
        // offered its own.
        let partner = !status.provider;
        let n = self.pdos(number, partner, true, &mut pdos).ok()?;
        let pdo = Pdo::parse(*pdos.get(position - 1).filter(|_| position <= n)?);
        Contract::of(pdo, status.rdo)
    }

    /// A connector that changed and has not been acknowledged yet, the
    /// lowest first: one a command saw, or one the CCI shows now.
    pub fn changed(&mut self) -> Result<Option<u8>, Error> {
        if self.pending == 0 {
            let cci = self.cci()?;
            let connector = cci >> cci::CONNECTOR_SHIFT & 0x7f;
            if connector != 0 {
                self.pending |= 1 << connector;
            }
        }
        if self.pending == 0 {
            return Ok(None);
        }
        Ok(Some(self.pending.trailing_zeros() as u8))
    }

    /// Acknowledges the change on connector `number`, read by
    /// [`Ppm::connector_status`].
    pub fn ack_change(&mut self, number: u8) -> Result<(), Error> {
        self.pending &= !(1 << number);
        self.acknowledge(ACK_CHANGE)
    }
}

/// A command's connector number (bits 16 to 22).
fn connector(number: u8) -> u64 {
    u64::from(number & 0x7f) << 16
}
