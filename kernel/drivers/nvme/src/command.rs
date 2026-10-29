// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Commands and their completions (NVM Express Base Specification 2.0,
//! sections 4.2 and 4.6), what a completion's status says, and the lists
//! a command's data are in: of pages (PRP lists, 4.1.1), or of runs of
//! memory (scatter gather lists, 4.1.2).

/// The size of a submission queue entry, a command.
pub const COMMAND_SIZE: usize = 64;
/// The size of a completion queue entry.
pub const COMPLETION_SIZE: usize = 16;
/// The memory page size the driver sets (CC.MPS = 0): every PRP entry
/// but the first starts one.
pub const PAGE: u64 = 4096;
/// The most entries one command's PRP list holds: a kibibyte of them.
pub const LIST_ENTRIES: usize = 128;
/// The most descriptors one command's scatter gather list holds, in the
/// same kibibyte.
pub const SGL_ENTRIES: usize = LIST_ENTRIES * 8 / 16;
/// The average length of the runs of memory a command's data are in from
/// which they go as a scatter gather list rather than PRPs, as Linux's
/// `sgl_threshold` has it: shorter runs take no fewer entries.
pub const SGL_FROM: u64 = 32 * 1024;
/// SGL descriptor types (4.1.2.1), in the high half of a descriptor's
/// last byte, whose low half (0, an address) is the same for both.
const DATA_BLOCK: u64 = 0x0;
const LAST_SEGMENT: u64 = 0x3;
/// Command dword 0's PSDT field (bits 15:14) for data in a scatter gather
/// list (and metadata, which no command here has, in one buffer).
const PSDT_SGL: u32 = 1 << 14;

/// The namespace ID that names all of them.
pub const ALL_NAMESPACES: u32 = u32::MAX;
/// The SMART / Health Information log (5.16.1.3): the controller's
/// temperature, spare capacity, wear and error counts, 512 bytes.
pub const LOG_HEALTH: u8 = 0x02;
/// The bytes of [`LOG_HEALTH`].
pub const LOG_HEALTH_SIZE: usize = 512;

/// Admin command opcodes (5).
pub mod admin {
    /// Delete I/O Submission Queue.
    pub const DELETE_SQ: u8 = 0x00;
    /// Create I/O Submission Queue.
    pub const CREATE_SQ: u8 = 0x01;
    /// Get Log Page.
    pub const GET_LOG_PAGE: u8 = 0x02;
    /// Delete I/O Completion Queue.
    pub const DELETE_CQ: u8 = 0x04;
    /// Create I/O Completion Queue.
    pub const CREATE_CQ: u8 = 0x05;
    /// Identify: what the controller or a namespace is.
    pub const IDENTIFY: u8 = 0x06;
    /// Set Features.
    pub const SET_FEATURES: u8 = 0x09;
    /// Doorbell Buffer Config: shadow doorbells.
    pub const DOORBELL_BUFFER_CONFIG: u8 = 0x7c;
}

/// NVM command set opcodes (NVM Command Set Specification 1.0, 3).
pub mod nvm {
    /// Flush: makes completed writes durable.
    pub const FLUSH: u8 = 0x00;
    /// Write.
    pub const WRITE: u8 = 0x01;
    /// Read.
    pub const READ: u8 = 0x02;
    /// Dataset Management: here, deallocating ranges of blocks.
    pub const DATASET_MANAGEMENT: u8 = 0x09;
}

/// What Identify returns (the CNS field, 5.17).
pub mod cns {
    /// The Identify Namespace data structure of one namespace.
    pub const NAMESPACE: u8 = 0x00;
    /// The Identify Controller data structure.
    pub const CONTROLLER: u8 = 0x01;
    /// The active namespace IDs, in order, from the one after the NSID.
    pub const ACTIVE_NAMESPACES: u8 = 0x02;
    /// The I/O command set's own Identify Controller data structure: for
    /// the NVM command set, its limits on Dataset Management.
    pub const NVM_CONTROLLER: u8 = 0x06;
}

/// The Number of Queues feature (5.27.1.5).
const FEATURE_QUEUES: u32 = 0x07;
/// A Write's or Read's Force Unit Access bit, in command dword 12.
const FUA: u32 = 1 << 30;
/// Dataset Management's Deallocate attribute, in command dword 11.
const DEALLOCATE: u32 = 1 << 2;
/// Create I/O Completion Queue: physically contiguous, interrupts on.
const QUEUE_CONTIGUOUS: u32 = 1 << 0;
const QUEUE_INTERRUPTS: u32 = 1 << 1;

