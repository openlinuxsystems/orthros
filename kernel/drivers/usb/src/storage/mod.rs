// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Mass storage interfaces, as the *Universal Serial Bus Mass Storage
//! Class Bulk-Only Transport, Revision 1.0* has USB sticks, card readers
//! and USB disks carry SCSI commands (Phase 11b, ADR 0161).
//!
//! A Bulk-Only interface (class 8, subclass 6 for SCSI, protocol 50h) has
//! a bulk IN and a bulk OUT endpoint. Each command goes in three stages:
//! the host sends a Command Block Wrapper ([`Cbw`]) on the OUT pipe, the
//! data moves on one pipe or the other, and the device answers with a
//! Command Status Wrapper ([`Csw`]) on the IN pipe. A command that failed
//! has the host ask the device why, with REQUEST SENSE. [`Bot`] runs a
//! command over the pipes that the host controller's driver carries
//! ([`Pipes`]), and recovers as the specification and Linux's usb-storage
//! do: a pipe that stalled has its halt cleared, and a device that sent
//! what it should not have, or did not answer, is reset (Bulk-Only Mass
//! Storage Reset, then both pipes' halts cleared). It also bears with
//! two ways sticks are known to break the specification, as Linux does
//! without being told of the stick: a status whose signature is not
//! "USBS" is taken if every status has the same one, and a stick that
//! says data did not move when all of it did, for an INQUIRY or a READ
//! CAPACITY, has what it says of that ignored from then on.

use radix_scsi::{Cdb, Data, Reply, SENSE_LENGTH, Sense, Transport, TransportError};

use crate::{CLASS, DEVICE_TO_HOST, Setup, TO_INTERFACE};

#[cfg(test)]
mod tests;

/// The mass storage class's SCSI subclass: the commands are SCSI's.
pub const SUBCLASS_SCSI: u8 = 0x06;
/// The Bulk-Only Transport protocol.
pub const PROTOCOL_BULK_ONLY: u8 = 0x50;
/// The USB Attached SCSI protocol (UAS).
pub const PROTOCOL_UAS: u8 = 0x62;

/// The class's requests (Bulk-Only Transport 1.0, 3.1 and 3.2).
pub mod request {
    /// Bulk-Only Mass Storage Reset.
    pub const RESET: u8 = 0xff;
    /// Get Max LUN.
    pub const GET_MAX_LUN: u8 = 0xfe;
}

/// A Command Block Wrapper's length.
pub const CBW_LENGTH: usize = 31;
/// A Command Status Wrapper's length.
pub const CSW_LENGTH: usize = 13;
/// "USBC", which starts a Command Block Wrapper.
const CBW_SIGNATURE: u32 = 0x4342_5355;
/// "USBS", which starts a Command Status Wrapper.
pub const CSW_SIGNATURE: u32 = 0x5342_5355;
/// The most logical units an interface has: the LUN field's four bits.
pub const MAX_LUNS: usize = 16;

impl Setup {
    /// Get Max LUN of interface `interface`: its highest logical unit's
    /// number, in one byte.
    pub fn get_max_lun(interface: u8) -> Setup {
        Setup {
            request_type: DEVICE_TO_HOST | CLASS | TO_INTERFACE,
            request: request::GET_MAX_LUN,
            value: 0,
            index: u16::from(interface),
            length: 1,
        }
    }

    /// Bulk-Only Mass Storage Reset of interface `interface`: it gets
    /// ready for the next Command Block Wrapper.
    pub fn bulk_only_reset(interface: u8) -> Setup {
        Setup {
            request_type: CLASS | TO_INTERFACE,
            request: request::RESET,
            value: 0,
            index: u16::from(interface),
            length: 0,
        }
    }
}

/// A Command Block Wrapper (Bulk-Only Transport 1.0, 5.1): a command, its
/// tag, and how much data it moves which way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cbw {
    /// dCBWTag, which the status's must match.
    pub tag: u32,
    /// dCBWDataTransferLength.
    pub length: u32,
    /// Whether the data comes from the device.
    pub into_host: bool,
    /// bCBWLUN.
    pub lun: u8,
    /// The command.
    pub cdb: Cdb,
}

impl Cbw {
    /// Its 31 bytes.
    pub fn to_bytes(&self) -> [u8; CBW_LENGTH] {
        let mut bytes = [0; CBW_LENGTH];
        bytes[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.tag.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.length.to_le_bytes());
        bytes[12] = if self.into_host { 0x80 } else { 0 };
        bytes[13] = self.lun & 0xf;
        let cdb = self.cdb.bytes();
        bytes[14] = cdb.len() as u8;
        bytes[15..15 + cdb.len()].copy_from_slice(cdb);
        bytes
    }
}

/// bCSWStatus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CswStatus {
    /// The command worked.
    Passed,
    /// It failed: REQUEST SENSE says why.
    Failed,
    /// The device and host disagree on the command's stages: a reset is
    /// needed.
    PhaseError,
}

