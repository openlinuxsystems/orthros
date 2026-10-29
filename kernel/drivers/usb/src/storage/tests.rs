// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Bulk-Only Transport against a simulated stick: commands with and
//! without data, failures with their sense, and the stick breaking the
//! protocol in the ways the specification lets it.

use proptest::prelude::*;
use std::collections::HashMap;
use std::vec::Vec;

use radix_scsi::{Capacity, Meaning, Reply, key, op, probe};

use super::*;

/// What the stick does to the next command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    /// Refuses the command block.
    StallCommand,
    /// Ends the data stage with a stall after this many bytes.
    StallData(usize),
    /// Stalls the status stage once.
    StallStatus,
    /// Sends a zero-length packet before its status.
    EmptyStatus,
    /// Sends its status with another tag.
    WrongTag,
    /// Reports a phase error.
    PhaseError,
    /// Reports this residue, whatever moved.
    Residue(u32),
    /// Starts its status with this signature.
    Signature(u32),
    /// Takes the command block, then answers nothing.
    Silent,
}

/// Where the stick is in a command.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stage {
    Command,
    /// The data stage: what it sends, or the block it writes to, then
    /// its status.
    Data {
        into_host: bool,
        length: usize,
        bytes: Vec<u8>,
        write: Option<usize>,
        failed: bool,
    },
    Status(Csw),
    Silent,
}

struct Stick {
    blocks: Vec<[u8; 512]>,
    stage: Stage,
    tag: u32,
    sense: Option<Sense>,
    /// The next command's fault, and the present one's.
    next: Fault,
    fault: Fault,
    /// Halted pipes: IN, OUT.
    halted: [bool; 2],
    resets: u32,
    cleared: Vec<bool>,
    max_lun: Option<u8>,
    gone: bool,
    /// Pulled out when the host resets it.
    pull_at_reset: bool,
    cbws: Vec<[u8; CBW_LENGTH]>,
    /// The memory transfers' frames are in, by address.
    memory: HashMap<u64, Vec<u8>>,
    /// What its statuses start with.
    signature: u32,
}

fn pipe(into_host: bool) -> usize {
    usize::from(!into_host)
}

impl Stick {
    fn new(blocks: usize) -> Stick {
        let mut blocks = std::vec![[0u8; 512]; blocks];
        for (i, block) in blocks.iter_mut().enumerate() {
            block.fill(i as u8);
        }
        Stick {
            blocks,
            stage: Stage::Command,
            tag: 0,
            sense: None,
            next: Fault::None,
            fault: Fault::None,
            halted: [false; 2],
            resets: 0,
            cleared: Vec::new(),
            max_lun: Some(0),
            gone: false,
            pull_at_reset: false,
            cbws: Vec::new(),
            memory: HashMap::new(),
            signature: CSW_SIGNATURE,
        }
    }

    /// What the command block `cbw` asks for: its data, the block it
    /// writes to, or why it fails.
    fn execute(&mut self, cdb: &[u8], into_host: bool) -> Result<(Vec<u8>, Option<usize>), Sense> {
        let fail = |asc| Sense { key: key::ILLEGAL_REQUEST, asc, ascq: 0 };
        let lba = cdb.get(2..6).map_or(0, |b| u32::from_be_bytes(b.try_into().unwrap()) as usize);
        let count = cdb.get(7..9).map_or(0, |b| usize::from(u16::from_be_bytes([b[0], b[1]])));
        let fits = lba + count <= self.blocks.len();
        match cdb[0] {
            op::INQUIRY => {
                let mut bytes = std::vec![0u8; 36];
                bytes[1] = 0x80;
                bytes[2] = 6;
                bytes[8..16].copy_from_slice(b"QEMU    ");
                bytes[16..32].copy_from_slice(b"QEMU HARDDISK   ");
                bytes[32..36].copy_from_slice(b"2.5+");
                Ok((bytes, None))
            }
            op::TEST_UNIT_READY | op::SYNCHRONIZE_CACHE_10 => Ok((Vec::new(), None)),
            op::REQUEST_SENSE => {
                let sense = self.sense.take().unwrap_or(Sense { key: 0, asc: 0, ascq: 0 });
                let mut bytes = std::vec![0u8; 18];
                bytes[0] = 0x70;
                bytes[2] = sense.key;
                bytes[7] = 10;
                bytes[12] = sense.asc;
                Ok((bytes, None))
            }
            op::READ_CAPACITY_10 => {
                let mut bytes = ((self.blocks.len() - 1) as u32).to_be_bytes().to_vec();
                bytes.extend(512u32.to_be_bytes());
                Ok((bytes, None))
            }
            op::MODE_SENSE_6 => Ok((std::vec![3, 0, 0x00, 0], None)),
            op::READ_10 if fits && into_host => {
                Ok((self.blocks[lba..lba + count].iter().flatten().copied().collect(), None))
            }
            op::WRITE_10 if fits && !into_host => Ok((Vec::new(), Some(lba))),
            op::READ_10 | op::WRITE_10 => Err(fail(0x21)),
            _ => Err(fail(0x20)),
        }
    }