/// A command, as the driver fills one in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Command {
    /// What it does.
    pub opcode: u8,
    /// Its identifier, which its completion carries back.
    pub id: u16,
    /// The namespace it is for (NSID), 0 for none.
    pub namespace: u32,
    /// Whether the data pointer is a scatter gather list's first
    /// descriptor ([`sgl`]) rather than PRPs.
    pub sgl: bool,
    /// The first data pointer: the first page of the data, at an offset;
    /// or a descriptor's address.
    pub prp1: u64,
    /// The second: the second page, or the PRP list of the others; or a
    /// descriptor's length and type.
    pub prp2: u64,
    /// Command dwords 10 to 15.
    pub dwords: [u32; 6],
}

impl Command {
    /// The 64 bytes the controller reads.
    pub fn bytes(&self) -> [u8; COMMAND_SIZE] {
        let mut out = [0u8; COMMAND_SIZE];
        let psdt = if self.sgl { PSDT_SGL } else { 0 };
        let dword0 = u32::from(self.opcode) | psdt | (u32::from(self.id) << 16);
        out[0..4].copy_from_slice(&dword0.to_le_bytes());
        out[4..8].copy_from_slice(&self.namespace.to_le_bytes());
        out[24..32].copy_from_slice(&self.prp1.to_le_bytes());
        out[32..40].copy_from_slice(&self.prp2.to_le_bytes());
        for (i, dword) in self.dwords.iter().enumerate() {
            out[40 + 4 * i..44 + 4 * i].copy_from_slice(&dword.to_le_bytes());
        }
        out
    }

    /// Identify: `cns` of `namespace` into the page at `buffer`.
    pub fn identify(cns: u8, namespace: u32, buffer: u64) -> Command {
        let mut dwords = [0; 6];
        dwords[0] = u32::from(cns);
        Command { opcode: admin::IDENTIFY, namespace, prp1: buffer, dwords, ..Command::default() }
    }

    /// Get Log Page: the first `bytes` bytes (a multiple of 4, at most a
    /// page) of log `log` for every namespace, into the page at `buffer`.
    pub fn log_page(log: u8, bytes: u32, buffer: u64) -> Command {
        let mut dwords = [0; 6];
        // The count of dwords less one; its low 16 bits.
        dwords[0] = u32::from(log) | ((bytes / 4 - 1) << 16);
        Command {
            opcode: admin::GET_LOG_PAGE,
            namespace: ALL_NAMESPACES,
            prp1: buffer,
            dwords,
            ..Command::default()
        }
    }

    /// Set Features, Number of Queues: asks for `count` submission and as
    /// many completion queues.
    pub fn set_queues(count: u16) -> Command {
        let wanted = u32::from(count.max(1) - 1);
        let mut dwords = [0; 6];
        dwords[0] = FEATURE_QUEUES;
        dwords[1] = wanted | (wanted << 16);
        Command { opcode: admin::SET_FEATURES, dwords, ..Command::default() }
    }

    /// Create I/O Completion Queue `queue` of `entries` entries at
    /// `memory`, interrupting with MSI-X entry `vector`, if any.
    pub fn create_cq(queue: u16, entries: u16, memory: u64, vector: Option<u16>) -> Command {
        let mut dwords = [0; 6];
        dwords[0] = u32::from(queue) | (u32::from(entries - 1) << 16);
        dwords[1] = match vector {
            Some(vector) => QUEUE_CONTIGUOUS | QUEUE_INTERRUPTS | (u32::from(vector) << 16),
            None => QUEUE_CONTIGUOUS,
        };
        Command { opcode: admin::CREATE_CQ, prp1: memory, dwords, ..Command::default() }
    }

    /// Create I/O Submission Queue `queue` of `entries` entries at
    /// `memory`, completing into completion queue `queue`.
    pub fn create_sq(queue: u16, entries: u16, memory: u64) -> Command {
        let mut dwords = [0; 6];
        dwords[0] = u32::from(queue) | (u32::from(entries - 1) << 16);
        dwords[1] = QUEUE_CONTIGUOUS | (u32::from(queue) << 16);
        Command { opcode: admin::CREATE_SQ, prp1: memory, dwords, ..Command::default() }
    }