/// A Command Status Wrapper (Bulk-Only Transport 1.0, 5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csw {
    /// dCSWDataResidue: the bytes of data that did not move.
    pub residue: u32,
    /// bCSWStatus.
    pub status: CswStatus,
}

impl Csw {
    /// Reads a Command Status Wrapper, which is valid (6.3.1) if it is 13
    /// bytes, starts with `signature` ([`CSW_SIGNATURE`] for a device that
    /// keeps to the specification) and carries `tag`, and meaningful
    /// (6.3.2) if its status is one the specification names.
    pub fn parse(bytes: &[u8], tag: u32, signature: u32) -> Option<Csw> {
        if bytes.len() != CSW_LENGTH {
            return None;
        }
        let word = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        if word(0) != signature || word(4) != tag {
            return None;
        }
        let status = match bytes[12] {
            0 => CswStatus::Passed,
            1 => CswStatus::Failed,
            2 => CswStatus::PhaseError,
            _ => return None,
        };
        Some(Csw { residue: word(8), status })
    }
}

/// Why a transfer on a pipe failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeError {
    /// The endpoint stalled, after this many bytes moved: the device
    /// refused the transfer, or ended it early.
    Stall(usize),
    /// The device is gone.
    Gone,
    /// The transfer did not finish in time, and was stopped.
    TimedOut,
    /// It failed otherwise (a transaction error, babble).
    Failed,
}