    /// Takes a command block.
    fn take(&mut self, cbw: &[u8]) {
        self.cbws.push(cbw.try_into().unwrap());
        self.fault = core::mem::replace(&mut self.next, Fault::None);
        self.tag = u32::from_le_bytes(cbw[4..8].try_into().unwrap());
        let length = u32::from_le_bytes(cbw[8..12].try_into().unwrap()) as usize;
        let into_host = cbw[12] & 0x80 != 0;
        let cdb = cbw[15..15 + usize::from(cbw[14])].to_vec();
        if self.fault == Fault::Silent {
            self.stage = Stage::Silent;
            return;
        }
        let (bytes, write, failed) = match self.execute(&cdb, into_host) {
            Ok((bytes, write)) => (bytes, write, false),
            Err(sense) => {
                self.sense = Some(sense);
                (Vec::new(), None, true)
            }
        };
        self.stage = if length > 0 {
            Stage::Data { into_host, length, bytes, write, failed }
        } else {
            let status = if failed { CswStatus::Failed } else { CswStatus::Passed };
            Stage::Status(Csw { residue: 0, status })
        };
    }

    /// Copies `bytes` into `data`; returns how many fit.
    fn give(&mut self, data: Data<'_>, bytes: &[u8]) -> usize {
        match data {
            Data::In(buffer) => {
                let n = bytes.len().min(buffer.len());
                buffer[..n].copy_from_slice(&bytes[..n]);
                n
            }
            Data::Frames { frames, .. } => {
                let mut at = 0;
                for &(phys, len) in frames {
                    let frame = self.memory.get_mut(&phys).unwrap();
                    let n = (len as usize).min(bytes.len() - at);
                    frame[..n].copy_from_slice(&bytes[at..at + n]);
                    at += n;
                }
                at
            }
            _ => unreachable!(),
        }
    }

    /// The bytes of `data`.
    fn bytes_of(&self, data: &Data<'_>) -> Vec<u8> {
        match data {
            Data::Out(bytes) => bytes.to_vec(),
            Data::Frames { frames, .. } => frames
                .iter()
                .flat_map(|&(phys, len)| self.memory[&phys][..len as usize].to_vec())
                .collect(),
            _ => unreachable!(),
        }
    }
}