    /// Delete I/O Submission Queue `queue`, or its completion queue.
    pub fn delete(queue: u16, completion: bool) -> Command {
        let mut dwords = [0; 6];
        dwords[0] = u32::from(queue);
        let opcode = if completion { admin::DELETE_CQ } else { admin::DELETE_SQ };
        Command { opcode, dwords, ..Command::default() }
    }

    /// Doorbell Buffer Config: shadow doorbells in the page at
    /// `doorbells`, and event indexes in the page at `events`.
    pub fn doorbell_buffer(doorbells: u64, events: u64) -> Command {
        Command {
            opcode: admin::DOORBELL_BUFFER_CONFIG,
            prp1: doorbells,
            prp2: events,
            ..Command::default()
        }
    }

    /// Read or Write of `blocks` blocks (1 to 65,536) from `block` of
    /// `namespace`, with Force Unit Access if `fua`.
    pub fn transfer(write: bool, fua: bool, namespace: u32, block: u64, blocks: u32) -> Command {
        let mut dwords = [0; 6];
        dwords[0] = block as u32;
        dwords[1] = (block >> 32) as u32;
        dwords[2] = (blocks.clamp(1, 65_536) - 1) | if fua { FUA } else { 0 };
        let opcode = if write { nvm::WRITE } else { nvm::READ };
        Command { opcode, namespace, dwords, ..Command::default() }
    }

    /// Flush of `namespace`'s volatile write cache.
    pub fn flush(namespace: u32) -> Command {
        Command { opcode: nvm::FLUSH, namespace, ..Command::default() }
    }

    /// Dataset Management, Deallocate, of the one range at `range`.
    pub fn deallocate(namespace: u32, range: u64) -> Command {
        let mut dwords = [0; 6];
        dwords[1] = DEALLOCATE;
        Command {
            opcode: nvm::DATASET_MANAGEMENT,
            namespace,
            prp1: range,
            dwords,
            ..Command::default()
        }
    }
}

/// A Dataset Management range: `blocks` blocks from `block`.
pub fn range(block: u64, blocks: u32) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[4..8].copy_from_slice(&blocks.to_le_bytes());
    out[8..16].copy_from_slice(&block.to_le_bytes());
    out
}

/// A completion queue entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    /// The command's own result (dword 0).
    pub result: u32,
    /// How far the controller has read the submission queue.
    pub sq_head: u16,
    /// The command's identifier.
    pub id: u16,
    /// The phase tag: flips each time the controller wraps the queue, so
    /// an entry it has written is told from an old one.
    pub phase: bool,
    /// How it ended.
    pub status: Status,
}

impl Completion {
    /// Reads an entry's 16 bytes.
    pub fn parse(bytes: &[u8; COMPLETION_SIZE]) -> Completion {
        let (dwords, _) = bytes.as_chunks::<4>();
        Completion::from_dwords(core::array::from_fn(|i| u32::from_le_bytes(dwords[i])))
    }

    /// Reads an entry's four dwords.
    pub fn from_dwords(dwords: [u32; 4]) -> Completion {
        let last = dwords[3];
        Completion {
            result: dwords[0],
            sq_head: dwords[2] as u16,
            id: last as u16,
            phase: last & (1 << 16) != 0,
            status: Status((last >> 17) as u16),
        }
    }
}

/// A command's status: its type and code, and whether it may be tried
/// again (4.6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(pub u16);

/// Status code types.
const GENERIC: u8 = 0;
const MEDIA: u8 = 2;

impl Status {
    /// The status of a command that worked.
    pub const SUCCESS: Status = Status(0);

    /// Its type (SCT): 0 generic, 1 command specific, 2 media and data
    /// integrity, 3 path related.
    pub fn kind(self) -> u8 {
        ((self.0 >> 8) & 0x7) as u8
    }

    /// Its code within its type (SC).
    pub fn code(self) -> u8 {
        self.0 as u8
    }

    /// Whether it worked.
    pub fn worked(self) -> bool {
        self.kind() == GENERIC && self.code() == 0
    }

    /// Do Not Retry: the command would fail again.
    pub fn do_not_retry(self) -> bool {
        self.0 & (1 << 14) != 0
    }