/// The pipes of a Bulk-Only interface, as the host controller's driver
/// carries transfers on them.
pub trait Pipes {
    /// Moves `data` on the bulk IN pipe, if it comes from the device, or
    /// on the bulk OUT pipe, waiting at most `timeout_ms`; returns how
    /// many bytes moved.
    fn bulk(&mut self, data: Data<'_>, timeout_ms: u32) -> Result<usize, PipeError>;

    /// Clears the halt of the bulk IN pipe (`into_host`) or OUT pipe:
    /// the controller's side of it, and the device's with CLEAR_FEATURE.
    fn clear_halt(&mut self, into_host: bool) -> Result<(), PipeError>;

    /// Makes the control transfer `setup` on the default pipe, with its
    /// data in or from `data`; returns how many bytes moved.
    fn control(&mut self, setup: Setup, data: Data<'_>) -> Result<usize, PipeError>;
}

/// A Bulk-Only interface's transport: SCSI commands to its logical units,
/// over its [`Pipes`].
pub struct Bot<P> {
    pipes: P,
    /// Its number, which the class's requests go to.
    interface: u8,
    /// The last command's tag.
    tag: u32,
    /// The signature its statuses start with: the first valid one's.
    signature: Option<u32>,
    /// Whether what its statuses say did not move is ignored: it said
    /// so of data that all moved.
    ignore_residue: bool,
}

impl<P: Pipes> Bot<P> {
    /// The transport of interface `interface`, over `pipes`.
    pub fn new(pipes: P, interface: u8) -> Bot<P> {
        Bot { pipes, interface, tag: 0, signature: None, ignore_residue: false }
    }

    /// Its pipes.
    pub fn pipes(&mut self) -> &mut P {
        &mut self.pipes
    }

    /// The number of its highest logical unit, from Get Max LUN. A device
    /// with one may stall the request, or answer past what the LUN field
    /// holds: 0 then, as on Linux. Fails only if the device is gone.
    pub fn max_lun(&mut self) -> Result<u8, PipeError> {
        let mut byte = [0u8; 1];
        match self.pipes.control(Setup::get_max_lun(self.interface), Data::In(&mut byte)) {
            Ok(1) if usize::from(byte[0]) < MAX_LUNS => Ok(byte[0]),
            Err(PipeError::Gone) => Err(PipeError::Gone),
            _ => Ok(0),
        }
    }

    /// Reset recovery (Bulk-Only Transport 1.0, 5.3.4): the class's reset,
    /// then both pipes' halts cleared.
    pub fn reset(&mut self) -> Result<(), TransportError> {
        let gone = |err| {
            if err == PipeError::Gone {
                TransportError::Gone
            } else {
                TransportError::Failed("the reset failed")
            }
        };
        self.pipes.control(Setup::bulk_only_reset(self.interface), Data::None).map_err(gone)?;
        self.pipes.clear_halt(true).map_err(gone)?;
        self.pipes.clear_halt(false).map_err(gone)
    }

    /// Resets the device after it broke the protocol; fails the command
    /// with `why`.
    fn recover(&mut self, why: &'static str) -> TransportError {
        match self.reset() {
            Err(TransportError::Gone) => TransportError::Gone,
            _ => TransportError::Failed(why),
        }
    }

    /// Reads the status of the command with `tag`, again once if the IN
    /// pipe stalled or brought nothing, as Linux does.
    fn status(&mut self, tag: u32, timeout_ms: u32) -> Result<Csw, TransportError> {
        let mut bytes = [0u8; CSW_LENGTH];
        for attempt in 0..2 {
            match self.pipes.bulk(Data::In(&mut bytes), timeout_ms) {
                Ok(0) if attempt == 0 => {}
                Ok(came) => {
                    // The first status's signature, whatever it is, is
                    // what every one after must start with.
                    let first = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    let signature = self.signature.unwrap_or(first);
                    let csw = Csw::parse(&bytes[..came.min(CSW_LENGTH)], tag, signature);
                    let Some(csw) = csw else {
                        return Err(self.recover("it sent a status that is not valid"));
                    };
                    self.signature = Some(signature);
                    return Ok(csw);
                }
                Err(PipeError::Stall(_)) if attempt == 0 => {
                    self.pipes.clear_halt(true).map_err(|err| match err {
                        PipeError::Gone => TransportError::Gone,
                        _ => self.recover("its status pipe stayed halted"),
                    })?;
                }
                Err(PipeError::Gone) => return Err(TransportError::Gone),
                Err(PipeError::TimedOut) => return Err(self.recover("it did not answer in time")),
                Err(_) => return Err(self.recover("its status could not be read")),
            }
        }
        Err(self.recover("its status could not be read"))
    }

    /// Asks logical unit `lun` why its last command failed.
    fn sense(&mut self, lun: u8, timeout_ms: u32) -> Result<Option<Sense>, TransportError> {
        let mut bytes = [0u8; SENSE_LENGTH as usize];
        let cdb = Cdb::request_sense(SENSE_LENGTH);
        match self.run(lun, &cdb, Data::In(&mut bytes), timeout_ms)? {
            (CswStatus::Passed, residue) => {
                let came = bytes.len().saturating_sub(residue as usize);
                Ok(Sense::parse(&bytes[..came]))
            }
            _ => Ok(None),
        }
    }

    /// Runs one command's three stages; returns its status and residue.
    fn run(
        &mut self,
        lun: u8,
        cdb: &Cdb,
        data: Data<'_>,
        timeout_ms: u32,
    ) -> Result<(CswStatus, u32), TransportError> {
        self.tag = self.tag.wrapping_add(1);
        let tag = self.tag;
        let length = data.len() as u32;
        let into_host = data.into_host();
        let cbw = Cbw { tag, length, into_host, lun, cdb: *cdb }.to_bytes();
        match self.pipes.bulk(Data::Out(&cbw), timeout_ms) {
            Ok(CBW_LENGTH) => {}
            Err(PipeError::Gone) => return Err(TransportError::Gone),
            Err(PipeError::TimedOut) => {
                return Err(self.recover("it did not take a command in time"));
            }
            _ => return Err(self.recover("it refused a command")),
        }
        let mut moved = 0;
        if length > 0 {
            match self.pipes.bulk(data, timeout_ms) {
                Ok(came) => moved = came as u32,
                // The device stalls a transfer it ends early, and then
                // sends its status.
                Err(PipeError::Stall(came)) => {
                    moved = came as u32;
                    self.pipes.clear_halt(into_host).map_err(|err| match err {
                        PipeError::Gone => TransportError::Gone,
                        _ => self.recover("its data pipe stayed halted"),
                    })?;
                }
                Err(PipeError::Gone) => return Err(TransportError::Gone),
                Err(PipeError::TimedOut) => return Err(self.recover("it did not answer in time")),
                Err(PipeError::Failed) => return Err(self.recover("its data did not move")),
            }
        }
        let csw = self.status(tag, timeout_ms)?;
        if csw.status == CswStatus::PhaseError {
            return Err(self.recover("it reported a phase error"));
        }
        // A stick that says data did not move when all of it did, for
        // commands whose lengths every stick takes whole, is not believed
        // again (as Linux's US_FL_IGNORE_RESIDUE, which it sets so).
        let whole = matches!(
            (cdb.opcode(), length),
            (radix_scsi::op::INQUIRY, 36) | (radix_scsi::op::READ_CAPACITY_10, 8)
        );
        if csw.status == CswStatus::Passed && csw.residue != 0 && moved == length && whole {
            self.ignore_residue = true;
        }
        // What did not move, by the controller's count or the device's,
        // whichever is more, as Linux takes it.
        let said = if self.ignore_residue { 0 } else { csw.residue.min(length) };
        Ok((csw.status, length.saturating_sub(moved).max(said)))
    }
}

impl<P: Pipes> Transport for Bot<P> {
    fn command(
        &mut self,
        lun: u8,
        cdb: &Cdb,
        data: Data<'_>,
        timeout_ms: u32,
    ) -> Result<Reply, TransportError> {
        match self.run(lun, cdb, data, timeout_ms)? {
            (CswStatus::Passed, residue) => Ok(Reply::Good { residue }),
            _ => Ok(Reply::Failed(self.sense(lun, timeout_ms)?)),
        }
    }
}