impl Pipes for Stick {
    fn bulk(&mut self, data: Data<'_>, _timeout_ms: u32) -> Result<usize, PipeError> {
        if self.gone {
            return Err(PipeError::Gone);
        }
        let into_host = data.into_host();
        if self.halted[pipe(into_host)] {
            return Err(PipeError::Stall(0));
        }
        match (self.stage.clone(), into_host) {
            (Stage::Command, false) => {
                let cbw = self.bytes_of(&data);
                if self.next == Fault::StallCommand {
                    self.next = Fault::None;
                    self.halted[pipe(false)] = true;
                    return Err(PipeError::Stall(0));
                }
                assert_eq!(cbw.len(), CBW_LENGTH);
                self.take(&cbw);
                Ok(CBW_LENGTH)
            }
            (Stage::Data { into_host: true, length, bytes, failed, .. }, true) => {
                let stall = match self.fault {
                    Fault::StallData(after) => Some(after),
                    _ if failed => Some(0),
                    _ => None,
                };
                let sent = &bytes[..bytes.len().min(stall.unwrap_or(usize::MAX))];
                let n = self.give(data, sent);
                let status = if failed { CswStatus::Failed } else { CswStatus::Passed };
                self.stage = Stage::Status(Csw { residue: (length - n) as u32, status });
                if stall.is_some() {
                    self.halted[pipe(true)] = true;
                    return Err(PipeError::Stall(n));
                }
                Ok(n)
            }
            (Stage::Data { into_host: false, length, write, failed, .. }, false) => {
                let bytes = self.bytes_of(&data);
                let n = match self.fault {
                    Fault::StallData(after) => after.min(bytes.len()),
                    _ if failed => 0,
                    _ => bytes.len(),
                };
                if let Some(lba) = write {
                    for (i, block) in bytes[..n].chunks(512).enumerate() {
                        self.blocks[lba + i][..block.len()].copy_from_slice(block);
                    }
                }
                let status = if failed { CswStatus::Failed } else { CswStatus::Passed };
                self.stage = Stage::Status(Csw { residue: (length - n) as u32, status });
                if n < bytes.len() {
                    self.halted[pipe(false)] = true;
                    return Err(PipeError::Stall(n));
                }
                Ok(n)
            }
            (Stage::Status(mut csw), true) => {
                match self.fault {
                    Fault::StallStatus => {
                        self.fault = Fault::None;
                        self.halted[pipe(true)] = true;
                        return Err(PipeError::Stall(0));
                    }
                    Fault::EmptyStatus => {
                        self.fault = Fault::None;
                        return Ok(0);
                    }
                    Fault::PhaseError => csw.status = CswStatus::PhaseError,
                    Fault::Residue(residue) => csw.residue = residue,
                    _ => {}
                }
                let tag = if self.fault == Fault::WrongTag { self.tag + 7 } else { self.tag };
                let signature = match self.fault {
                    Fault::Signature(signature) => signature,
                    _ => self.signature,
                };
                let mut bytes = signature.to_le_bytes().to_vec();
                bytes.extend(tag.to_le_bytes());
                bytes.extend(csw.residue.to_le_bytes());
                bytes.push(match csw.status {
                    CswStatus::Passed => 0,
                    CswStatus::Failed => 1,
                    CswStatus::PhaseError => 2,
                });
                self.stage = Stage::Command;
                Ok(self.give(data, &bytes))
            }
            (Stage::Silent, _) => Err(PipeError::TimedOut),
            // Out of step with the host: a real stick stalls.
            _ => {
                self.halted[pipe(into_host)] = true;
                Err(PipeError::Stall(0))
            }
        }
    }

    fn clear_halt(&mut self, into_host: bool) -> Result<(), PipeError> {
        if self.gone {
            return Err(PipeError::Gone);
        }
        self.halted[pipe(into_host)] = false;
        self.cleared.push(into_host);
        Ok(())
    }

    fn control(&mut self, setup: Setup, data: Data<'_>) -> Result<usize, PipeError> {
        if self.gone {
            return Err(PipeError::Gone);
        }
        assert_eq!(setup.index, 2, "the interface's number");
        match setup.request {
            request::GET_MAX_LUN => match self.max_lun {
                Some(lun) => Ok(self.give(data, &[lun])),
                None => Err(PipeError::Stall(0)),
            },
            request::RESET => {
                assert_eq!(setup.request_type, 0x21);
                if self.pull_at_reset {
                    self.gone = true;
                    return Err(PipeError::Gone);
                }
                self.resets += 1;
                self.stage = Stage::Command;
                Ok(0)
            }
            _ => Err(PipeError::Stall(0)),
        }
    }
}

fn bot(blocks: usize) -> Bot<Stick> {
    Bot::new(Stick::new(blocks), 2)
}

/// Frames of `pages` 4 KiB pages in the stick's memory.
fn frames(bot: &mut Bot<Stick>, pages: u64) -> Vec<(u64, u32)> {
    (0..pages)
        .map(|i| {
            let phys = 0x1000_0000 + i * 0x1000;
            bot.pipes().memory.insert(phys, std::vec![0xee; 4096]);
            (phys, 4096)
        })
        .collect()
}

