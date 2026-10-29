// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Commands as devices take them, and the probe against simulated disks
//! that are slow, empty, protected, huge or not disks at all.

use proptest::prelude::*;
use std::format;
use std::string::ToString;
use std::vec::Vec;

use crate::*;

/// A simulated logical unit, answering as a USB stick's firmware does.
struct Sim {
    qualifier: u8,
    device_type: u8,
    removable: bool,
    blocks: u64,
    block_length: u32,
    physical_exponent: u8,
    /// TEST UNIT READYs answered with "becoming ready" first.
    becoming: u32,
    /// Not ready until a START STOP UNIT.
    stopped: bool,
    no_medium: bool,
    protected: bool,
    /// UNIT ATTENTIONs to answer the next commands with, as after a reset.
    attentions: u32,
    /// Refuses MODE SENSE of every page; with `no_mode_sense`, of any.
    no_all_pages: bool,
    no_mode_sense: bool,
    /// Its caching page's WCE bit, if it has the page.
    cache: Option<bool>,
    /// Transport failures to give the next commands.
    broken: u32,
    gone: bool,
    /// The operation codes it was sent, in order.
    sent: Vec<u8>,
}

impl Sim {
    fn stick() -> Sim {
        Sim {
            qualifier: 0,
            device_type: device_type::DISK,
            removable: true,
            blocks: 0x0040_0000,
            block_length: 512,
            physical_exponent: 0,
            becoming: 0,
            stopped: false,
            no_medium: false,
            protected: false,
            attentions: 0,
            no_all_pages: false,
            no_mode_sense: false,
            cache: None,
            broken: 0,
            gone: false,
            sent: Vec::new(),
        }
    }

    fn sense(key: u8, asc: u8, ascq: u8) -> Reply {
        Reply::Failed(Some(Sense { key, asc, ascq }))
    }