    /// Whether the command fails for good: it says not to retry it. The
    /// controller says so of what would fail again, an invalid field or
    /// a sector it cannot read; any other failure may work when sent
    /// again (an abort, a namespace not ready yet, a read the medium got
    /// wrong once), as Linux has it too.
    pub fn fails(self) -> bool {
        !self.worked() && self.do_not_retry()
    }

    /// What it says, for the log.
    pub fn describe(self) -> &'static str {
        match (self.kind(), self.code()) {
            (GENERIC, 0x00) => "success",
            (GENERIC, 0x01) => "invalid command opcode",
            (GENERIC, 0x02) => "invalid field in command",
            (GENERIC, 0x04) => "data transfer error",
            (GENERIC, 0x05) => "aborted by a power loss",
            (GENERIC, 0x06) => "internal error",
            (GENERIC, 0x07) => "aborted on request",
            (GENERIC, 0x08) => "aborted: its queue was deleted",
            (GENERIC, 0x0b) => "invalid namespace",
            (GENERIC, 0x80) => "block out of range",
            (GENERIC, 0x81) => "capacity exceeded",
            (GENERIC, 0x82) => "namespace not ready",
            (1, _) => "command specific error",
            (MEDIA, 0x80) => "write fault",
            (MEDIA, 0x81) => "unrecovered read error",
            (MEDIA, 0x86) => "access denied",
            (MEDIA, _) => "media error",
            (3, _) => "path error",
            _ => "error",
        }
    }
}

/// Why a command's buffers cannot go in a PRP list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap;

/// The data pointers of a command whose data are in `data` (physical
/// address, length), each inside one page: PRP1, and PRP2, the second
/// page, or with more than two the address of the PRP list, `list_phys`,
/// whose entries are then every buffer's after the first, in order.
/// Every buffer but the first must start a page, and every one but the
/// last end one, so that together they are one run of pages, the first
/// entered at an offset; at most [`LIST_ENTRIES`] + 1 of them.
pub fn prps(data: &[(u64, u32)], list_phys: u64) -> Result<(u64, u64), Gap> {
    let Some((&(first, first_len), rest)) = data.split_first() else { return Err(Gap) };
    if rest.len() > LIST_ENTRIES || first_len == 0 || !first.is_multiple_of(4) {
        return Err(Gap);
    }
    let in_page = |phys: u64, len: u32| (phys % PAGE) + u64::from(len) <= PAGE;
    if !in_page(first, first_len) {
        return Err(Gap);
    }
    for (i, &(phys, len)) in data.iter().enumerate() {
        let last = i + 1 == data.len();
        let starts = i == 0 || phys.is_multiple_of(PAGE);
        let ends = last || phys % PAGE + u64::from(len) == PAGE;
        if len == 0 || !in_page(phys, len) || !starts || !ends {
            return Err(Gap);
        }
    }
    let prp2 = match rest {
        [] => 0,
        [(second, _)] => *second,
        _ => list_phys,
    };
    Ok((first, prp2))
}

/// The runs of memory the buffers in `data` make: each buffer merged with
/// those that follow it in memory, (address, length).
fn runs(data: &[(u64, u32)]) -> impl Iterator<Item = (u64, u64)> + '_ {
    let mut rest = data;
    core::iter::from_fn(move || {
        let (&(start, len), more) = rest.split_first()?;
        let mut end = start.wrapping_add(u64::from(len));
        rest = more;
        while let Some((&(phys, len), more)) = rest.split_first()
            && phys == end
        {
            end = end.wrapping_add(u64::from(len));
            rest = more;
        }
        Some((start, end.wrapping_sub(start)))
    })
}