#[test]
fn a_command_block_says_its_tag_length_direction_lun_and_command() {
    let cbw = Cbw {
        tag: 0x1234_5678,
        length: 0x2_0000,
        into_host: true,
        lun: 0x13,
        cdb: Cdb::read(0x10, 0x100, false).unwrap(),
    };
    let bytes = cbw.to_bytes();
    assert_eq!(&bytes[..4], b"USBC");
    assert_eq!(bytes[4..8], [0x78, 0x56, 0x34, 0x12]);
    assert_eq!(bytes[8..12], [0, 0, 2, 0]);
    assert_eq!(bytes[12..15], [0x80, 3, 10]);
    assert_eq!(bytes[15..25], [op::READ_10, 0, 0, 0, 0, 0x10, 0, 1, 0, 0]);
    assert_eq!(bytes[25..], [0; 6]);
    let mut csw = [0x55, 0x53, 0x42, 0x53, 9, 0, 0, 0, 0x10, 0, 0, 0, 1];
    assert_eq!(
        Csw::parse(&csw, 9, CSW_SIGNATURE),
        Some(Csw { residue: 16, status: CswStatus::Failed })
    );
    assert_eq!(Csw::parse(&csw, 8, CSW_SIGNATURE), None);
    assert_eq!(Csw::parse(&csw[..12], 9, CSW_SIGNATURE), None);
    csw[12] = 3;
    assert_eq!(Csw::parse(&csw, 9, CSW_SIGNATURE), None);
    csw[0] = b'X';
    csw[12] = 0;
    assert_eq!(Csw::parse(&csw, 9, CSW_SIGNATURE), None);
    assert!(Csw::parse(&csw, 9, u32::from_le_bytes(*b"XSBS")).is_some());
    assert_eq!(Setup::get_max_lun(2).request_type, 0xa1);
    assert_eq!(Setup::bulk_only_reset(2).length, 0);
}

#[test]
fn reads_and_writes_move_blocks_between_the_stick_and_memory() {
    let mut bot = bot(64);
    let pages = frames(&mut bot, 2);
    let cdb = Cdb::read(3, 16, false).unwrap();
    let reply = bot.command(0, &cdb, Data::Frames { into_host: true, frames: &pages }, 1000);
    assert_eq!(reply, Ok(Reply::Good { residue: 0 }));
    let memory = &bot.pipes().memory;
    assert!(memory[&pages[0].0][..512].iter().all(|&b| b == 3));
    assert!(memory[&pages[1].0][3584..].iter().all(|&b| b == 18));
    // The block command and its data stage went as the specification
    // has them.
    let cbw = bot.pipes().cbws[0];
    assert_eq!(cbw[8..12], 8192u32.to_le_bytes());
    assert_eq!(cbw[12], 0x80);
    for (phys, _) in &pages {
        bot.pipes().memory.get_mut(phys).unwrap().fill(0xab);
    }
    let cdb = Cdb::write(40, 16, false).unwrap();
    let reply = bot.command(0, &cdb, Data::Frames { into_host: false, frames: &pages }, 1000);
    assert_eq!(reply, Ok(Reply::Good { residue: 0 }));
    assert!(bot.pipes().blocks[40..56].iter().flatten().all(|&b| b == 0xab));
    assert_eq!(bot.pipes().blocks[56][0], 56);
    let reply = bot.command(0, &Cdb::synchronize_cache(), Data::None, 1000);
    assert_eq!(reply, Ok(Reply::Good { residue: 0 }));
    // Each command has its own tag.
    let tags: Vec<u8> = bot.pipes().cbws.iter().map(|cbw| cbw[4]).collect();
    assert_eq!(tags, [1, 2, 3]);
}

#[test]
fn a_failed_command_says_why_through_request_sense() {
    let mut bot = bot(64);
    let pages = frames(&mut bot, 1);
    let cdb = Cdb::read(60, 8, false).unwrap();
    let reply = bot.command(0, &cdb, Data::Frames { into_host: true, frames: &pages }, 1000);
    let sense = Sense { key: key::ILLEGAL_REQUEST, asc: 0x21, ascq: 0 };
    assert_eq!(reply, Ok(Reply::Failed(Some(sense))));
    // The stick stalled the data stage; its halt was cleared.
    assert_eq!(bot.pipes().cleared, [true]);
    assert_eq!(bot.pipes().cbws[1][15], op::REQUEST_SENSE);
    let reply = bot.command(0, &Cdb::new(&[0xd9, 0, 0, 0, 0, 0]), Data::None, 1000);
    let sense = Sense { key: key::ILLEGAL_REQUEST, asc: 0x20, ascq: 0 };
    assert_eq!(reply, Ok(Reply::Failed(Some(sense))));
    assert_eq!(sense.meaning(), Meaning::Unsupported);
    assert_eq!(bot.pipes().resets, 0);
}