    /// Answers `cdb`, with its data in `out`; returns the reply and how
    /// many bytes it sends.
    fn answer(&mut self, cdb: &[u8], out: &mut Vec<u8>) -> Reply {
        if self.attentions > 0 {
            self.attentions -= 1;
            return Sim::sense(key::UNIT_ATTENTION, 0x29, 0);
        }
        match cdb[0] {
            op::INQUIRY => {
                let mut bytes = std::vec![0u8; 36];
                bytes[0] = self.qualifier << 5 | self.device_type;
                bytes[1] = if self.removable { 0x80 } else { 0 };
                bytes[2] = 0x06;
                bytes[4] = 31;
                bytes[8..16].copy_from_slice(b"QEMU    ");
                bytes[16..32].copy_from_slice(b"QEMU HARDDISK   ");
                bytes[32..36].copy_from_slice(b"2.5+");
                *out = bytes;
                Reply::Good { residue: 0 }
            }
            op::TEST_UNIT_READY => {
                if self.no_medium {
                    Sim::sense(key::NOT_READY, 0x3a, 0)
                } else if self.stopped {
                    Sim::sense(key::NOT_READY, 0x04, 0x02)
                } else if self.becoming > 0 {
                    self.becoming -= 1;
                    Sim::sense(key::NOT_READY, 0x04, 0x01)
                } else {
                    Reply::Good { residue: 0 }
                }
            }
            op::START_STOP_UNIT => {
                if cdb[4] & 1 != 0 {
                    self.stopped = false;
                }
                Reply::Good { residue: 0 }
            }
            op::READ_CAPACITY_10 => {
                if self.no_medium {
                    return Sim::sense(key::NOT_READY, 0x3a, 0);
                }
                let last = u32::try_from(self.blocks - 1).unwrap_or(u32::MAX);
                out.extend(last.to_be_bytes());
                out.extend(self.block_length.to_be_bytes());
                Reply::Good { residue: 0 }
            }
            op::SERVICE_ACTION_IN_16 if cdb[1] == op::READ_CAPACITY_16 => {
                out.extend((self.blocks - 1).to_be_bytes());
                out.extend(self.block_length.to_be_bytes());
                out.extend([0, self.physical_exponent, 0, 0]);
                out.resize(32, 0);
                Reply::Good { residue: 0 }
            }
            op::MODE_SENSE_6 => {
                if self.no_mode_sense || (self.no_all_pages && cdb[2] & 0x3f == ALL_PAGES) {
                    return Sim::sense(key::ILLEGAL_REQUEST, 0x24, 0);
                }
                out.extend([3, 0, if self.protected { 0x80 } else { 0 }, 0]);
                if cdb[2] & 0x3f == ALL_PAGES {
                    // A block descriptor, the error recovery page, then
                    // the caching page if it has one.
                    out[3] = 8;
                    out.extend([0, 0, 0, 0, 0, 0, 2, 0]);
                    out.extend([0x01, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
                    if let Some(wce) = self.cache {
                        out.extend([CACHING_PAGE, 18, if wce { 0x04 } else { 0 }]);
                        out.extend([0; 17]);
                    }
                    out[0] = (out.len() - 1) as u8;
                }
                Reply::Good { residue: 0 }
            }
            _ => Sim::sense(key::ILLEGAL_REQUEST, 0x20, 0),
        }
    }
}

impl Transport for Sim {
    fn command(
        &mut self,
        _lun: u8,
        cdb: &Cdb,
        data: Data<'_>,
        _timeout_ms: u32,
    ) -> Result<Reply, TransportError> {
        if self.gone {
            return Err(TransportError::Gone);
        }
        self.sent.push(cdb.opcode());
        if self.broken > 0 {
            self.broken -= 1;
            return Err(TransportError::Failed("the device stopped answering"));
        }
        let mut out = Vec::new();
        let reply = self.answer(cdb.bytes(), &mut out);
        match (data, reply) {
            (Data::In(buffer), Reply::Good { .. }) => {
                let n = out.len().min(buffer.len());
                buffer[..n].copy_from_slice(&out[..n]);
                Ok(Reply::Good { residue: (buffer.len() - n) as u32 })
            }
            (_, reply) => Ok(reply),
        }
    }
}

fn probe_sim(sim: &mut Sim) -> (Result<Found, Absent>, u32) {
    let mut waited = 0;
    let found = probe(sim, 0, &mut |ms| waited += ms);
    (found, waited)
}

#[test]
fn a_stick_is_asked_what_it_is_whether_it_is_ready_how_big_and_whether_protected() {
    let mut sim = Sim::stick();
    let (found, waited) = probe_sim(&mut sim);
    let found = found.unwrap();
    assert_eq!(waited, 0);
    assert_eq!(
        sim.sent,
        [op::INQUIRY, op::TEST_UNIT_READY, op::READ_CAPACITY_10, op::MODE_SENSE_6]
    );
    assert_eq!(found.inquiry.vendor(), "QEMU");
    assert_eq!(found.inquiry.product(), "QEMU HARDDISK");
    assert_eq!(found.inquiry.revision(), "2.5+");
    assert!(found.inquiry.removable);
    assert_eq!(
        found.capacity,
        Capacity { blocks: 0x40_0000, block_length: 512, physical_exponent: 0 }
    );
    assert_eq!(found.capacity.bytes(), 2 << 30);
    assert!(!found.read_only && !found.sixteen && !found.write_cache);
}

#[test]
fn a_write_cache_is_what_the_caching_page_says_and_none_without_one() {
    for (cache, expected) in [(Some(true), true), (Some(false), false), (None, false)] {
        let mut sim = Sim { cache, ..Sim::stick() };
        assert_eq!(probe_sim(&mut sim).0.unwrap().write_cache, expected, "{cache:?}");
    }
    // Only page 0 answered: no caching page to read.
    let mut sim = Sim { cache: Some(true), no_all_pages: true, ..Sim::stick() };
    assert!(!probe_sim(&mut sim).0.unwrap().write_cache);
    // Cut short in the page, or a list ended by page 0.
    assert_eq!(write_cache(&[5, 0, 0, 0, 0x08, 18]), None);
    assert_eq!(write_cache(&[7, 0, 0, 0, 0x00, 2, 0x08, 0x04]), None);
    assert_eq!(write_cache(&[6, 0, 0, 0, 0x88, 1, 0x04]), Some(true));
    assert_eq!(write_cache(&[]), None);
}

#[test]
fn a_disk_becoming_ready_is_waited_for_and_one_stopped_is_started_once() {
    let mut sim = Sim { becoming: 4, ..Sim::stick() };
    let (found, waited) = probe_sim(&mut sim);
    assert!(found.is_ok());
    assert_eq!(waited, 4_000);
    let mut sim = Sim { stopped: true, ..Sim::stick() };
    let (found, waited) = probe_sim(&mut sim);
    assert!(found.is_ok());
    assert_eq!(waited, 1_000);
    assert_eq!(sim.sent.iter().filter(|&&op| op == op::START_STOP_UNIT).count(), 1);
    // One that never gets ready is given up on after 100 s.
    let mut sim = Sim { becoming: u32::MAX, ..Sim::stick() };
    let (found, waited) = probe_sim(&mut sim);
    assert_eq!(
        found,
        Err(Absent::Failed(
            "TEST UNIT READY",
            Some(Sense { key: key::NOT_READY, asc: 4, ascq: 1 })
        ))
    );
    assert_eq!(waited, 100_000);
}

#[test]
fn a_card_reader_with_no_card_has_no_medium() {
    let mut sim = Sim { no_medium: true, ..Sim::stick() };
    let (found, _) = probe_sim(&mut sim);
    assert_eq!(found, Err(Absent::NoMedium));
    assert_eq!(found.unwrap_err().to_string(), "it has no medium in it");
}

#[test]
fn unit_attentions_and_transport_resets_send_a_command_again() {
    let mut sim = Sim { attentions: 2, ..Sim::stick() };
    assert!(probe_sim(&mut sim).0.is_ok());
    assert_eq!(&sim.sent[..3], [op::INQUIRY; 3]);
    let mut sim = Sim { broken: 2, ..Sim::stick() };
    assert!(probe_sim(&mut sim).0.is_ok());
    // Three failures in a row end the probe.
    let mut sim = Sim { broken: 3, ..Sim::stick() };
    assert_eq!(probe_sim(&mut sim).0, Err(Absent::Transport("the device stopped answering")));
    let mut sim = Sim { attentions: 3, ..Sim::stick() };
    let sense = Sense { key: key::UNIT_ATTENTION, asc: 0x29, ascq: 0 };
    assert_eq!(probe_sim(&mut sim).0, Err(Absent::Failed("INQUIRY", Some(sense))));
    let mut sim = Sim { gone: true, ..Sim::stick() };
    assert_eq!(probe_sim(&mut sim).0, Err(Absent::Gone));
}

#[test]
fn a_disk_past_2_tib_is_read_with_16_byte_commands() {
    let mut sim =
        Sim { blocks: 0x1_2000_0000, block_length: 4096, physical_exponent: 0, ..Sim::stick() };
    let found = probe_sim(&mut sim).0.unwrap();
    assert!(sim.sent.contains(&op::SERVICE_ACTION_IN_16));
    assert_eq!(found.capacity.blocks, 0x1_2000_0000);
    assert!(found.sixteen);
    // 512-byte blocks in 4 KiB physical ones.
    let mut sim = Sim { blocks: 0x2_0000_0000, physical_exponent: 3, ..Sim::stick() };
    let found = probe_sim(&mut sim).0.unwrap();
    assert_eq!(found.capacity.physical_block(), 4096);
    // Exactly 2^32 blocks make READ CAPACITY(10)'s last block its marker
    // for more, and take 16-byte commands, as on Linux.
    let mut sim = Sim { blocks: 1 << 32, ..Sim::stick() };
    let found = probe_sim(&mut sim).0.unwrap();
    assert!(sim.sent.contains(&op::SERVICE_ACTION_IN_16));
    assert_eq!(found.capacity.blocks, 1 << 32);
    assert!(found.sixteen);
}

#[test]
fn write_protection_is_read_from_every_page_then_page_zero_else_writable() {
    let mut sim = Sim { protected: true, ..Sim::stick() };
    assert!(probe_sim(&mut sim).0.unwrap().read_only);
    let mut sim = Sim { protected: true, no_all_pages: true, ..Sim::stick() };
    assert!(probe_sim(&mut sim).0.unwrap().read_only);
    assert_eq!(sim.sent.iter().filter(|&&op| op == op::MODE_SENSE_6).count(), 2);
    let mut sim = Sim { protected: true, no_mode_sense: true, ..Sim::stick() };
    assert!(!probe_sim(&mut sim).0.unwrap().read_only);
}

#[test]
fn what_is_not_a_disk_is_left_alone() {
    let mut sim = Sim { device_type: device_type::CD, ..Sim::stick() };
    let found = probe_sim(&mut sim).0;
    assert_eq!(found, Err(Absent::NotDisk(device_type::CD)));
    assert_eq!(found.unwrap_err().to_string(), "it is a CD or DVD drive");
    let mut sim = Sim { qualifier: 3, device_type: 0x1f, ..Sim::stick() };
    assert_eq!(probe_sim(&mut sim).0, Err(Absent::NoUnit));
    assert_eq!(sim.sent, [op::INQUIRY]);
    let mut sim = Sim { block_length: 520, ..Sim::stick() };
    let found = probe_sim(&mut sim).0;
    assert!(matches!(found, Err(Absent::BadCapacity(_))));
    assert_eq!(found.unwrap_err().to_string(), "it says it has 4194304 blocks of 520 bytes");
}

#[test]
fn reads_and_writes_take_ten_byte_commands_unless_they_need_sixteen() {
    let read = Cdb::read(0x1234_5678, 240, false).unwrap();
    assert_eq!(read.bytes(), [op::READ_10, 0, 0x12, 0x34, 0x56, 0x78, 0, 0, 240, 0]);
    let write = Cdb::write(7, 0xffff, false).unwrap();
    assert_eq!(write.bytes(), [op::WRITE_10, 0, 0, 0, 0, 7, 0, 0xff, 0xff, 0]);
    let far = Cdb::read(0x1_0000_0000, 8, false).unwrap();
    assert_eq!(far.bytes(), [op::READ_16, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0]);
    let long = Cdb::write(0, 0x1_0000, false).unwrap();
    assert_eq!(long.opcode(), op::WRITE_16);
    assert_eq!(&long.bytes()[10..14], [0, 1, 0, 0]);
    assert_eq!(Cdb::read(0, 1, true).unwrap().opcode(), op::READ_16);
    assert_eq!(Cdb::read(0, 0, false), None);
    assert_eq!(
        Cdb::synchronize_cache().bytes(),
        [op::SYNCHRONIZE_CACHE_10, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(Cdb::start_stop_unit(false, true).bytes(), [op::START_STOP_UNIT, 0, 0, 0, 2, 0]);
    assert_eq!(Cdb::read_capacity_16(32).bytes()[..2], [op::SERVICE_ACTION_IN_16, 0x10]);
    assert_eq!(Cdb::read_capacity_16(32).bytes()[13], 32);
    assert_eq!(Cdb::mode_sense_6(ALL_PAGES, 192).bytes(), [op::MODE_SENSE_6, 0, 0x3f, 0, 192, 0]);
}

#[test]
fn sense_comes_in_either_format_and_says_what_it_means() {
    let mut fixed = [0u8; 18];
    fixed[0] = 0x70;
    fixed[2] = key::NOT_READY;
    fixed[7] = 10;
    fixed[12] = 0x3a;
    let sense = Sense::parse(&fixed).unwrap();
    assert_eq!(sense, Sense { key: key::NOT_READY, asc: 0x3a, ascq: 0 });
    assert_eq!(sense.meaning(), Meaning::NoMedium);
    assert_eq!(sense.to_string(), "not ready, medium not present (3a/00)");
    let descriptor = [0x72, key::MEDIUM_ERROR, 0x11, 0x00, 0, 0, 0, 0];
    let sense = Sense::parse(&descriptor).unwrap();
    assert_eq!(sense.meaning(), Meaning::Error);
    assert_eq!(sense.to_string(), "medium error, unrecovered read error (11/00)");
    // Short fixed sense has the key alone.
    assert_eq!(
        Sense::parse(&[0xf0, 0, key::UNIT_ATTENTION]).unwrap().meaning(),
        Meaning::Attention
    );
    assert_eq!(Sense::parse(&[0x70, 0]), None);
    assert_eq!(Sense::parse(&[0x7f, 0, 0]), None);
    assert_eq!(Sense::parse(&[]), None);
    let cases = [
        ((key::NO_SENSE, 0, 0), Meaning::Fine),
        ((key::RECOVERED_ERROR, 0x17, 1), Meaning::Fine),
        ((key::NOT_READY, 4, 1), Meaning::BecomingReady),
        ((key::NOT_READY, 4, 2), Meaning::NeedsStart),
        ((key::NOT_READY, 4, 3), Meaning::Error),
        ((key::ILLEGAL_REQUEST, 0x20, 0), Meaning::Unsupported),
        ((key::DATA_PROTECT, 0x27, 0), Meaning::WriteProtected),
        ((key::ABORTED_COMMAND, 0x47, 0), Meaning::Aborted),
        ((key::HARDWARE_ERROR, 0x44, 0), Meaning::Error),
    ];
    for ((key, asc, ascq), meaning) in cases {
        assert_eq!(Sense { key, asc, ascq }.meaning(), meaning, "{key:x}/{asc:x}/{ascq:x}");
    }
    let unknown = Sense { key: key::ABORTED_COMMAND, asc: 0x4b, ascq: 2 };
    assert_eq!(unknown.to_string(), "aborted command (4b/02)");
}

#[test]
fn short_and_strange_answers_are_read_safely() {
    let short = Inquiry::parse(&[0x00, 0x80, 0x05]).unwrap();
    assert!(short.present() && short.is_disk() && short.removable);
    assert_eq!(short.vendor(), "");
    assert_eq!(Inquiry::parse(&[0]), None);
    assert_eq!(text(b"Kingston\xff"), "");
    assert_eq!(text(b"  SanDisk \0\0"), "SanDisk");
    assert_eq!(Capacity::parse_10(&[0; 7]), None);
    assert_eq!(Capacity::parse_16(&[0xff; 12]), None);
    let short16 = Capacity::parse_16(&[0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 2, 0]).unwrap();
    assert_eq!(short16, Capacity { blocks: 10, block_length: 512, physical_exponent: 0 });
    // A physical block past 64 KiB is held to it.
    let huge = Capacity { blocks: 1, block_length: 4096, physical_exponent: 15 };
    assert_eq!(huge.physical_block(), 65536);
    assert_eq!(write_protected(&[3, 0]), None);
    assert_eq!(format!("{}", Absent::Failed("INQUIRY", None)), "it failed INQUIRY");
}

proptest! {
    #[test]
    fn any_answer_is_read_without_panicking(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        if let Some(inquiry) = Inquiry::parse(&bytes) {
            for text in [inquiry.vendor(), inquiry.product(), inquiry.revision()] {
                prop_assert!(text.bytes().all(|b| (0x20..0x7f).contains(&b)));
            }
        }
        let _ = Capacity::parse_10(&bytes).map(|c| (c.usable(), c.physical_block(), c.bytes()));
        let _ = Capacity::parse_16(&bytes).map(|c| (c.usable(), c.physical_block(), c.bytes()));
        if let Some(sense) = Sense::parse(&bytes) {
            let _ = (sense.meaning(), sense.to_string());
        }
        let _ = write_protected(&bytes);
        let _ = write_cache(&bytes);
    }

    #[test]
    fn a_transfer_command_says_its_block_and_count(lba in any::<u64>(), blocks in 1u32.., sixteen in any::<bool>()) {
        let cdb = Cdb::read(lba, blocks, sixteen).unwrap();
        let bytes = cdb.bytes();
        if cdb.opcode() == op::READ_10 {
            prop_assert!(!sixteen);
            prop_assert_eq!(u64::from(u32::from_be_bytes(bytes[2..6].try_into().unwrap())), lba);
            prop_assert_eq!(u32::from(u16::from_be_bytes(bytes[7..9].try_into().unwrap())), blocks);
        } else {
            prop_assert_eq!(cdb.opcode(), op::READ_16);
            prop_assert_eq!(u64::from_be_bytes(bytes[2..10].try_into().unwrap()), lba);
            prop_assert_eq!(u32::from_be_bytes(bytes[10..14].try_into().unwrap()), blocks);
        }
    }
}