/// The data pointer of a command whose data are in `data` (physical
/// address, length) as a scatter gather list, for a controller that takes
/// them: a data block descriptor for each run of memory the buffers make,
/// which `put` writes (the descriptor's place and its two words) to the
/// list at `list_phys` if there is more than one. Returns the two words of
/// the descriptor the command holds: the one data block, or the last
/// segment descriptor of the list. `None`, for PRPs, if a buffer is empty
/// or not dword aligned, or the runs are more than [`SGL_ENTRIES`] or
/// shorter than [`SGL_FROM`] on average, as Linux decides.
pub fn sgl(
    data: &[(u64, u32)],
    list_phys: u64,
    mut put: impl FnMut(usize, u64, u64),
) -> Option<(u64, u64)> {
    let fits = |&(phys, len): &(u64, u32)| {
        len > 0
            && phys.is_multiple_of(4)
            && len.is_multiple_of(4)
            && phys.checked_add(len.into()).is_some()
    };
    let bytes = data
        .iter()
        .try_fold(0u64, |sum, buffer| fits(buffer).then(|| sum + u64::from(buffer.1)))?;
    let (count, longest) =
        runs(data).fold((0usize, 0), |(count, longest), (_, len)| (count + 1, longest.max(len)));
    if count == 0
        || count > SGL_ENTRIES
        || longest > u64::from(u32::MAX)
        || bytes.div_ceil(count as u64) < SGL_FROM
    {
        return None;
    }
    let descriptor = |len: u64, kind: u64| len | kind << 60;
    if count == 1 {
        let (start, len) = runs(data).next()?;
        return Some((start, descriptor(len, DATA_BLOCK)));
    }
    for (i, (start, len)) in runs(data).enumerate() {
        put(i, start, descriptor(len, DATA_BLOCK));
    }
    Some((list_phys, descriptor(16 * count as u64, LAST_SEGMENT)))
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// A run of buffers [`prps`] accepts is one the controller reads as
    /// the same bytes: PRP1 is the first buffer, every buffer after it
    /// starts a page, every one before the last ends one, and PRP2 is the
    /// second buffer or the list of them.
    #[kani::proof]
    #[kani::unwind(5)]
    fn prps_describe_the_buffers() {
        let data: [(u64, u32); 3] = kani::any();
        let count: usize = kani::any_where(|&c: &usize| c <= 3);
        let list_phys: u64 = kani::any();
        if let Ok((prp1, prp2)) = prps(&data[..count], list_phys) {
            assert!(count >= 1);
            assert_eq!(prp1, data[0].0);
            for &(phys, len) in &data[1..count] {
                assert!(phys.is_multiple_of(PAGE));
                assert!(len > 0 && u64::from(len) <= PAGE);
            }
            match count {
                1 => assert_eq!(prp2, 0),
                2 => assert_eq!(prp2, data[1].0),
                _ => assert_eq!(prp2, list_phys),
            }
            for &(phys, len) in &data[..count - 1] {
                assert_eq!(phys % PAGE + u64::from(len), PAGE);
            }
        }
    }

    /// What [`sgl`] makes covers the buffers' bytes, in order: one data
    /// block of them all, or a last segment descriptor of a list of data
    /// blocks, each of whole buffers that follow each other in memory.
    #[kani::proof]
    #[kani::unwind(5)]
    fn sgls_describe_the_buffers() {
        let data: [(u64, u32); 3] = kani::any();
        let count: usize = kani::any_where(|&c: &usize| c <= 3);
        let list_phys: u64 = kani::any();
        let mut list = [(0u64, 0u64); 3];
        let mut puts = 0;
        let found = sgl(&data[..count], list_phys, |i, address, rest| {
            assert!(i == puts && i < 3);
            list[i] = (address, rest);
            puts += 1;
        });
        if let Some((first, second)) = found {
            let single = [(first, second)];
            let blocks = if puts == 0 {
                &single[..]
            } else {
                assert_eq!(first, list_phys);
                assert_eq!(second, 16 * puts as u64 | LAST_SEGMENT << 60);
                &list[..puts]
            };
            let (mut at, mut sum) = (0, 0u64);
            for &(address, rest) in blocks {
                assert_eq!(rest >> 32, DATA_BLOCK);
                let len = rest & 0xffff_ffff;
                let mut covered = 0u64;
                while covered < len {
                    assert!(at < count);
                    assert_eq!(data[at].0, address.wrapping_add(covered));
                    covered += u64::from(data[at].1);
                    at += 1;
                }
                assert_eq!(covered, len);
                sum += len;
            }
            assert_eq!(at, count);
            assert_eq!(sum, data[..count].iter().map(|&(_, len)| u64::from(len)).sum::<u64>());
        }
    }

    /// A completion's fields come back as they were written.
    #[kani::proof]
    fn completions_read_back() {
        let bytes: [u8; COMPLETION_SIZE] = kani::any();
        let found = Completion::parse(&bytes);
        assert_eq!(found.id, u16::from_le_bytes([bytes[12], bytes[13]]));
        assert_eq!(found.phase, bytes[14] & 1 != 0);
        assert!(!found.status.worked() || !found.status.fails());
    }
}