#[test]
fn a_data_stage_cut_short_counts_what_moved() {
    let mut bot = bot(64);
    bot.pipes().next = Fault::StallData(1024);
    let pages = frames(&mut bot, 1);
    let cdb = Cdb::read(0, 8, false).unwrap();
    let reply = bot.command(0, &cdb, Data::Frames { into_host: true, frames: &pages }, 1000);
    assert_eq!(reply, Ok(Reply::Good { residue: 3072 }));
    assert_eq!(bot.pipes().cleared, [true]);
    // A write cut short: the OUT pipe's halt is cleared.
    bot.pipes().next = Fault::StallData(512);
    let cdb = Cdb::write(0, 8, false).unwrap();
    let reply = bot.command(0, &cdb, Data::Frames { into_host: false, frames: &pages }, 1000);
    assert_eq!(reply, Ok(Reply::Good { residue: 3584 }));
    assert_eq!(bot.pipes().cleared, [true, false]);
    // A short answer: INQUIRY of 96 bytes brings 36.
    let mut bytes = [0u8; 96];
    let reply = bot.command(0, &Cdb::inquiry(96), Data::In(&mut bytes), 1000);
    assert_eq!(reply, Ok(Reply::Good { residue: 60 }));
    // A residue past the length is held to it, and one under what the
    // controller counted is overruled.
    bot.pipes().next = Fault::Residue(1 << 20);
    let mut block = [0u8; 512];
    let reply = bot.command(0, &Cdb::read(0, 1, false).unwrap(), Data::In(&mut block), 1000);
    assert_eq!(reply, Ok(Reply::Good { residue: 512 }));
    bot.pipes().next = Fault::Residue(0);
    let reply = bot.command(0, &Cdb::inquiry(96), Data::In(&mut bytes), 1000);
    assert_eq!(reply, Ok(Reply::Good { residue: 60 }));
    assert_eq!(bot.pipes().resets, 0);
}

#[test]
fn a_status_stalled_or_empty_is_read_again() {
    let mut bot = bot(64);
    bot.pipes().next = Fault::StallStatus;
    assert_eq!(
        bot.command(0, &Cdb::test_unit_ready(), Data::None, 1000),
        Ok(Reply::Good { residue: 0 })
    );
    assert_eq!(bot.pipes().cleared, [true]);
    bot.pipes().next = Fault::EmptyStatus;
    assert_eq!(
        bot.command(0, &Cdb::test_unit_ready(), Data::None, 1000),
        Ok(Reply::Good { residue: 0 })
    );
    assert_eq!(bot.pipes().resets, 0);
}

#[test]
fn a_stick_that_breaks_the_protocol_is_reset() {
    for (fault, why) in [
        (Fault::WrongTag, "it sent a status that is not valid"),
        (Fault::PhaseError, "it reported a phase error"),
        (Fault::StallCommand, "it refused a command"),
        (Fault::Silent, "it did not answer in time"),
    ] {
        let mut bot = bot(64);
        bot.pipes().next = fault;
        let mut bytes = [0u8; 36];
        let reply = bot.command(0, &Cdb::inquiry(36), Data::In(&mut bytes), 1000);
        assert_eq!(reply, Err(TransportError::Failed(why)), "{fault:?}");
        let stick = bot.pipes();
        assert_eq!(stick.resets, 1, "{fault:?}");
        assert_eq!(stick.cleared, [true, false], "{fault:?}");
        assert_eq!(stick.halted, [false, false]);
        // It takes the next command.
        let reply = bot.command(0, &Cdb::test_unit_ready(), Data::None, 1000);
        assert_eq!(reply, Ok(Reply::Good { residue: 0 }), "{fault:?}");
    }
}

#[test]
fn a_stick_pulled_out_fails_its_commands_as_gone() {
    let mut gone = bot(64);
    gone.pipes().gone = true;
    let reply = gone.command(0, &Cdb::test_unit_ready(), Data::None, 1000);
    assert_eq!(reply, Err(TransportError::Gone));
    assert_eq!(gone.max_lun(), Err(PipeError::Gone));
    assert_eq!(gone.reset(), Err(TransportError::Gone));
    // Pulled out while a command waits: the reset after says so.
    let mut pulled = bot(64);
    pulled.pipes().next = Fault::Silent;
    pulled.pipes().pull_at_reset = true;
    let mut bytes = [0u8; 36];
    let reply = pulled.command(0, &Cdb::inquiry(36), Data::In(&mut bytes), 1000);
    assert_eq!(reply, Err(TransportError::Gone));
}

#[test]
fn get_max_lun_says_the_logical_units_or_none_past_the_first() {
    let mut bot = bot(64);
    assert_eq!(bot.max_lun(), Ok(0));
    bot.pipes().max_lun = Some(3);
    assert_eq!(bot.max_lun(), Ok(3));
    bot.pipes().max_lun = None;
    assert_eq!(bot.max_lun(), Ok(0));
    bot.pipes().max_lun = Some(200);
    assert_eq!(bot.max_lun(), Ok(0));
}

#[test]
fn a_status_signature_of_its_own_is_taken_if_the_stick_keeps_to_it() {
    let mut bot = bot(64);
    let odd = u32::from_le_bytes(*b"USBC");
    bot.pipes().signature = odd;
    let mut block = [0u8; 512];
    let read = Cdb::read(3, 1, false).unwrap();
    assert_eq!(bot.command(0, &read, Data::In(&mut block), 1000), Ok(Reply::Good { residue: 0 }));
    assert_eq!(block, [3; 512]);
    assert_eq!(bot.command(0, &read, Data::In(&mut block), 1000), Ok(Reply::Good { residue: 0 }));
    // Another after: broken, and reset.
    bot.pipes().next = Fault::Signature(CSW_SIGNATURE);
    assert_eq!(
        bot.command(0, &read, Data::In(&mut block), 1000),
        Err(TransportError::Failed("it sent a status that is not valid"))
    );
    assert_eq!(bot.pipes().resets, 1);
}

#[test]
fn a_stick_that_says_whole_data_did_not_move_is_not_believed_again() {
    let mut bot = bot(64);
    let mut block = [0u8; 512];
    let read = Cdb::read(3, 1, false).unwrap();
    // Believed, of a read.
    bot.pipes().next = Fault::Residue(512);
    assert_eq!(bot.command(0, &read, Data::In(&mut block), 1000), Ok(Reply::Good { residue: 512 }));
    // Said of an INQUIRY that came whole: not, then or after.
    bot.pipes().next = Fault::Residue(36);
    let mut inquiry = [0u8; 36];
    let cdb = Cdb::inquiry(36);
    assert_eq!(bot.command(0, &cdb, Data::In(&mut inquiry), 1000), Ok(Reply::Good { residue: 0 }));
    bot.pipes().next = Fault::Residue(512);
    assert_eq!(bot.command(0, &read, Data::In(&mut block), 1000), Ok(Reply::Good { residue: 0 }));
    // What the controller counts still counts.
    bot.pipes().next = Fault::StallData(100);
    assert_eq!(bot.command(0, &read, Data::In(&mut block), 1000), Ok(Reply::Good { residue: 412 }));
}

#[test]
fn the_probe_finds_the_disk_through_the_transport() {
    let mut bot = bot(2048);
    let found = probe(&mut bot, 0, &mut |_| panic!("no waiting")).unwrap();
    assert_eq!(found.capacity, Capacity { blocks: 2048, block_length: 512, physical_exponent: 0 });
    assert_eq!(found.inquiry.product(), "QEMU HARDDISK");
    assert!(found.inquiry.removable && !found.read_only);
    let sent: Vec<u8> = bot.pipes().cbws.iter().map(|cbw| cbw[15]).collect();
    assert_eq!(sent, [op::INQUIRY, op::TEST_UNIT_READY, op::READ_CAPACITY_10, op::MODE_SENSE_6]);
}

proptest! {
    #[test]
    fn any_status_is_read_without_panicking(bytes in proptest::collection::vec(any::<u8>(), 0..20), tag in any::<u32>()) {
        if let Some(csw) = Csw::parse(&bytes, tag, CSW_SIGNATURE) {
            prop_assert_eq!(bytes.len(), CSW_LENGTH);
            prop_assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), tag);
            prop_assert_eq!(csw.residue, u32::from_le_bytes(bytes[8..12].try_into().unwrap()));
        }
    }
}
